//! Recovery gates: bounded retries, no-progress detection, and replanning
//! that can never escape the frozen acceptance contract.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use lfm3_harness::{
    agent::{
        packets::{
            canonical_action, evidence_refs, failure_signature, normalize_text, FailurePacket,
        },
        BoxFuture, Budget, FailureClass, FailureRecord, RecoveryController, RecoveryDecision,
        RecoveryError, RemainingBudget, StopReason,
    },
    verify::{CheckEvidence, CheckResult, CheckStatus, ContentDigest},
    AcceptanceSpec, AttemptStatus, CancellationChild, CancellationContext, CheckSpec, DriverError,
    ExecutionStrategy, FrozenAcceptance, IncompleteReason, OutputContract, PlanError, PlannedStep,
    RunStatus, Runner, StepDriver, StepExecution, StepRequest, Store,
};
use serde_json::{json, Value};
use tempfile::TempDir;

// ------------------------------------------------------- recovery fixtures

#[test]
fn replanning_does_not_reset_total_attempt_budget() {
    let mut recovery = RecoveryController::new(Budget::fixture(3, 2, 6));
    recovery.record(FailureRecord::fixture(1));
    recovery.record(FailureRecord::fixture(2));
    recovery.record(FailureRecord::fixture(3));
    assert_eq!(
        recovery.next(FailureRecord::fixture(4)),
        RecoveryDecision::Replan
    );
    recovery.record_replan();
    recovery.record(FailureRecord::fixture(5));
    recovery.record(FailureRecord::fixture(6));
    assert!(matches!(
        recovery.next(FailureRecord::fixture(7)),
        RecoveryDecision::Incomplete { .. }
    ));
}

#[test]
fn duplicate_actions_are_not_retried_in_place() {
    let mut recovery = RecoveryController::new(Budget::fixture(5, 2, 10));
    recovery.record(FailureRecord::new(
        1,
        FailureClass::CheckFailed,
        "same-action",
        "sig-1",
    ));
    assert_eq!(
        recovery.next(FailureRecord::new(
            1,
            FailureClass::CheckFailed,
            "same-action",
            "sig-2",
        )),
        RecoveryDecision::Replan
    );
    recovery.record(FailureRecord::new(
        1,
        FailureClass::CheckFailed,
        "same-action",
        "sig-2",
    ));
    assert!(recovery.record_replan());
    assert_eq!(
        recovery.next(FailureRecord::new(
            1,
            FailureClass::CheckFailed,
            "different-action",
            "sig-3",
        )),
        RecoveryDecision::Retry
    );
}

#[test]
fn alternating_action_cycles_stop_without_a_strategy_budget() {
    let mut recovery = RecoveryController::new(Budget::fixture(5, 1, 10));
    let failure = |action: &str, signature: &str| {
        FailureRecord::new(1, FailureClass::CheckFailed, action, signature)
    };
    recovery.record(failure("a", "sig-a"));
    recovery.record(failure("b", "sig-b"));
    recovery.record(failure("a", "sig-a"));
    assert_eq!(
        recovery.next(failure("b", "sig-b")),
        RecoveryDecision::Incomplete {
            reason: StopReason::NoProgress
        }
    );
}

#[test]
fn the_same_failure_with_new_actions_still_needs_a_new_strategy() {
    let mut recovery = RecoveryController::new(Budget::fixture(5, 2, 10));
    recovery.record(FailureRecord::new(
        1,
        FailureClass::CheckFailed,
        "action-1",
        "same-signature",
    ));
    recovery.record(FailureRecord::new(
        1,
        FailureClass::CheckFailed,
        "action-2",
        "same-signature",
    ));
    assert_eq!(
        recovery.next(FailureRecord::new(
            1,
            FailureClass::CheckFailed,
            "action-3",
            "same-signature",
        )),
        RecoveryDecision::Replan
    );
}

