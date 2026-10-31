//! Durable runner and coordinator gates.
//!
//! These tests exercise the state machine the way a model driver would use it:
//! steps commit before side effects, attempts commit before verification, a
//! pass needs same-attempt evidence, a second concurrent run is rejected, and
//! cancellation can never turn into `passed`. Golden journals pin the durable
//! event sequence for pass, failure, final failure, cancellation, and restart.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use lfm3_harness::{
    agent::BoxFuture,
    verify::{CheckContext, CheckError, CheckEvidence, CheckResult, CheckStatus},
    AcceptanceSpec, AttemptStatus, CancellationChild, CancellationContext, CheckSpec,
    CoordinatorError, DriverError, FrozenAcceptance, HarnessError, IncompleteReason,
    OutputContract, PlannedStep, RequiredArtifact, RunCoordinator, RunStatus, Runner, StartAttempt,
    StepDriver, StepExecution, StepRequest, StepStatus, Store, Verifier,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{sync::Semaphore, time::timeout};

// ---------------------------------------------------------------- fixtures

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

fn acceptance(
    steps: Vec<PlannedStep>,
    max_attempts: usize,
    output: OutputContract,
) -> FrozenAcceptance {
    AcceptanceSpec {
        schema_version: 1,
        mission: "runner_fixture".to_owned(),
        mission_version: 1,
        goal: "Produce the requested note in the workspace".to_owned(),
        inputs: Default::default(),
        steps,
        output,
        allowed_tools: ["write_file".to_owned()].into_iter().collect(),
        allowed_checks: ["file_exists".to_owned(), "content_matches".to_owned()]
            .into_iter()
            .collect(),
        max_steps: 8,
        max_attempts,
    }
    .freeze()
    .expect("fixture acceptance is valid")
}

fn step(index: u32, path: &str) -> PlannedStep {
    PlannedStep {
        index,
        title: format!("Write {path}"),
        criterion: format!("{path} exists and is the requested note"),
        checks: vec![CheckSpec::new("file_exists", [("path", json!(path))])],
        tool: Some("write_file".to_owned()),
    }
}

fn runner(
    store: &Store,
    acceptance: &FrozenAcceptance,
    workspace: &Path,
    driver: Arc<dyn StepDriver>,
    cancel: CancellationContext,
) -> Runner {
    Runner::new(store.clone(), acceptance.clone(), workspace, driver, cancel).expect("runner")
}

#[derive(Clone)]
enum Action {
    Write {
        path: &'static str,
        content: &'static str,
    },
    Fail(&'static str),
}

/// Deterministic driver: writes files or fails a fixed number of times.
struct ScriptedDriver {
    workspace: PathBuf,
    script: Vec<Action>,
    calls: AtomicUsize,
    store: Option<Store>,
    observed_step_counts: Mutex<Vec<usize>>,
    started: Option<Arc<Semaphore>>,
    release: Option<Arc<Semaphore>>,
}

impl ScriptedDriver {
    fn new(workspace: &Path, script: Vec<Action>) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            script,
            calls: AtomicUsize::new(0),
            store: None,
            observed_step_counts: Mutex::new(Vec::new()),
            started: None,
            release: None,
        }
    }

    fn observing(mut self, store: &Store) -> Self {
        self.store = Some(store.clone());
        self
    }

    fn gated(mut self, started: Arc<Semaphore>, release: Arc<Semaphore>) -> Self {
        self.started = Some(started);
        self.release = Some(release);
        self
    }

    fn observed_step_counts(&self) -> Vec<usize> {
        self.observed_step_counts
            .lock()
            .expect("observed lock")
            .clone()
    }
}

impl StepDriver for ScriptedDriver {
    fn model(&self) -> String {
        "fake-model".to_owned()
    }

    fn execute<'a>(
        &'a self,
        request: StepRequest<'a>,
        _cancel: &'a CancellationChild,
    ) -> BoxFuture<'a, Result<StepExecution, DriverError>> {
        Box::pin(async move {
            if let Some(started) = &self.started {
                started.add_permits(1);
            }
            if let Some(release) = &self.release {
                let _ = release.acquire().await;
            }
            if let Some(store) = &self.store {
                let snapshot = store
                    .snapshot(request.run_id)
                    .await
                    .map_err(|error| DriverError::Failed(error.to_string()))?;
                self.observed_step_counts
                    .lock()
                    .expect("observed lock")
                    .push(snapshot.steps.len());
            }
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            match self
                .script
                .get(index)
                .or_else(|| self.script.last())
                .cloned()
            {
                Some(Action::Fail(message)) => Err(DriverError::Failed(message.to_owned())),
                Some(Action::Write { path, content }) => {
                    std::fs::write(self.workspace.join(path), content)
                        .map_err(|error| DriverError::Failed(error.to_string()))?;
                    Ok(StepExecution {
                        model: self.model(),
                        action: json!({"tool": "write_file", "path": path}),
                        tool_results: Vec::new(),
                        command_runs: Vec::new(),
                    })
                }
                None => Err(DriverError::Failed("driver script is exhausted".to_owned())),
            }
        })
    }
}

