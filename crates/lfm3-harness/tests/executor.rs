//! Executor protocol gates.
//!
//! The executor asks the local model for exactly one tool call, validates it
//! against the frozen acceptance and the workspace grants before any side
//! effect, and turns everything else into a typed failure. A valid action is
//! executed through the real tool registry, so these tests drive complete
//! fake-model runs without a scripted production executor.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use lfm3_harness::{
    agent::{FailureClass, FailurePacket, RemainingBudget},
    model::{
        AssistantMessage, ChatChoice, ChatRequest, ChatResponse, Decision, DecisionRequest,
        FunctionCall, ModelError, ModelFuture, ModelGateway, ToolCall, TranscribeRequest,
        Transcript,
    },
    verify::FileManifest,
    AcceptanceSpec, AttemptId, AttemptStatus, CancelReason, CancellationChild, CancellationContext,
    CheckSpec, DriverError, Executor, FrozenAcceptance, IncompleteReason, OutputContract,
    PlannedStep, RequiredArtifact, RunId, RunStatus, Runner, StepDriver, StepRequest, Store,
    ToolName, ToolRegistry, ToolStatus,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- fixtures

fn base_spec() -> AcceptanceSpec {
    AcceptanceSpec {
        schema_version: 1,
        mission: "executor_fixture".to_owned(),
        mission_version: 1,
        goal: "Write the verified note".to_owned(),
        inputs: Default::default(),
        steps: vec![PlannedStep {
            index: 1,
            title: "Write notes.md".to_owned(),
            criterion: "notes.md contains the verified note".to_owned(),
            checks: vec![CheckSpec::new(
                "content_matches",
                [("path", json!("notes.md")), ("contains", json!("verified"))],
            )],
            tool: Some("write_file".to_owned()),
        }],
        output: OutputContract {
            schema: None,
            required_artifacts: vec![RequiredArtifact {
                path: "notes.md".to_owned(),
                non_empty: true,
            }],
        },
        allowed_tools: ["read_file".to_owned(), "write_file".to_owned()]
            .into_iter()
            .collect(),
        allowed_checks: ["content_matches".to_owned()].into_iter().collect(),
        max_steps: 4,
        max_attempts: 2,
    }
}

fn frozen_spec(spec: AcceptanceSpec) -> FrozenAcceptance {
    spec.freeze().expect("fixture acceptance is valid")
}

fn registry(workspace: &Path) -> Arc<ToolRegistry> {
    Arc::new(
        ToolRegistry::new(workspace)
            .expect("tool registry")
            .grant(ToolName::WriteFile)
            .grant(ToolName::ReadFile),
    )
}

fn test_cancel() -> CancellationContext {
    CancellationContext::new(Duration::from_secs(30))
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        kind: "function".to_owned(),
        function: FunctionCall {
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        },
    }
}

fn calls_reply(calls: Vec<ToolCall>) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse {
        id: "fake-response".to_owned(),
        model: "fake-model".to_owned(),
        choices: vec![ChatChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant".to_owned(),
                content: None,
                reasoning_content: None,
                tool_calls: calls,
            },
            finish_reason: Some("tool_calls".to_owned()),
        }],
        usage: None,
    })
}

fn tool_reply(name: &str, arguments: Value) -> Result<ChatResponse, ModelError> {
    calls_reply(vec![call("call-1", name, &arguments.to_string())])
}

fn text_reply(content: &str) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse {
        id: "fake-response".to_owned(),
        model: "fake-model".to_owned(),
        choices: vec![ChatChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant".to_owned(),
                content: Some(content.to_owned()),
                reasoning_content: None,
                tool_calls: Vec::new(),
            },
            finish_reason: Some("stop".to_owned()),
        }],
        usage: None,
    })
}

#[derive(Clone)]
struct FakeModelGateway {
    replies: Arc<Mutex<VecDeque<Result<ChatResponse, ModelError>>>>,
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl FakeModelGateway {
    fn new(replies: Vec<Result<ChatResponse, ModelError>>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
            calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl ModelGateway for FakeModelGateway {
    fn chat(
        &self,
        request: ChatRequest,
        _cancel: CancellationChild,
    ) -> ModelFuture<'_, ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().expect("requests lock").push(request);
        let reply = self.replies.lock().expect("replies lock").pop_front();
        Box::pin(async move {
            reply.unwrap_or_else(|| {
                Err(ModelError::Transport(
                    "fake gateway script is exhausted".to_owned(),
                ))
            })
        })
    }

    fn transcribe(
        &self,
        _request: TranscribeRequest,
        _cancel: CancellationChild,
    ) -> ModelFuture<'_, Transcript> {
        Box::pin(async {
            Err(ModelError::Transport(
                "transcribe is not scripted".to_owned(),
            ))
        })
    }

    fn decide(
        &self,
        _request: DecisionRequest,
        _cancel: CancellationChild,
    ) -> ModelFuture<'_, Decision> {
        Box::pin(async { Err(ModelError::Transport("decide is not scripted".to_owned())) })
    }
}

