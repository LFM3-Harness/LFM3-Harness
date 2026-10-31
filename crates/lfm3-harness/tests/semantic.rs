//! Semantic checks are evidence, not authority.
//!
//! Every test here starts from the same premise: a confident model output is
//! not a pass. A semantic result may pass only when the mission carries an
//! automatic holdout qualification, the judge is reachable, the answer is
//! unambiguous, its confidence clears the frozen threshold, and every judged
//! value is literally present in an immutable source. A deterministic failure
//! in the same step always wins.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use lfm3_harness::{
    agent::{BoxFuture, SemanticVerifier},
    mission::{load_missions, CheckSpec, InputArtifact, MissionManifest},
    sha256_hex,
    store::ArtifactStore,
    verify::{
        evaluate_semantic, is_semantic_kind, CheckContext, CheckStatus, ContentDigest, JudgeError,
        JudgeFuture, JudgeRequest, JudgeVerdict, Qualification, SemanticJudge, SemanticMode,
        SemanticPolicy,
    },
    AcceptanceSpec, ArtifactId, AttemptId, AttemptStatus, CancellationContext, DriverError,
    FrozenAcceptance, IncompleteReason, OutputContract, PlannedStep, RunStatus, Runner, StepDriver,
    StepExecution, StepRequest, StepStatus, Store,
};
use serde_json::{json, Value};
use tempfile::TempDir;

// ---------------------------------------------------------------- fixtures

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/semantic/dev")
}

struct Fixture {
    _directory: TempDir,
    root: PathBuf,
    attempt: AttemptId,
    artifacts: ArtifactStore,
    inputs: BTreeMap<String, InputArtifact>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical workspace");
        let artifacts = ArtifactStore::new(root.join("artifacts"));
        for name in [
            "transcript.txt",
            "output_grounded.json",
            "output_fabricated.json",
            "output_wrong_total.json",
            "spec.md",
            "screen.png",
        ] {
            fs::copy(fixture_root().join(name), root.join(name)).expect("copy fixture");
        }
        fs::write(
            root.join("notes.md"),
            "The caller is Dana Whitfield with reference AX-4471.\n",
        )
        .expect("write note");
        let mut inputs = BTreeMap::new();
        for (name, file) in [("transcript", "transcript.txt"), ("image", "screen.png")] {
            let bytes = fs::read(root.join(file)).expect("read fixture");
            let sha256 = sha256_hex(&bytes);
            artifacts.write(&sha256, &bytes).expect("store artifact");
            inputs.insert(
                name.to_owned(),
                InputArtifact {
                    id: ArtifactId::new(),
                    sha256,
                },
            );
        }
        let attempt = AttemptId::new();
        Self {
            _directory: directory,
            root,
            attempt,
            artifacts,
            inputs,
        }
    }

    fn context(&self) -> CheckContext {
        let mut context =
            CheckContext::new("a".repeat(64), 1, self.attempt.clone(), self.root.clone());
        context.artifacts = Some(self.artifacts.clone());
        context.inputs = self.inputs.clone();
        context
    }

    fn digest(&self, relative: &str) -> ContentDigest {
        ContentDigest::of_file(&self.root.join(relative))
            .expect("digest fixture")
            .0
    }
}

// ------------------------------------------------------------ scripted judge

#[derive(Clone)]
enum Reply {
    Verdict {
        label: &'static str,
        confidence: f64,
        rationale: &'static str,
    },
    Vision(&'static str),
    Failure(&'static str),
}

struct ScriptedJudge {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<JudgeRequest>>,
}

impl ScriptedJudge {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<JudgeRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl SemanticJudge for ScriptedJudge {
    fn judge<'a>(
        &'a self,
        request: JudgeRequest,
    ) -> JudgeFuture<'a, Result<JudgeVerdict, JudgeError>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
            let reply = self
                .replies
                .lock()
                .expect("replies")
                .pop_front()
                .expect("judge script is exhausted");
            match reply {
                Reply::Verdict {
                    label,
                    confidence,
                    rationale,
                } => Ok(JudgeVerdict {
                    label: label.to_owned(),
                    confidence,
                    model: "fake-d1".to_owned(),
                    prompt: request.instruction.clone(),
                    response: format!("{{\"label\":\"{label}\",\"confidence\":{confidence}}}"),
                    rationale: rationale.to_owned(),
                }),
                Reply::Vision(observation) => Ok(JudgeVerdict {
                    label: String::new(),
                    confidence: 1.0,
                    model: "fake-vl".to_owned(),
                    prompt: request.instruction.clone(),
                    response: observation.to_owned(),
                    rationale: String::new(),
                }),
                Reply::Failure(message) => Err(JudgeError::Unavailable(message.to_owned())),
            }
        })
    }
}