#[test]
fn transient_failures_back_off_and_the_delay_is_bounded() {
    let mut recovery = RecoveryController::new(Budget::fixture(3, 2, 6));
    recovery.record(FailureRecord::new(
        1,
        FailureClass::Transient,
        "driver:failed",
        "transient|no-evidence",
    ));
    assert_eq!(recovery.backoff_delay(), Duration::from_millis(100));
    assert_eq!(
        recovery.next(FailureRecord::new(
            1,
            FailureClass::Transient,
            "driver:failed",
            "transient|no-evidence",
        )),
        RecoveryDecision::Backoff
    );
    recovery.record(FailureRecord::new(
        1,
        FailureClass::Transient,
        "driver:failed",
        "transient|no-evidence",
    ));
    assert_eq!(recovery.backoff_delay(), Duration::from_millis(200));
    recovery.record(FailureRecord::new(
        1,
        FailureClass::Transient,
        "driver:failed",
        "transient|no-evidence",
    ));
    assert_eq!(recovery.backoff_delay(), Duration::from_millis(400));
    // The fourth identical transient exceeds the per-strategy budget.
    assert_eq!(
        recovery.next(FailureRecord::new(
            1,
            FailureClass::Transient,
            "driver:failed",
            "transient|no-evidence",
        )),
        RecoveryDecision::Incomplete {
            reason: StopReason::NoProgress
        }
    );
}

#[test]
fn backoff_is_capped_for_a_long_transient_streak() {
    let mut recovery = RecoveryController::new(Budget::fixture(10, 2, 20));
    for _ in 0..8 {
        recovery.record(FailureRecord::new(
            1,
            FailureClass::Transient,
            "driver:failed",
            "transient|no-evidence",
        ));
    }
    assert_eq!(recovery.backoff_delay(), Duration::from_secs(2));
}

#[test]
fn inconclusive_failures_require_review_and_never_pass() {
    let mut recovery = RecoveryController::new(Budget::fixture(3, 2, 6));
    assert_eq!(
        recovery.next(FailureRecord::new(
            1,
            FailureClass::Inconclusive,
            "action-1",
            "inconclusive|no-evidence",
        )),
        RecoveryDecision::NeedsReview
    );
    assert_eq!(recovery.next(FailureRecord::fixture(2)).as_str(), "retry");
    assert_eq!(
        recovery.next(FailureRecord::new(
            1,
            FailureClass::Unrecoverable,
            "action-1",
            "unrecoverable|no-evidence",
        )),
        RecoveryDecision::Incomplete {
            reason: StopReason::Unrecoverable
        }
    );
    assert_eq!(
        recovery.next(FailureRecord::new(
            1,
            FailureClass::Cancelled,
            "action-1",
            "cancelled|no-evidence",
        )),
        RecoveryDecision::Cancelled
    );
}

#[test]
fn exhausting_the_total_budget_stops_with_budget_exhausted() {
    let mut recovery = RecoveryController::new(Budget::fixture(5, 2, 2));
    recovery.record(FailureRecord::fixture(1));
    assert_eq!(
        recovery.next(FailureRecord::fixture(2)),
        RecoveryDecision::Incomplete {
            reason: StopReason::BudgetExhausted
        }
    );
}

#[test]
fn a_new_step_resets_counters_and_history() {
    let mut recovery = RecoveryController::new(Budget::fixture(3, 2, 6));
    recovery.record(FailureRecord::fixture(1));
    recovery.record(FailureRecord::fixture(2));
    recovery.begin_step(2);
    assert_eq!(recovery.strategy(), 1);
    assert_eq!(recovery.next_attempt_number(), 1);
    assert!(recovery.history().is_empty());
    assert_eq!(
        recovery.next(FailureRecord::new(
            2,
            FailureClass::CheckFailed,
            "action-1",
            "signature-1",
        )),
        RecoveryDecision::Retry
    );
}

// --------------------------------------------------------- frozen contract

fn acceptance() -> FrozenAcceptance {
    AcceptanceSpec {
        schema_version: 1,
        mission: "recovery_fixture".to_owned(),
        mission_version: 1,
        goal: "Produce the requested note in the workspace".to_owned(),
        inputs: Default::default(),
        steps: vec![PlannedStep {
            index: 1,
            title: "Write notes.md".to_owned(),
            criterion: "notes.md exists and is the requested note".to_owned(),
            checks: vec![CheckSpec::new("file_exists", [("path", json!("notes.md"))])],
            tool: Some("write_file".to_owned()),
        }],
        output: OutputContract::default(),
        allowed_tools: ["write_file".to_owned()].into_iter().collect(),
        allowed_checks: ["file_exists".to_owned()].into_iter().collect(),
        max_steps: 8,
        max_attempts: 2,
    }
    .freeze()
    .expect("fixture acceptance is valid")
}

fn strategy(acceptance: &FrozenAcceptance) -> ExecutionStrategy {
    ExecutionStrategy {
        acceptance_hash: acceptance.hash().to_owned(),
        actions: Vec::new(),
    }
}

