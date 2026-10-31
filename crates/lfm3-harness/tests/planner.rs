//! Planner protocol gates.
//!
//! The planner must produce exactly one validated plan from a constrained
//! brief, repair a rejected reply exactly once, and fail closed on prose,
//! several JSON values, unknown fields, vague criteria, or transport errors.
//! No reply may smuggle in a tool or check the mission did not grant.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use lfm3_harness::{
    agent::{decode_single_json, plan_request, PlanRequest, Planner, PlannerError},
    model::{
        AssistantMessage, ChatChoice, ChatRequest, ChatResponse, ChatRole, Decision,
        DecisionRequest, MessageContent, ModelError, ModelFuture, ModelGateway, TranscribeRequest,
        Transcript,
    },
    CancelReason, CancellationContext, MissionManifest, PlanError,
};

// ---------------------------------------------------------------- fixtures

const MISSION: &str = r#"{
  "schema_version": 1,
  "name": "desk_note",
  "version": 1,
  "title": "Desk note",
  "description": "Write one verified note file from the supplied source note.",
  "inputs": {
    "source": {"kind": "string", "required": true}
  },
  "tools": ["read_file", "write_file"],
  "checks": ["file_exists", "content_matches"],
  "output": {
    "required_artifacts": [{"path": "notes.md", "non_empty": true}]
  },
  "limits": {"max_steps": 4, "max_attempts": 3}
}"#;

const VALID_PLAN: &str = r#"{
  "steps": [
    {
      "index": 1,
      "title": "Write notes.md",
      "criterion": "notes.md exists and contains the source terms",
      "tool": "write_file",
      "checks": [
        {"kind": "content_matches", "path": "notes.md", "contains": "alpha"},
        {"kind": "file_exists", "path": "notes.md"}
      ]
    }
  ]
}"#;

const EMPTY_PLAN: &str = r#"{"steps": []}"#;

fn mission() -> MissionManifest {
    MissionManifest::from_json(MISSION).expect("fixture manifest is valid")
}

fn plan_request_fixture() -> PlanRequest {
    plan_request(
        mission(),
        "Write the note from the source",
        BTreeMap::from([("source".to_owned(), "The source note says alpha".to_owned())]),
        std::env::temp_dir().join("lfm3-planner-workspace"),
    )
}

fn test_cancel() -> CancellationContext {
    CancellationContext::new(Duration::from_secs(10))
}

fn reply(content: impl Into<String>) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse {
        id: "fake-response".to_owned(),
        model: "fake-model".to_owned(),
        choices: vec![ChatChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant".to_owned(),
                content: Some(content.into()),
                reasoning_content: None,
                tool_calls: Vec::new(),
            },
            finish_reason: Some("stop".to_owned()),
        }],
        usage: None,
    })
}

fn transport(message: &str) -> Result<ChatResponse, ModelError> {
    Err(ModelError::Transport(message.to_owned()))
}

/// Scripted gateway that records every request and pops one reply per call.
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
        _cancel: lfm3_harness::CancellationChild,
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
        _cancel: lfm3_harness::CancellationChild,
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
        _cancel: lfm3_harness::CancellationChild,
    ) -> ModelFuture<'_, Decision> {
        Box::pin(async { Err(ModelError::Transport("decide is not scripted".to_owned())) })
    }
}