fn automatic() -> SemanticPolicy {
    SemanticPolicy {
        mode: SemanticMode::Automatic,
        min_confidence_bps: 8_000,
        qualification: Some(Qualification {
            holdout_sha256: "b".repeat(64),
            runs: 4,
            false_completions: 0,
            evaluated_at: "2026-10-31".to_owned(),
        }),
        reason: None,
    }
}

fn human_review() -> SemanticPolicy {
    SemanticPolicy {
        mode: SemanticMode::HumanReviewOnly,
        min_confidence_bps: 8_000,
        qualification: None,
        reason: Some("no holdout qualification is recorded".to_owned()),
    }
}

fn decision(kind: &str) -> CheckSpec {
    CheckSpec::new(
        kind,
        [
            ("path", json!("notes.md")),
            (
                "criterion",
                json!("the note records the caller's reference AX-4471"),
            ),
        ],
    )
}

fn verdict(label: &'static str, confidence: f64, rationale: &'static str) -> Reply {
    Reply::Verdict {
        label,
        confidence,
        rationale,
    }
}

// ------------------------------------------------------------------- policy

#[test]
fn human_review_only_never_consults_the_judge() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![verdict("supported", 0.99, "looks right")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        decision("d1_noul"),
        fixture.context(),
        judge.clone(),
        human_review(),
    ));
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.message.contains("human review required"));
    assert!(result
        .message
        .contains("no holdout qualification is recorded"));
    assert!(judge.requests().is_empty(), "the judge must not be called");
    assert!(result.evidence.iter().all(|item| item.semantic.is_none()));
}

#[test]
fn qualification_requires_runs_and_no_false_completions() {
    let mut policy = automatic();
    policy.qualification = None;
    assert!(policy.validate().is_err());

    let mut policy = automatic();
    policy.qualification.as_mut().expect("qualification").runs = 2;
    assert!(policy.validate().is_err());

    let mut policy = automatic();
    policy
        .qualification
        .as_mut()
        .expect("qualification")
        .false_completions = 1;
    assert!(policy.validate().is_err());

    let mut policy = automatic();
    policy
        .qualification
        .as_mut()
        .expect("qualification")
        .holdout_sha256 = "not-a-digest".to_owned();
    assert!(policy.validate().is_err());

    let mut policy = automatic();
    policy
        .qualification
        .as_mut()
        .expect("qualification")
        .evaluated_at = "  ".to_owned();
    assert!(policy.validate().is_err());

    assert!(automatic().validate().is_ok());
    assert!(human_review().validate().is_ok());
}

#[test]
fn unperformed_holdout_qualification_is_not_accepted() {
    let holdout =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/semantic/holdout");
    let manifest = fs::read(holdout.join("manifest.json")).expect("holdout manifest");
    let raw = fs::read_to_string(holdout.join("qualification.unperformed.json"))
        .expect("qualification fixture");
    let qualification: Qualification = serde_json::from_str(&raw).expect("qualification");
    assert_eq!(
        sha256_hex(&manifest),
        qualification.holdout_sha256,
        "the fixture must name the holdout it was written against"
    );
    let policy = SemanticPolicy {
        mode: SemanticMode::Automatic,
        min_confidence_bps: 8_000,
        qualification: Some(qualification),
        reason: None,
    };
    assert!(
        policy.validate().is_err(),
        "an unperformed holdout can never enable automatic passing"
    );
}

// --------------------------------------------------------------------- d1

#[test]
fn supported_verdict_passes_with_full_provenance() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![verdict("supported", 0.97, "the note states it")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        decision("d1_noul"),
        fixture.context(),
        judge.clone(),
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Pass, "{:?}", result.message);
    let judged = result
        .evidence
        .iter()
        .find(|item| item.semantic.is_some())
        .expect("provenance evidence");
    let semantic = judged.semantic.as_ref().expect("semantic evidence");
    assert_eq!(semantic.chain, "d1");
    assert_eq!(semantic.model, "fake-d1");
    assert_eq!(semantic.label, "supported");
    assert_eq!(semantic.confidence_bps, 9_700);
    assert_eq!(semantic.threshold_bps, 8_000);
    assert!(semantic
        .prompt
        .contains("the note records the caller's reference AX-4471"));
    assert_eq!(
        semantic.response,
        "{\"label\":\"supported\",\"confidence\":0.97}"
    );
    assert_eq!(judged.digest.as_ref(), Some(&fixture.digest("notes.md")));
    assert_eq!(judge.requests().len(), 1);
}