#[test]
fn accept_strategy_rejects_a_changed_acceptance_hash() {
    let acceptance = acceptance();
    let mut recovery = RecoveryController::new(Budget::fixture(3, 2, 6));
    let mut forged = strategy(&acceptance);
    forged.acceptance_hash = "0".repeat(64);
    match recovery.accept_strategy(&acceptance, &forged) {
        Err(RecoveryError::Plan(PlanError::AcceptanceHashMismatch)) => {}
        other => panic!("expected an acceptance hash mismatch, got {other:?}"),
    }
    // A rejected strategy must not spend a slot.
    assert_eq!(recovery.strategy(), 1);
    assert!(recovery.can_replan());
}

#[test]
fn accept_strategy_requires_remaining_strategy_budget() {
    let acceptance = acceptance();
    let mut recovery = RecoveryController::new(Budget::fixture(3, 1, 6));
    match recovery.accept_strategy(&acceptance, &strategy(&acceptance)) {
        Err(RecoveryError::NoStrategyBudget) => {}
        other => panic!("expected no strategy budget, got {other:?}"),
    }
}

#[test]
fn accept_strategy_keeps_the_frozen_contract_and_spends_one_slot() {
    let acceptance = acceptance();
    let before = acceptance.hash().to_owned();
    let mut recovery = RecoveryController::new(Budget::fixture(3, 2, 6));
    recovery.record(FailureRecord::fixture(1));
    let started = recovery
        .accept_strategy(&acceptance, &strategy(&acceptance))
        .expect("valid strategy is accepted");
    assert_eq!(started, 2);
    assert_eq!(recovery.strategy(), 2);
    assert!(!recovery.can_replan());
    assert_eq!(acceptance.hash(), before);
    acceptance.verify_hash().expect("contract unchanged");
}

// ------------------------------------------------------------- packets

fn failing_check(attempt: &lfm3_harness::AttemptId) -> CheckResult {
    CheckResult {
        kind: "file_exists".to_owned(),
        acceptance_hash: "hash".to_owned(),
        step_index: 1,
        attempt: attempt.clone(),
        status: CheckStatus::Fail,
        message: "  notes.md   does NOT exist  ".to_owned(),
        evidence: vec![CheckEvidence {
            source: "workspace:notes.md".to_owned(),
            attempt: Some(attempt.clone()),
            digest: Some(ContentDigest::of(b"draft", "text/markdown")),
            detail: "line 1: draft".to_owned(),
            semantic: None,
        }],
    }
}

#[test]
fn canonical_actions_ignore_key_order() {
    let first = json!({"b": 1, "a": 2});
    let second = json!({"a": 2, "b": 1});
    assert_eq!(canonical_action(&first), canonical_action(&second));
    assert_eq!(canonical_action(&first), "{\"a\":2,\"b\":1}");
}

#[test]
fn signatures_normalize_ids_numbers_and_whitespace() {
    assert_eq!(
        normalize_text("Wrote 3 files to deadbeefdeadbeefdeadbeef"),
        "wrote <n> files to <id>"
    );
    let attempt = lfm3_harness::AttemptId::new();
    let checks = vec![failing_check(&attempt)];
    assert_eq!(
        failure_signature(FailureClass::CheckFailed, &checks),
        "check_failed|file_exists:fail:notes.md does not exist"
    );
    assert_eq!(
        failure_signature(FailureClass::CheckFailed, &[]),
        "check_failed|no-evidence"
    );
}