fn fixture_request<'a>(
    run_id: &'a RunId,
    acceptance: &'a FrozenAcceptance,
    attempt: &'a AttemptId,
    before: &'a FileManifest,
    packet: Option<&'a FailurePacket>,
) -> StepRequest<'a> {
    StepRequest {
        run_id,
        acceptance: acceptance.spec(),
        step: &acceptance.spec().steps[0],
        attempt,
        attempt_number: 1,
        strategy: 1,
        before,
        prior_failures: &[],
        packet,
        history: &[],
    }
}

fn packet(acceptance: &FrozenAcceptance) -> FailurePacket {
    FailurePacket {
        acceptance_hash: acceptance.hash().to_owned(),
        step_index: 1,
        criterion: "notes.md contains the verified note".to_owned(),
        strategy: 1,
        class: FailureClass::InvalidAction,
        action: "driver:invalid".to_owned(),
        signature: "invalid_action|no-evidence".to_owned(),
        message: "the model returned no tool call".to_owned(),
        failed_checks: Vec::new(),
        evidence: Vec::new(),
        prior_signatures: Vec::new(),
        remaining: RemainingBudget {
            attempts_left_in_strategy: 1,
            strategies_left: 1,
            total_attempts_left: 2,
        },
    }
}

fn strategy_json(hash: &str) -> String {
    json!({
        "acceptance_hash": hash,
        "actions": [{
            "step_index": 1,
            "tool": "write_file",
            "args": {"path": "notes.md"},
        }],
    })
    .to_string()
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn one_valid_action_runs_the_real_tool_and_returns_evidence() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway = FakeModelGateway::new(vec![tool_reply(
        "write_file",
        json!({"path": "notes.md", "content": "verified note"}),
    )]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let execution = StepDriver::execute(&executor, request, &child)
        .await
        .expect("one valid tool action executes");

    assert_eq!(
        std::fs::read_to_string(workspace.join("notes.md")).expect("file"),
        "verified note"
    );
    assert_eq!(execution.model, "fake-model");
    assert_eq!(execution.action["tool"], json!("write_file"));
    assert_eq!(execution.action["call_id"], json!("call-1"));
    assert_eq!(execution.tool_results.len(), 1);
    assert_eq!(execution.tool_results[0].status, ToolStatus::Succeeded);
    assert_eq!(execution.tool_results[0].attempt, attempt);
    assert_eq!(execution.command_runs.len(), 0);
    assert_eq!(gateway.calls(), 1);

    let requests = gateway.requests();
    let request = &requests[0];
    assert_eq!(request.tool_choice.as_deref(), Some("required"));
    assert_eq!(request.temperature, 0.1);
    let names: Vec<&str> = request
        .tools
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect();
    assert_eq!(names, vec!["read_file", "write_file"]);
}

#[tokio::test]
async fn a_reply_without_a_tool_call_is_invalid() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway = FakeModelGateway::new(vec![text_reply("I will write the file now.")]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("text is not an action");

    match error {
        DriverError::InvalidAction(message) => assert!(message.contains("no tool call")),
        other => panic!("expected an invalid action, got {other:?}"),
    }
    assert!(!workspace.join("notes.md").exists());
}

#[tokio::test]
async fn two_tool_calls_are_invalid() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway = FakeModelGateway::new(vec![calls_reply(vec![
        call(
            "call-1",
            "write_file",
            &json!({"path": "one.md", "content": "a"}).to_string(),
        ),
        call(
            "call-2",
            "write_file",
            &json!({"path": "two.md", "content": "b"}).to_string(),
        ),
    ])]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("two actions cannot be accepted");

    match error {
        DriverError::InvalidAction(message) => assert!(message.contains("exactly one")),
        other => panic!("expected an invalid action, got {other:?}"),
    }
    assert!(!workspace.join("one.md").exists());
    assert!(!workspace.join("two.md").exists());
}

#[tokio::test]
async fn a_tool_outside_the_acceptance_is_invalid() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut spec = base_spec();
    spec.steps[0].tool = Some("read_file".to_owned());
    spec.allowed_tools = ["read_file".to_owned()].into_iter().collect();
    let frozen = frozen_spec(spec);
    let gateway = FakeModelGateway::new(vec![tool_reply(
        "write_file",
        json!({"path": "notes.md", "content": "verified note"}),
    )]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("an ungranted tool is rejected before it runs");

    match error {
        DriverError::InvalidAction(message) => {
            assert!(message.contains("not granted by this mission"));
        }
        other => panic!("expected an invalid action, got {other:?}"),
    }
    assert!(!workspace.join("notes.md").exists());
}

#[tokio::test]
async fn a_tool_missing_from_the_workspace_grant_is_invalid() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut spec = base_spec();
    spec.steps[0].tool = None;
    spec.allowed_tools = ["list_files".to_owned(), "write_file".to_owned()]
        .into_iter()
        .collect();
    let frozen = frozen_spec(spec);
    let gateway = FakeModelGateway::new(vec![tool_reply("list_files", json!({}))]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("the workspace grants only read and write");

    match error {
        DriverError::InvalidAction(message) => {
            assert!(message.contains("not installed in this workspace"));
        }
        other => panic!("expected an invalid action, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unknown_tool_name_is_invalid() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway = FakeModelGateway::new(vec![tool_reply("delete_everything", json!({}))]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("unknown tools are rejected");

    assert!(matches!(error, DriverError::InvalidAction(_)));
}

#[tokio::test]
async fn malformed_arguments_are_invalid() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway = FakeModelGateway::new(vec![calls_reply(vec![call(
        "call-1",
        "write_file",
        "{not json",
    )])]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("malformed JSON arguments are rejected");

    match error {
        DriverError::InvalidAction(message) => assert!(message.contains("not JSON")),
        other => panic!("expected an invalid action, got {other:?}"),
    }
}

#[tokio::test]
async fn non_object_arguments_are_invalid() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway =
        FakeModelGateway::new(vec![calls_reply(vec![call("call-1", "write_file", "[]")])]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("arguments must be a JSON object");

    match error {
        DriverError::InvalidAction(message) => assert!(message.contains("JSON object")),
        other => panic!("expected an invalid action, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_failures_become_invalid_actions() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway = FakeModelGateway::new(vec![tool_reply(
        "read_file",
        json!({"path": "missing.txt"}),
    )]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = StepDriver::execute(&executor, request, &child)
        .await
        .expect_err("reading a missing file cannot succeed");

    match error {
        DriverError::InvalidAction(message) => assert!(message.contains("does not exist")),
        other => panic!("expected an invalid action, got {other:?}"),
    }
}

#[tokio::test]
async fn transport_failures_are_transient_driver_failures() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway =
        FakeModelGateway::new(vec![Err(ModelError::Transport("socket closed".to_owned()))]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("a dead endpoint cannot act");

    match error {
        DriverError::Failed(message) => assert!(message.contains("transport")),
        other => panic!("expected a transient failure, got {other:?}"),
    }
}

#[tokio::test]
async fn cancellation_before_the_call_never_reaches_the_model() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway = FakeModelGateway::new(vec![tool_reply(
        "write_file",
        json!({"path": "notes.md", "content": "verified note"}),
    )]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    cancel.cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .next_action(&request, &child)
        .await
        .expect_err("a cancelled run never acts");

    assert!(matches!(
        error,
        DriverError::Cancelled(CancelReason::Requested)
    ));
    assert_eq!(gateway.calls(), 0);
    assert!(!workspace.join("notes.md").exists());
}

#[tokio::test]
async fn replanning_needs_a_failure_packet() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let gateway = FakeModelGateway::new(vec![text_reply("{}")]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let request = fixture_request(&run_id, &frozen, &attempt, &before, None);

    let error = executor
        .replan_strategy(&request, &child)
        .await
        .expect_err("the first attempt cannot replan");

    match error {
        DriverError::InvalidAction(message) => assert!(message.contains("failure packet")),
        other => panic!("expected an invalid action, got {other:?}"),
    }
    assert_eq!(gateway.calls(), 0);
}

#[tokio::test]
async fn a_replan_that_changes_the_frozen_hash_is_invalid() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let bad_hash = "0".repeat(64);
    let strategy = strategy_json(&bad_hash);
    let gateway = FakeModelGateway::new(vec![text_reply(&strategy)]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let failure = packet(&frozen);
    let request = fixture_request(&run_id, &frozen, &attempt, &before, Some(&failure));

    let error = executor
        .replan_strategy(&request, &child)
        .await
        .expect_err("a replacement strategy cannot redefine the contract");

    match error {
        DriverError::InvalidAction(message) => {
            assert!(message.contains("frozen acceptance hash"));
        }
        other => panic!("expected an invalid action, got {other:?}"),
    }
}

#[tokio::test]
async fn a_replan_with_the_frozen_hash_is_accepted() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let frozen = frozen_spec(base_spec());
    let strategy = json!({
        "acceptance_hash": frozen.hash(),
        "actions": [{
            "step_index": 1,
            "tool": "write_file",
            "args": {"path": "notes.md"},
        }],
    })
    .to_string();
    let gateway = FakeModelGateway::new(vec![text_reply(&strategy)]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let cancel = test_cancel();
    let child = cancel.child();
    let run_id = RunId::new();
    let attempt = AttemptId::new();
    let before = FileManifest::capture(&workspace).expect("manifest");
    let failure = packet(&frozen);
    let request = fixture_request(&run_id, &frozen, &attempt, &before, Some(&failure));

    let proposed = executor
        .replan_strategy(&request, &child)
        .await
        .expect("the same contract with different actions is accepted");

    assert_eq!(proposed.acceptance_hash, frozen.hash());
    assert_eq!(proposed.actions.len(), 1);
    assert_eq!(proposed.actions[0].step_index, 1);
    assert_eq!(proposed.actions[0].tool, "write_file");
}

// ------------------------------------------------------- fake-model runs

async fn open_store(directory: &Path) -> Store {
    Store::open(directory.join("runs.db"))
        .await
        .expect("store opens")
}

fn workspace(directory: &Path) -> PathBuf {
    let workspace = directory.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    workspace
}

#[tokio::test]
async fn a_fake_model_run_passes_without_a_scripted_executor() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = workspace(directory.path());
    let store = open_store(directory.path()).await;
    let frozen = frozen_spec(base_spec());
    let run = store.create_run(&frozen, None).await.expect("run created");
    let gateway = FakeModelGateway::new(vec![tool_reply(
        "write_file",
        json!({"path": "notes.md", "content": "verified note"}),
    )]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let runner = Runner::new(
        store.clone(),
        frozen,
        &workspace,
        Arc::new(executor),
        test_cancel(),
    )
    .expect("runner");

    let outcome = runner.execute(&run.id).await.expect("execute");

    assert_eq!(outcome.status, RunStatus::Passed);
    assert_eq!(outcome.reason, None);
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes.md")).expect("notes"),
        "verified note"
    );
    assert_eq!(gateway.calls(), 1);

    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.steps.len(), 1);
    assert_eq!(snapshot.steps[0].attempts.len(), 1);
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Passed);
}