fn write(path: &'static str, content: &'static str) -> Action {
    Action::Write { path, content }
}

/// A verifier that always claims a pass without any evidence.
struct NoEvidenceVerifier;

impl Verifier for NoEvidenceVerifier {
    fn evaluate<'a>(
        &'a self,
        spec: CheckSpec,
        context: CheckContext,
    ) -> BoxFuture<'a, Result<CheckResult, CheckError>> {
        Box::pin(async move {
            Ok(CheckResult {
                kind: spec.kind,
                acceptance_hash: context.acceptance_hash,
                step_index: context.step_index,
                attempt: context.attempt,
                status: CheckStatus::Pass,
                message: "trust me".to_owned(),
                evidence: Vec::new(),
            })
        })
    }
}

/// A verifier that blocks until released, so cancellation can race it.
struct BlockingVerifier {
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

impl Verifier for BlockingVerifier {
    fn evaluate<'a>(
        &'a self,
        spec: CheckSpec,
        context: CheckContext,
    ) -> BoxFuture<'a, Result<CheckResult, CheckError>> {
        Box::pin(async move {
            self.started.add_permits(1);
            let _ = self.release.acquire().await;
            Ok(CheckResult {
                kind: spec.kind,
                acceptance_hash: context.acceptance_hash.clone(),
                step_index: context.step_index,
                attempt: context.attempt.clone(),
                status: CheckStatus::Pass,
                message: "released".to_owned(),
                evidence: vec![CheckEvidence {
                    source: "fake".to_owned(),
                    attempt: Some(context.attempt),
                    digest: None,
                    detail: "blocked verifier".to_owned(),
                    semantic: None,
                }],
            })
        })
    }
}

// ------------------------------------------------------------ state machine

#[tokio::test]
async fn legal_run_reaches_passed() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "note")],
    ));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Passed);
    assert_eq!(outcome.reason, None);
    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.run.status, RunStatus::Passed);
    assert_eq!(snapshot.steps[0].status, StepStatus::Passed);
    assert_eq!(snapshot.steps[0].attempts.len(), 1);
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Passed);
}

#[tokio::test]
async fn all_steps_are_persisted_before_any_execution() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = acceptance(
        vec![step(1, "a.md"), step(2, "b.md"), step(3, "c.md")],
        2,
        OutputContract::default(),
    );
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(
        ScriptedDriver::new(
            &workspace,
            vec![write("a.md", "a"), write("b.md", "b"), write("c.md", "c")],
        )
        .observing(&store),
    );

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver.clone(),
        CancellationContext::new(Duration::from_secs(30)),
    )
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Passed);
    assert_eq!(driver.observed_step_counts(), vec![3, 3, 3]);
}

#[tokio::test]
async fn final_check_failure_is_incomplete() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "note")],
    ));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .with_final_check(CheckSpec::new(
        "file_exists",
        [("path", json!("summary.md"))],
    ))
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(
        outcome.reason,
        Some(IncompleteReason::FinalVerificationFailed)
    );
    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.run.status, RunStatus::Incomplete);
    // The step itself succeeded; only the final contract rejected the run.
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Passed);
}

#[tokio::test]
async fn empty_required_artifact_is_incomplete() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let output = OutputContract {
        schema: None,
        required_artifacts: vec![RequiredArtifact {
            path: "notes.md".to_owned(),
            non_empty: true,
        }],
    };
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, output);
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(&workspace, vec![write("notes.md", "")]));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(
        outcome.reason,
        Some(IncompleteReason::FinalVerificationFailed)
    );
    // The step only proved existence; the final contract rejected the empty
    // artifact, so the run itself never passed.
    assert_ne!(
        store.snapshot(&run.id).await.expect("snapshot").run.status,
        RunStatus::Passed
    );
}

#[tokio::test]
async fn pass_without_same_attempt_evidence_is_rejected() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "note")],
    ));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .with_verifier(Arc::new(NoEvidenceVerifier))
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(
        outcome.reason,
        Some(IncompleteReason::StepBudgetExhausted { step_index: 1 })
    );
    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.steps[0].attempts.len(), 2);
    assert!(snapshot.steps[0]
        .attempts
        .iter()
        .all(|attempt| attempt.status == AttemptStatus::Inconclusive));
}