#[test]
fn packets_carry_typed_evidence_and_remaining_budgets_only() {
    let acceptance = acceptance();
    let step = acceptance.spec().steps[0].clone();
    let attempt = lfm3_harness::AttemptId::new();
    let checks = vec![failing_check(&attempt)];
    let signature = failure_signature(FailureClass::CheckFailed, &checks);
    let record = FailureRecord::new(
        1,
        FailureClass::CheckFailed,
        canonical_action(&json!({"tool": "write_file", "path": "notes.md"})),
        signature,
    )
    .with_message("the file never appeared");
    let remaining = RemainingBudget::from_counts(Budget::fixture(3, 2, 6), 1, 1, 1);
    let packet = FailurePacket::build(
        &acceptance,
        &step,
        1,
        &record,
        &checks,
        &["prior-signature".to_owned()],
        remaining,
    );

    assert_eq!(packet.acceptance_hash, acceptance.hash());
    assert_eq!(packet.step_index, 1);
    assert_eq!(packet.strategy, 1);
    assert_eq!(packet.class, FailureClass::CheckFailed);
    assert_eq!(packet.failed_checks.len(), 1);
    assert_eq!(packet.failed_checks[0].status, CheckStatus::Fail);
    assert_eq!(packet.evidence.len(), 1);
    assert_eq!(packet.evidence[0].source, "workspace:notes.md");
    assert_eq!(packet.evidence[0].attempt.as_ref(), Some(&attempt));
    assert_eq!(
        packet.evidence[0].sha256.as_deref(),
        Some(ContentDigest::of(b"draft", "text/markdown").sha256.as_str())
    );
    assert_eq!(
        packet.remaining,
        RemainingBudget {
            attempts_left_in_strategy: 2,
            strategies_left: 1,
            total_attempts_left: 5,
        }
    );
    assert_eq!(packet.prior_signatures, vec!["prior-signature".to_owned()]);

    // The packet is an allow-list: no transcript, prompt, or raw bytes ride along.
    let value = serde_json::to_value(&packet).expect("packet json");
    let mut keys: Vec<String> = value
        .as_object()
        .expect("packet object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "acceptance_hash",
            "action",
            "class",
            "criterion",
            "evidence",
            "failed_checks",
            "message",
            "prior_signatures",
            "remaining",
            "signature",
            "step_index",
            "strategy",
        ]
    );
    let long = "x".repeat(500);
    let clipped = FailurePacket::build(
        &acceptance,
        &step,
        1,
        &record.clone().with_message(long),
        &checks,
        &[],
        remaining,
    );
    assert_eq!(clipped.message.chars().count(), 403);
    assert!(clipped.message.ends_with("..."));
}

#[test]
fn evidence_refs_deduplicate_and_cap() {
    let attempt = lfm3_harness::AttemptId::new();
    let mut check = failing_check(&attempt);
    let first = check.evidence[0].clone();
    check.evidence = std::iter::repeat_n(first.clone(), 40).collect();
    let refs = evidence_refs(&[check]);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].source, first.source);
}

// ------------------------------------------------- runner-level replanning

async fn open_store() -> (TempDir, Store) {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::open(directory.path().join("runs.db"))
        .await
        .expect("store opens");
    (directory, store)
}

fn workspace(directory: &TempDir) -> PathBuf {
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    workspace
}

fn checked_acceptance() -> FrozenAcceptance {
    let mut checked = PlannedStep {
        index: 1,
        title: "Write notes.md".to_owned(),
        criterion: "notes.md contains the verified note".to_owned(),
        checks: vec![CheckSpec::new(
            "content_matches",
            [("path", json!("notes.md")), ("contains", json!("verified"))],
        )],
        tool: Some("write_file".to_owned()),
    };
    checked
        .checks
        .push(CheckSpec::new("file_exists", [("path", json!("notes.md"))]));
    let mut spec = acceptance().spec().clone();
    spec.steps = vec![checked];
    spec.max_attempts = 2;
    spec.freeze().expect("checked acceptance")
}

/// Fails on strategy 1, then succeeds once replanned to strategy 2.
struct ReplanningDriver {
    workspace: PathBuf,
    bad_hash: bool,
    replans: Mutex<Vec<Value>>,
}

impl ReplanningDriver {
    fn new(workspace: &Path, bad_hash: bool) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            bad_hash,
            replans: Mutex::new(Vec::new()),
        }
    }

    fn replans(&self) -> Vec<Value> {
        self.replans.lock().expect("replans lock").clone()
    }
}

impl StepDriver for ReplanningDriver {
    fn model(&self) -> String {
        "fake-model".to_owned()
    }