fn text_of(message: &lfm3_harness::model::ChatMessage) -> String {
    match &message.content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(_) => panic!("expected a text message"),
    }
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn invalid_plan_receives_exactly_one_repair() {
    let gateway = FakeModelGateway::new(vec![reply(EMPTY_PLAN), reply(VALID_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let plan = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect("the repaired plan is accepted");

    assert_eq!(gateway.calls(), 2);
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].index, 1);
    assert_eq!(plan.steps[0].checks.len(), 2);

    let requests = gateway.requests();
    assert_eq!(requests[0].messages.len(), 2);
    let repair = &requests[1];
    assert_eq!(repair.messages.len(), 4);
    assert_eq!(repair.messages[0].role, ChatRole::System);
    assert_eq!(repair.messages[1].role, ChatRole::User);
    assert_eq!(repair.messages[2].role, ChatRole::Assistant);
    assert_eq!(text_of(&repair.messages[2]), EMPTY_PLAN);
    assert_eq!(repair.messages[3].role, ChatRole::User);
    assert!(text_of(&repair.messages[3]).contains("rejected by the validator"));
}

#[tokio::test]
async fn a_plan_that_stays_invalid_after_one_repair_is_rejected() {
    let gateway = FakeModelGateway::new(vec![reply(EMPTY_PLAN), reply(EMPTY_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let error = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect_err("two invalid plans cannot be accepted");

    assert!(matches!(error, PlannerError::RepairExhausted(_)));
    assert_eq!(gateway.calls(), 2);
}

#[tokio::test]
async fn a_fenced_plan_is_accepted_without_repair() {
    let gateway = FakeModelGateway::new(vec![reply(format!("```json\n{VALID_PLAN}\n```"))]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let plan = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect("one fenced object is fine");

    assert_eq!(gateway.calls(), 1);
    assert_eq!(plan.steps.len(), 1);
}

#[tokio::test]
async fn a_single_step_output_contract_forces_a_write_tool_in_the_schema() {
    let gateway = FakeModelGateway::new(vec![reply(VALID_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();
    let mut mission = mission();
    mission.limits.max_steps = 1;
    let request = plan_request(
        mission,
        "Write the note from the source",
        BTreeMap::from([("source".to_owned(), "The source note says alpha".to_owned())]),
        std::env::temp_dir().join("lfm3-planner-workspace"),
    );

    planner.draft(request, &cancel).await.expect("plan");

    let requests = gateway.requests();
    let schema = requests[0]
        .response_format
        .as_ref()
        .and_then(|format| format.json_schema.as_ref())
        .map(|format| format.schema.clone())
        .expect("planning uses a JSON schema");
    let step = &schema["properties"]["steps"]["items"];
    assert_eq!(
        step["properties"]["tool"]["enum"],
        serde_json::json!(["write_file"])
    );
    assert!(step["required"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("tool")));
}

#[tokio::test]
async fn an_unknown_plan_field_is_rejected_and_repaired() {
    let with_extra = r#"{
      "steps": [
        {
          "index": 1,
          "title": "Write notes.md",
          "criterion": "notes.md exists and contains the source terms",
          "tool": "write_file",
          "rationale": "the model explaining itself",
          "checks": [{"kind": "file_exists", "path": "notes.md"}]
        }
      ]
    }"#;
    let gateway = FakeModelGateway::new(vec![reply(with_extra), reply(VALID_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let plan = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect("the repaired plan is accepted");

    assert_eq!(gateway.calls(), 2);
    assert_eq!(plan.steps.len(), 1);
}

#[tokio::test]
async fn a_vague_criterion_is_rejected_and_repaired() {
    let vague = r#"{
      "steps": [
        {
          "index": 1,
          "title": "Write",
          "criterion": "do it",
          "tool": "write_file",
          "checks": [{"kind": "file_exists", "path": "notes.md"}]
        }
      ]
    }"#;
    let gateway = FakeModelGateway::new(vec![reply(vague), reply(VALID_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let plan = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect("the repaired plan is accepted");

    assert_eq!(gateway.calls(), 2);
    assert!(plan.steps[0].criterion.chars().count() >= 10);
}

#[tokio::test]
async fn a_check_outside_the_mission_is_repaired_once() {
    let outside = r#"{
      "steps": [
        {
          "index": 1,
          "title": "Write notes.md",
          "criterion": "the vision judge confirms notes.md",
          "tool": "write_file",
          "checks": [{"kind": "vl_criterion", "criterion": "looks right"}]
        }
      ]
    }"#;
    let gateway = FakeModelGateway::new(vec![reply(outside), reply(VALID_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let plan = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect("the repaired plan is accepted");

    assert_eq!(gateway.calls(), 2);
    assert_eq!(plan.steps[0].checks[0].kind, "content_matches");
}

#[tokio::test]
async fn transport_errors_surface_without_a_repair() {
    let gateway = FakeModelGateway::new(vec![transport("connection reset")]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let error = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect_err("a dead endpoint cannot produce a plan");

    assert!(matches!(
        error,
        PlannerError::Model(ModelError::Transport(_))
    ));
    assert_eq!(gateway.calls(), 1);
}

#[tokio::test]
async fn blank_content_is_not_a_plan_and_is_not_repaired() {
    let gateway = FakeModelGateway::new(vec![reply("   ")]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let error = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect_err("empty content is a hard failure");

    assert!(matches!(error, PlannerError::Empty));
    assert_eq!(gateway.calls(), 1);
}

#[tokio::test]
async fn an_expired_context_never_calls_the_model() {
    let gateway = FakeModelGateway::new(vec![reply(VALID_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = CancellationContext::new(Duration::ZERO);

    let error = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect_err("an expired deadline stops planning");

    assert!(matches!(
        error,
        PlannerError::Cancelled(CancelReason::Deadline)
    ));
    assert_eq!(gateway.calls(), 0);
}

#[tokio::test]
async fn a_cancelled_context_never_calls_the_model() {
    let gateway = FakeModelGateway::new(vec![reply(VALID_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();
    cancel.cancel();

    let error = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect_err("a cancelled run stops planning");

    assert!(matches!(
        error,
        PlannerError::Cancelled(CancelReason::Requested)
    ));
    assert_eq!(gateway.calls(), 0);
}

#[tokio::test]
async fn planning_is_deterministic_and_schema_constrained() {
    let gateway = FakeModelGateway::new(vec![reply(VALID_PLAN)]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect("plan");

    let requests = gateway.requests();
    let request = &requests[0];
    assert_eq!(request.temperature, 0.0);
    assert_eq!(
        request.response_format.as_ref().map(|f| f.kind.as_str()),
        Some("json_schema")
    );
    assert!(request
        .response_format
        .as_ref()
        .and_then(|format| format.json_schema.as_ref())
        .is_some());
    assert!(request.tools.is_empty());
    assert!(request.tool_choice.is_none());

    let system = text_of(&request.messages[0]);
    for expected in [
        "write_file",
        "read_file",
        "file_exists",
        "content_matches",
        "notes.md",
    ] {
        assert!(system.contains(expected), "brief is missing {expected:?}");
    }
    let user = text_of(&request.messages[1]);
    assert!(user.contains("Write the note from the source"));
    assert!(user.contains("the source note text") || user.contains("alpha"));
    assert!(user.contains("Mission JSON name: desk_note"));
}

#[test]
fn one_complete_json_value_is_required() {
    assert!(decode_single_json("{\"steps\":[]}").is_ok());
    assert!(decode_single_json("```json\n{\"steps\":[]}\n```").is_ok());

    let prose = decode_single_json("Here is the plan: {\"steps\":[]}");
    assert!(matches!(prose, Err(PlannerError::Decode(_))));

    let two_values = decode_single_json("{\"steps\":[]}{\"steps\":[]}");
    assert!(matches!(two_values, Err(PlannerError::Decode(_))));

    let trailing = decode_single_json("```json\n{\"steps\":[]}\n```\nand that is the plan");
    assert!(matches!(trailing, Err(PlannerError::Decode(_))));

    let array = decode_single_json("[1,2,3]");
    assert!(matches!(array, Err(PlannerError::Decode(_))));

    let blank = decode_single_json("   ");
    assert!(matches!(blank, Err(PlannerError::Decode(_))));

    let unterminated = decode_single_json("```json\n{\"steps\":[]}");
    assert!(matches!(unterminated, Err(PlannerError::Decode(_))));
}

#[tokio::test]
async fn a_repair_that_cannot_decode_is_reported_as_exhausted() {
    let gateway = FakeModelGateway::new(vec![reply(EMPTY_PLAN), reply("not JSON at all")]);
    let planner = Planner::new(Arc::new(gateway.clone()));
    let cancel = test_cancel();

    let error = planner
        .draft(plan_request_fixture(), &cancel)
        .await
        .expect_err("prose after a repair is still invalid");

    assert!(matches!(error, PlannerError::RepairExhausted(_)));
    assert_eq!(gateway.calls(), 2);
}

#[test]
fn the_fixture_mission_compiles() {
    let manifest = mission();
    assert_eq!(manifest.name, "desk_note");
    let error = mission()
        .validate_plan(&lfm3_harness::PlanDraft { steps: Vec::new() })
        .expect_err("an empty plan cannot be compiled");
    assert!(matches!(error, PlanError::NoSteps));
}