#[tokio::test]
async fn cancellation_during_verification_never_passes() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let cancel = CancellationContext::new(Duration::from_secs(60));
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "note")],
    ));
    let runner = runner(&store, &acceptance, &workspace, driver, cancel.clone()).with_verifier(
        Arc::new(BlockingVerifier {
            started: started.clone(),
            release: release.clone(),
        }),
    );

    let run_id = run.id.clone();
    let handle = tokio::spawn(async move { runner.execute(&run_id).await });
    started.acquire().await.expect("verifier started").forget();
    cancel.cancel();
    release.add_permits(1);

    let outcome = timeout(Duration::from_secs(5), handle)
        .await
        .expect("runner stopped")
        .expect("join")
        .expect("execute");

    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_eq!(outcome.reason, Some(IncompleteReason::Cancelled));
    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.run.status, RunStatus::Cancelled);
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Aborted);
    assert!(snapshot.steps[0]
        .attempts
        .iter()
        .all(|attempt| attempt.status != AttemptStatus::Passed));
}

#[tokio::test]
async fn second_active_run_is_rejected_as_busy() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, OutputContract::default());
    let first = store
        .create_run(&acceptance, None)
        .await
        .expect("first run");
    let second = store
        .create_run(&acceptance, None)
        .await
        .expect("second run");

    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let driver = Arc::new(
        ScriptedDriver::new(&workspace, vec![write("notes.md", "note")])
            .gated(started.clone(), release.clone()),
    );
    let idle = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "note")],
    ));
    let coordinator = RunCoordinator::new(store.clone());
    let runner_one = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    );
    let runner_two = runner(
        &store,
        &acceptance,
        &workspace,
        idle,
        CancellationContext::new(Duration::from_secs(30)),
    );

    let first_id = first.id.clone();
    let running = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.start(first_id, runner_one).await })
    };
    started.acquire().await.expect("driver started").forget();

    match coordinator.start(second.id.clone(), runner_two).await {
        Err(CoordinatorError::RunBusy { active }) => assert_eq!(active, first.id),
        other => panic!("expected RunBusy, got {other:?}"),
    }

    release.add_permits(1);
    let outcome = running.await.expect("join").expect("coordinator");
    assert_eq!(outcome.status, RunStatus::Passed);
    assert_eq!(coordinator.active().await, None);
}

#[tokio::test]
async fn illegal_transitions_fail_closed() {
    let (_directory, store) = open_store().await;
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");

    let error = store
        .transition_run(run.id.clone(), RunStatus::Created, RunStatus::Passed, None)
        .await
        .expect_err("created cannot jump to passed");
    assert!(matches!(error, HarnessError::Conflict(_)));

    store
        .transition_run(run.id.clone(), RunStatus::Created, RunStatus::Running, None)
        .await
        .expect("created to running is legal");
    let error = store
        .transition_run(run.id.clone(), RunStatus::Created, RunStatus::Running, None)
        .await
        .expect_err("stale source status is rejected");
    assert!(matches!(error, HarnessError::Conflict(_)));
}

#[tokio::test]
async fn driver_failure_retries_and_then_passes() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = acceptance(vec![step(1, "notes.md")], 3, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![
            Action::Fail("model transport reset"),
            write("notes.md", "note"),
        ],
    ));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Passed);
    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.steps[0].attempts.len(), 2);
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Failed);
    assert_eq!(snapshot.steps[0].attempts[1].status, AttemptStatus::Passed);
}

#[tokio::test]
async fn failed_check_retries_with_a_new_attempt() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let mut checked = step(1, "notes.md");
    checked.checks = vec![CheckSpec::new(
        "content_matches",
        [("path", json!("notes.md")), ("contains", json!("verified"))],
    )];
    let acceptance = acceptance(vec![checked], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![
            write("notes.md", "draft"),
            write("notes.md", "verified note"),
        ],
    ));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Passed);
    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    let attempts = &snapshot.steps[0].attempts;
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].id, attempts[1].id);
    assert_eq!(attempts[0].status, AttemptStatus::Failed);
    assert_eq!(attempts[1].status, AttemptStatus::Passed);
}

// ---------------------------------------------------------------- journals

async fn journal(store: &Store, run_id: &lfm3_harness::RunId) -> Vec<Value> {
    let events = store.events_after(run_id, 0).await.expect("events");
    let mut attempts = Vec::new();
    events
        .iter()
        .map(|event| {
            let mut payload = event.payload.clone();
            normalize(&mut payload, &mut attempts);
            json!({"kind": event.kind, "payload": payload})
        })
        .collect()
}