    fn execute<'a>(
        &'a self,
        request: StepRequest<'a>,
        _cancel: &'a CancellationChild,
    ) -> BoxFuture<'a, Result<StepExecution, DriverError>> {
        Box::pin(async move {
            let content = if request.strategy == 1 {
                "draft"
            } else {
                "verified note"
            };
            std::fs::write(self.workspace.join("notes.md"), content)
                .map_err(|error| DriverError::Failed(error.to_string()))?;
            Ok(StepExecution {
                model: self.model(),
                action: json!({
                    "tool": "write_file",
                    "path": "notes.md",
                    "strategy": request.strategy,
                }),
                tool_results: Vec::new(),
                command_runs: Vec::new(),
            })
        })
    }

    fn replan_strategy<'a>(
        &'a self,
        request: StepRequest<'a>,
        _cancel: &'a CancellationChild,
    ) -> BoxFuture<'a, Result<ExecutionStrategy, DriverError>> {
        Box::pin(async move {
            let packet = request.packet.ok_or_else(|| {
                DriverError::Failed("replanning needs a failure packet".to_owned())
            })?;
            self.replans.lock().expect("replans lock").push(json!({
                "step": packet.step_index,
                "class": packet.class.as_str(),
                "strategy": packet.strategy,
                "remaining": packet.remaining,
                "requested_strategy": request.strategy,
            }));
            let hash = if self.bad_hash {
                "0".repeat(64)
            } else {
                request
                    .acceptance
                    .clone()
                    .freeze()
                    .map(|frozen| frozen.hash().to_owned())
                    .unwrap_or_default()
            };
            Ok(ExecutionStrategy {
                acceptance_hash: hash,
                actions: Vec::new(),
            })
        })
    }
}

fn runner(
    store: &Store,
    acceptance: &FrozenAcceptance,
    workspace: &Path,
    driver: Arc<dyn StepDriver>,
) -> Runner {
    Runner::new(
        store.clone(),
        acceptance.clone(),
        workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .expect("runner")
}

#[tokio::test]
async fn replanning_switches_strategy_and_passes() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = checked_acceptance();
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ReplanningDriver::new(&workspace, false));
    let outcome = runner(&store, &acceptance, &workspace, driver.clone())
        .with_budget(Budget::fixture(1, 2, 3))
        .execute(&run.id)
        .await
        .expect("execute");

    assert_eq!(outcome.status, RunStatus::Passed);
    assert_eq!(outcome.reason, None);

    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    let attempts = &snapshot.steps[0].attempts;
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].id, attempts[1].id);
    assert_eq!(attempts[0].strategy, 1);
    assert_eq!(attempts[1].strategy, 2);
    assert_eq!(attempts[0].status, AttemptStatus::Failed);
    assert!(attempts[0]
        .error
        .as_deref()
        .is_some_and(|error| error.contains("content_matches")));
    assert_eq!(attempts[1].status, AttemptStatus::Passed);
    assert_eq!(attempts[1].action["strategy"], json!(2));

    let replans = driver.replans();
    assert_eq!(replans.len(), 1);
    assert_eq!(replans[0]["step"], json!(1));
    assert_eq!(replans[0]["class"], json!("check_failed"));
    assert_eq!(replans[0]["strategy"], json!(1));
    assert_eq!(replans[0]["requested_strategy"], json!(1));
    assert_eq!(replans[0]["remaining"]["strategies_left"], json!(1));

    let events = store.events_after(&run.id, 0).await.expect("events");
    let decisions: Vec<Value> = events
        .iter()
        .filter(|event| event.kind == "recovery.decision")
        .map(|event| event.payload["decision"]["outcome"].clone())
        .collect();
    assert_eq!(decisions, vec![json!("replan"), json!("replan_accepted")]);
    let accepted: Vec<u32> = events
        .iter()
        .filter(|event| event.kind == "recovery.decision")
        .filter(|event| event.payload["decision"]["outcome"] == json!("replan_accepted"))
        .map(|event| {
            u32::try_from(event.payload["strategy"].as_u64().unwrap_or_default())
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(accepted, vec![2]);
}

#[tokio::test]
async fn a_replan_that_changes_the_acceptance_never_passes() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = checked_acceptance();
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ReplanningDriver::new(&workspace, true));
    let outcome = runner(&store, &acceptance, &workspace, driver)
        .with_budget(Budget::fixture(1, 2, 3))
        .execute(&run.id)
        .await
        .expect("execute");

    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(outcome.reason, Some(IncompleteReason::NoProgress));
    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.steps[0].attempts.len(), 1);
    assert_ne!(snapshot.run.status, RunStatus::Passed);

    let events = store.events_after(&run.id, 0).await.expect("events");
    let outcomes: Vec<Value> = events
        .iter()
        .filter(|event| event.kind == "recovery.decision")
        .map(|event| event.payload["decision"]["outcome"].clone())
        .collect();
    assert_eq!(outcomes, vec![json!("replan"), json!("replan_failed")]);
    let failed = events
        .iter()
        .find(|event| event.payload["decision"]["outcome"] == json!("replan_failed"))
        .expect("replan failure journaled");
    assert!(failed.payload["decision"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("acceptance hash"));
}