#[tokio::test]
async fn invalid_actions_retry_once_then_stop_without_a_false_pass() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = workspace(directory.path());
    let store = open_store(directory.path()).await;
    let frozen = frozen_spec(base_spec());
    let run = store.create_run(&frozen, None).await.expect("run created");
    let gateway = FakeModelGateway::new(vec![
        text_reply("I would write the file."),
        text_reply("Writing the file now."),
    ]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let runner = Runner::new(
        store.clone(),
        frozen,
        &workspace,
        Arc::new(executor),
        test_cancel(),
    )
    .expect("runner");

    let outcome = runner.execute(&run.id).await.expect("execute");

    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(
        outcome.reason,
        Some(IncompleteReason::StepBudgetExhausted { step_index: 1 })
    );
    assert_eq!(gateway.calls(), 2);
    assert!(!workspace.join("notes.md").exists());

    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.steps[0].attempts.len(), 2);
    for attempt in &snapshot.steps[0].attempts {
        assert_eq!(attempt.status, AttemptStatus::Failed);
        assert!(attempt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("tool call")));
    }
}

#[tokio::test]
async fn a_failed_check_retries_with_the_failure_packet() {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = workspace(directory.path());
    let store = open_store(directory.path()).await;
    let frozen = frozen_spec(base_spec());
    let run = store.create_run(&frozen, None).await.expect("run created");
    let gateway = FakeModelGateway::new(vec![
        tool_reply(
            "write_file",
            json!({"path": "notes.md", "content": "an unconfirmed draft"}),
        ),
        tool_reply(
            "write_file",
            json!({"path": "notes.md", "content": "verified note"}),
        ),
    ]);
    let executor = Executor::new(
        Arc::new(gateway.clone()),
        registry(&workspace),
        "fake-model",
    );
    let runner = Runner::new(
        store.clone(),
        frozen,
        &workspace,
        Arc::new(executor),
        test_cancel(),
    )
    .expect("runner");

    let outcome = runner.execute(&run.id).await.expect("execute");

    assert_eq!(outcome.status, RunStatus::Passed);
    assert_eq!(gateway.calls(), 2);
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes.md")).expect("notes"),
        "verified note"
    );

    let requests = gateway.requests();
    let retry = &requests[1];
    let retry_brief = match &retry.messages[1].content {
        lfm3_harness::model::MessageContent::Text(text) => text.clone(),
        lfm3_harness::model::MessageContent::Parts(_) => panic!("expected text"),
    };
    assert!(retry_brief.contains("Failure:"));
    assert!(retry_brief.contains("content_matches"));

    let snapshot = store.snapshot(&run.id).await.expect("snapshot");
    assert_eq!(snapshot.steps[0].attempts.len(), 2);
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Failed);
    assert_eq!(snapshot.steps[0].attempts[1].status, AttemptStatus::Passed);
    assert_eq!(snapshot.steps[0].attempts[1].strategy, 1);
}