/// Scrubs run ids, attempt ids, and acceptance hashes so journals are stable.
fn normalize(value: &mut Value, attempts: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                match key.as_str() {
                    "attempt" => {
                        if let Some(raw) = item.as_str() {
                            let label = match attempts.iter().position(|seen| seen == raw) {
                                Some(position) => format!("attempt-{position}"),
                                None => {
                                    attempts.push(raw.to_owned());
                                    format!("attempt-{}", attempts.len() - 1)
                                }
                            };
                            *item = json!(label);
                        }
                    }
                    "acceptance_hash" => *item = json!("<acceptance>"),
                    _ => normalize(item, attempts),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize(item, attempts);
            }
        }
        _ => {}
    }
}

fn assert_golden(name: &str, journal: &[Value]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    let actual = format!(
        "{}\n",
        serde_json::to_string_pretty(journal).expect("journal json")
    );
    if !path.exists() {
        std::fs::create_dir_all(path.parent().expect("golden parent")).expect("create goldens");
        std::fs::write(&path, &actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).expect("read golden");
    assert_eq!(expected, actual, "normalized journal {name} changed");
}

#[tokio::test]
async fn golden_pass_journal() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let output = OutputContract {
        schema: None,
        required_artifacts: vec![RequiredArtifact {
            path: "notes.md".to_owned(),
            non_empty: true,
        }],
    };
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, output);
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "verified note")],
    ));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Passed);
    assert_golden("runner_pass.json", &journal(&store, &run.id).await);
}

#[tokio::test]
async fn golden_step_failure_journal() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let mut checked = step(1, "notes.md");
    checked.checks = vec![CheckSpec::new(
        "content_matches",
        [("path", json!("notes.md")), ("contains", json!("done"))],
    )];
    let acceptance = acceptance(vec![checked], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "draft")],
    ));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(
        outcome.reason,
        Some(IncompleteReason::StepBudgetExhausted { step_index: 1 })
    );
    assert_golden("runner_step_failure.json", &journal(&store, &run.id).await);
}

#[tokio::test]
async fn golden_final_failure_journal() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let output = OutputContract {
        schema: None,
        required_artifacts: vec![RequiredArtifact {
            path: "summary.md".to_owned(),
            non_empty: true,
        }],
    };
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, output);
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "note")],
    ));

    let outcome = runner(
        &store,
        &acceptance,
        &workspace,
        driver,
        CancellationContext::new(Duration::from_secs(30)),
    )
    .execute(&run.id)
    .await
    .expect("execute");

    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(
        outcome.reason,
        Some(IncompleteReason::FinalVerificationFailed)
    );
    assert_golden("runner_final_failure.json", &journal(&store, &run.id).await);
}

#[tokio::test]
async fn golden_cancellation_journal() {
    let (directory, store) = open_store().await;
    let workspace = workspace(&directory);
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let cancel = CancellationContext::new(Duration::from_secs(60));
    let driver = Arc::new(ScriptedDriver::new(
        &workspace,
        vec![write("notes.md", "note")],
    ));
    let runner = runner(&store, &acceptance, &workspace, driver, cancel.clone()).with_verifier(
        Arc::new(BlockingVerifier {
            started: started.clone(),
            release: release.clone(),
        }),
    );

    let run_id = run.id.clone();
    let handle = tokio::spawn(async move { runner.execute(&run_id).await });
    started.acquire().await.expect("verifier started").forget();
    cancel.cancel();
    release.add_permits(1);
    let outcome = timeout(Duration::from_secs(5), handle)
        .await
        .expect("runner stopped")
        .expect("join")
        .expect("execute");

    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_golden("runner_cancellation.json", &journal(&store, &run.id).await);
}

#[tokio::test]
async fn golden_interrupted_restart_journal() {
    let (directory, store) = open_store().await;
    let acceptance = acceptance(vec![step(1, "notes.md")], 2, OutputContract::default());
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    store
        .persist_pending_steps(&run.id, &acceptance.spec().steps)
        .await
        .expect("steps");
    store
        .start_attempt(StartAttempt {
            run_id: run.id.clone(),
            step_index: 1,
            strategy: 1,
            action: json!({"tool": "write_file", "path": "notes.md"}),
            model: "fake-model".to_owned(),
            deadline_at: None,
        })
        .await
        .expect("attempt");

    // A second engine instance reconciles the abandoned first instance.
    let restarted = Store::open(directory.path().join("runs.db"))
        .await
        .expect("restarted store");
    let reconciliation = restarted.reconcile_interrupted().await.expect("reconcile");
    assert_eq!(reconciliation.runs, 1);

    let snapshot = restarted.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.run.status, RunStatus::Interrupted);
    assert_eq!(snapshot.steps[0].status, StepStatus::Blocked);
    assert_eq!(
        snapshot.steps[0].attempts[0].status,
        AttemptStatus::Interrupted
    );
    assert_golden(
        "runner_interrupted.json",
        &journal(&restarted, &run.id).await,
    );
}