#[test]
fn unsupported_verdict_fails() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![verdict("unsupported", 0.99, "the note omits it")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        decision("d1_noul"),
        fixture.context(),
        judge,
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.evidence.iter().any(|item| item.semantic.is_some()));
}

#[test]
fn low_confidence_support_is_inconclusive() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![verdict("supported", 0.61, "probably")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        decision("d1_noul"),
        fixture.context(),
        judge,
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.message.contains("below"));
}

#[test]
fn ambiguous_verdict_is_inconclusive() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![verdict("ambiguous", 0.99, "cannot tell")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        decision("d1_noul"),
        fixture.context(),
        judge,
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.message.contains("ambiguous"));
}

#[test]
fn judge_failure_is_inconclusive() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![Reply::Failure("d1 endpoint is down")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        decision("d1_noul"),
        fixture.context(),
        judge,
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.message.contains("d1 endpoint is down"));
}

#[test]
fn d1_choice_wrong_label_fails() {
    let fixture = Fixture::new();
    let spec = CheckSpec::new(
        "d1_choice",
        [
            ("path", json!("notes.md")),
            ("criterion", json!("which reference number the caller gave")),
            (
                "options",
                json!({"AX-4471": "the note's reference", "ZZ-0000": "a different reference"}),
            ),
            ("expect", json!("AX-4471")),
        ],
    );
    let judge = ScriptedJudge::new(vec![verdict("ZZ-0000", 0.99, "chose the other one")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        spec,
        fixture.context(),
        judge,
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Fail);
}

// ---------------------------------------------------------------- grounded

fn grounded(path: &str) -> CheckSpec {
    CheckSpec::new(
        "d1_grounded",
        [
            ("path", json!(path)),
            ("source", json!("transcript")),
            (
                "fields",
                json!({
                    "/name": "the caller's full name",
                    "/reference": "the caller's reference number",
                }),
            ),
        ],
    )
}

#[test]
fn fabricated_field_fails_before_any_judge_call() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![
        verdict("grounded", 0.99, "present"),
        verdict("grounded", 0.99, "present"),
    ]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        grounded("output_fabricated.json"),
        fixture.context(),
        judge.clone(),
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("/reference"));
    assert_eq!(
        judge.requests().len(),
        1,
        "only the grounded /name field may reach the judge"
    );
    assert!(judge
        .requests()
        .iter()
        .all(|request| { !request.evidence.contains("ZZ-0000") }));
}

#[test]
fn grounded_values_pass_only_when_the_judge_agrees() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![
        verdict("grounded", 0.95, "present"),
        verdict("grounded", 0.96, "present"),
    ]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        grounded("output_grounded.json"),
        fixture.context(),
        judge.clone(),
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Pass, "{:?}", result.message);
    let judged: Vec<_> = result
        .evidence
        .iter()
        .filter_map(|item| item.semantic.as_ref())
        .collect();
    assert_eq!(judged.len(), 2);
    assert_eq!(judged[0].pointers, vec!["/name".to_owned()]);
    assert_eq!(judged[1].pointers, vec!["/reference".to_owned()]);
    assert!(judged.iter().all(|item| item.unsupported.is_empty()));
}

#[test]
fn grounded_judge_disagreement_fails() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![
        verdict("grounded", 0.95, "present"),
        verdict("unsupported", 0.95, "contradicted"),
    ]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        grounded("output_grounded.json"),
        fixture.context(),
        judge,
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("/reference"));
}

#[test]
fn grounded_ambiguity_is_inconclusive() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![
        verdict("grounded", 0.95, "present"),
        verdict("ambiguous", 0.95, "unclear"),
    ]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        grounded("output_grounded.json"),
        fixture.context(),
        judge,
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.message.contains("/reference"));
}

// ------------------------------------------------------------------ visual

#[test]
fn visual_chain_is_one_provenance_line() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(vec![
        Reply::Vision("A settings dialog with a Save button and no error banner."),
        verdict("supported", 0.93, "the description states it"),
    ]);
    let spec = CheckSpec::new(
        "vl_criterion",
        [
            (
                "criterion",
                json!("the screenshot shows a Save button and no error banner"),
            ),
            ("image", json!("image")),
        ],
    );
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        spec,
        fixture.context(),
        judge.clone(),
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Pass, "{:?}", result.message);
    let judged: Vec<_> = result
        .evidence
        .iter()
        .filter_map(|item| item.semantic.as_ref())
        .collect();
    assert_eq!(judged.len(), 2);
    assert_eq!(judged[0].chain, "vl->d1");
    assert_eq!(judged[0].model, "fake-vl");
    assert_eq!(judged[0].label, "observed");
    assert_eq!(judged[1].chain, "vl->d1");
    assert_eq!(judged[1].label, "supported");
    assert_eq!(judged[1].confidence_bps, 9_300);
    let request = &judge.requests()[0];
    assert!(request
        .image
        .as_deref()
        .is_some_and(|url| url.starts_with("data:image/png;base64,")));
}

#[test]
fn bytes_that_are_not_an_image_are_inconclusive() {
    let fixture = Fixture::new();
    let image = fixture.inputs.get("image").expect("image input");
    fs::write(
        fixture.artifacts.path_for(&image.sha256),
        b"this is not an image at all, it only claims to be one",
    )
    .expect("overwrite artifact");
    let judge = ScriptedJudge::new(vec![Reply::Vision("nothing")]);
    let spec = CheckSpec::new(
        "vl_criterion",
        [
            ("criterion", json!("the screenshot shows a Save button")),
            ("image", json!("image")),
        ],
    );
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        spec,
        fixture.context(),
        judge.clone(),
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.message.contains("do not match"));
    assert!(judge.requests().is_empty());
}

#[test]
fn unknown_semantic_kind_is_an_error_not_a_pass() {
    let fixture = Fixture::new();
    let judge = ScriptedJudge::new(Vec::new());
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(evaluate_semantic(
        CheckSpec::new("d1_magic", [("criterion", json!("anything"))]),
        fixture.context(),
        judge,
        automatic(),
    ));
    assert_eq!(result.status, CheckStatus::Error);
    assert!(is_semantic_kind("d1_noul"));
    assert!(!is_semantic_kind("file_exists"));
}

// ------------------------------------------------------------ mission gates

#[test]
fn packaged_semantic_missions_stay_human_review_only() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../missions");
    let missions = load_missions(&directory).expect("packaged missions load");
    for name in ["screen_qa", "voice_intake", "doc_extract"] {
        let mission = missions.get(name).expect("mission present");
        let policy = mission.semantic_policy().expect("semantic policy");
        assert_eq!(policy.mode, SemanticMode::HumanReviewOnly);
        let reason = mission
            .semantic_unavailable_reason()
            .expect("human review is announced");
        assert!(!reason.trim().is_empty());
    }
    assert!(missions["desk_task"].semantic_policy().is_none());
    assert!(missions["desk_task"]
        .semantic_unavailable_reason()
        .is_none());
}

#[test]
fn semantic_manifest_without_policy_is_rejected() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../missions");
    let raw = fs::read_to_string(directory.join("screen_qa.json")).expect("read mission");
    let mut value: Value = serde_json::from_str(&raw).expect("parse mission");
    value.as_object_mut().expect("object").remove("semantic");
    let error = MissionManifest::from_json(&serde_json::to_string(&value).expect("serialize"))
        .expect_err("a semantic mission needs a policy");
    assert!(error.to_string().contains("semantic policy"));
}

#[test]
fn automatic_policy_without_qualification_is_rejected() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../missions");
    let raw = fs::read_to_string(directory.join("screen_qa.json")).expect("read mission");
    let mut value: Value = serde_json::from_str(&raw).expect("parse mission");
    value["semantic"] = json!({
        "mode": "automatic",
        "min_confidence_bps": 8000
    });
    let error = MissionManifest::from_json(&serde_json::to_string(&value).expect("serialize"))
        .expect_err("automatic passing needs a qualification");
    assert!(error.to_string().contains("qualification"));
}

#[test]
fn grounded_check_must_name_a_declared_source() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../missions");
    let raw = fs::read_to_string(directory.join("voice_intake.json")).expect("read mission");
    let mission = MissionManifest::from_json(&raw).expect("mission parses");
    let plan = lfm3_harness::PlanDraft {
        steps: vec![PlannedStep {
            index: 1,
            title: "Extract fields".to_owned(),
            criterion: "the JSON records the caller's reference".to_owned(),
            checks: vec![CheckSpec::new(
                "d1_grounded",
                [
                    ("path", json!("out.json")),
                    ("source", json!("not_declared")),
                    ("fields", json!({"/reference": "reference"})),
                ],
            )],
            tool: Some("write_file".to_owned()),
        }],
    };
    let error = mission
        .validate_plan(&plan)
        .expect_err("undeclared sources are rejected");
    assert!(error.to_string().contains("not_declared"));
}

// ------------------------------------------------------------ runner gate

struct NoopDriver;

impl StepDriver for NoopDriver {
    fn model(&self) -> String {
        "fake-model".to_owned()
    }

    fn execute<'a>(
        &'a self,
        _request: StepRequest<'a>,
        _cancel: &'a lfm3_harness::CancellationChild,
    ) -> BoxFuture<'a, Result<StepExecution, DriverError>> {
        Box::pin(async move {
            Ok(StepExecution {
                model: self.model(),
                action: json!({"tool": "write_file"}),
                tool_results: Vec::new(),
                command_runs: Vec::new(),
            })
        })
    }
}

fn runner_acceptance() -> FrozenAcceptance {
    AcceptanceSpec {
        schema_version: 1,
        mission: "semantic_runner_fixture".to_owned(),
        mission_version: 1,
        goal: "Record the caller's reference without inventing details".to_owned(),
        inputs: BTreeMap::new(),
        steps: vec![PlannedStep {
            index: 1,
            title: "Write the note and judge it".to_owned(),
            criterion: "the note exists and states the caller's reference".to_owned(),
            checks: vec![
                CheckSpec::new("file_exists", [("path", json!("missing.txt"))]),
                CheckSpec::new(
                    "numeric_consistency",
                    [
                        ("path", json!("totals.json")),
                        ("total", json!("total")),
                        ("sum", json!(["parts"])),
                    ],
                ),
                CheckSpec::new(
                    "d1_noul",
                    [
                        ("path", json!("notes.md")),
                        (
                            "criterion",
                            json!("the note records the caller's reference AX-4471"),
                        ),
                    ],
                ),
            ],
            tool: None,
        }],
        output: OutputContract::default(),
        allowed_tools: Default::default(),
        allowed_checks: ["file_exists", "numeric_consistency", "d1_noul"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        max_steps: 8,
        max_attempts: 1,
    }
    .freeze()
    .expect("fixture acceptance")
}

#[tokio::test]
async fn deterministic_failure_wins_over_a_semantic_pass() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(
        workspace.join("notes.md"),
        "The caller is Dana Whitfield with reference AX-4471.\n",
    )
    .expect("note");
    fs::copy(
        fixture_root().join("output_wrong_total.json"),
        workspace.join("totals.json"),
    )
    .expect("wrong total fixture");
    let store = Store::open(directory.path().join("runs.db"))
        .await
        .expect("store");
    let acceptance = runner_acceptance();
    let run = store
        .create_run(&acceptance, None)
        .await
        .expect("run created");
    let judge = ScriptedJudge::new(vec![verdict("supported", 0.99, "the note states it")]);
    let verifier = Arc::new(SemanticVerifier::new(automatic(), judge.clone()));
    let runner = Runner::new(
        store.clone(),
        acceptance,
        workspace,
        Arc::new(NoopDriver),
        CancellationContext::new(Duration::from_secs(30)),
    )
    .expect("runner")
    .with_verifier(verifier)
    .with_budget(lfm3_harness::agent::Budget {
        attempts_per_strategy: 1,
        strategies: 1,
        total_attempts: 1,
    });

    let outcome = runner.execute(&run.id).await.expect("execute");
    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(
        outcome.reason,
        Some(IncompleteReason::StepBudgetExhausted { step_index: 1 })
    );
    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_ne!(snapshot.steps[0].status, StepStatus::Passed);
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Failed);
    assert_eq!(judge.requests().len(), 1, "the semantic check did run");

    // The durable event must carry the same deciding provenance the UI needs
    // to label the verdict as advisory rather than promote the message.
    let events = store.events_after(&run.id, 0).await.expect("events");
    let semantic_event = events
        .iter()
        .find(|event| {
            event.kind == "check.result"
                && event.payload.get("semantic") == Some(&Value::Bool(true))
        })
        .expect("semantic check event");
    assert_eq!(
        semantic_event.payload.get("semantic_chain"),
        Some(&Value::String("d1".to_owned()))
    );
    assert_eq!(
        semantic_event.payload.get("semantic_label"),
        Some(&Value::String("supported".to_owned()))
    );
    assert_eq!(
        semantic_event.payload.get("confidence_bps"),
        Some(&json!(9900))
    );
    assert_eq!(
        semantic_event.payload.get("threshold_bps"),
        Some(&json!(8000))
    );
}
