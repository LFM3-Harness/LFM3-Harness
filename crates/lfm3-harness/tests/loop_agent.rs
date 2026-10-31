//! The profile-shaped job loop: plan gate, checked steps, review turn.
//!
//! These tests script the model. The harness, not the script, decides whether a
//! step is done and whether the job passed.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use lfm3_harness::{
    agent::{AgentLoop, LoopOptions, LoopStatus, LFM3_TEXT},
    model::{
        AssistantMessage, ChatChoice, ChatRequest, ChatResponse, Decision, DecisionRequest,
        FunctionCall, ModelError, ModelFuture, ModelGateway, ToolCall, TranscribeRequest,
        Transcript,
    },
    CancellationContext, ToolName, ToolRegistry,
};
use serde_json::{json, Value};

/// Scripted gateway: one reply per acting call, recording every request.
#[derive(Clone)]
struct ScriptedGateway {
    replies: Arc<Mutex<VecDeque<Result<ChatResponse, ModelError>>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedGateway {
    fn new(replies: Vec<Result<ChatResponse, ModelError>>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("requests lock").clone()
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ModelGateway for ScriptedGateway {
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
                    "scripted gateway is exhausted".to_owned(),
                ))
            })
        })
    }

    fn transcribe(
        &self,
        _request: TranscribeRequest,
        _cancel: lfm3_harness::CancellationChild,
    ) -> ModelFuture<'_, Transcript> {
        Box::pin(async { Err(ModelError::Transport("not scripted".to_owned())) })
    }

    fn decide(
        &self,
        _request: DecisionRequest,
        _cancel: lfm3_harness::CancellationChild,
    ) -> ModelFuture<'_, Decision> {
        Box::pin(async { Err(ModelError::Transport("not scripted".to_owned())) })
    }
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}-{}", uuid::Uuid::new_v4().simple()),
        kind: "function".to_owned(),
        function: FunctionCall {
            name: name.to_owned(),
            arguments: arguments.to_string(),
        },
    }
}

fn reply(calls: Vec<ToolCall>) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse {
        id: "scripted-response".to_owned(),
        model: "scripted-model".to_owned(),
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

fn options() -> LoopOptions {
    LoopOptions {
        max_turns: 8,
        max_calls: 3,
        replay: 8,
        plan_first: true,
        max_plan_steps: 6,
    }
}

fn tool_names(request: &ChatRequest) -> Vec<String> {
    request
        .tools
        .iter()
        .map(|tool| tool.function.name.clone())
        .collect()
}

fn workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(directory.path().join("notes")).expect("notes");
    std::fs::write(
        directory.path().join("notes/alpha.md"),
        "alpha is the measured value.",
    )
    .expect("note");
    let workspace = directory.path().to_path_buf();
    (directory, workspace)
}

fn registry(workspace: &std::path::Path) -> ToolRegistry {
    ToolRegistry::new(workspace)
        .expect("registry")
        .grant(ToolName::ListFiles)
        .grant(ToolName::ReadFile)
        .grant(ToolName::WriteFile)
        .grant(ToolName::EditFile)
}

fn plan_calls(done: bool) -> Vec<ToolCall> {
    vec![call(
        "todo_write",
        json!({
            "items": [
                {
                    "text": "read the source note",
                    "done": done,
                    "check": {"kind": "file_exists", "path": "notes/alpha.md"}
                },
                {
                    "text": "write the report",
                    "done": done,
                    "check": {
                        "kind": "content_matches",
                        "path": "out/report.md",
                        "contains": "alpha"
                    }
                }
            ]
        }),
    )]
}

#[tokio::test]
async fn a_job_plans_first_checks_steps_and_reviews_the_output() {
    let gateway = ScriptedGateway::new(vec![
        reply(plan_calls(false)),
        reply(vec![call("read_file", json!({"path": "notes/alpha.md"}))]),
        reply(vec![
            call(
                "write_file",
                json!({
                    "path": "out/report.md",
                    "content": "# Report\n\nalpha is 24 units.\n"
                }),
            ),
            call(
                "todo_write",
                json!({
                    "items": [
                        {
                            "text": "read the source note",
                            "done": true,
                            "check": {"kind": "file_exists", "path": "notes/alpha.md"}
                        },
                        {
                            "text": "write the report",
                            "done": true,
                            "check": {
                                "kind": "content_matches",
                                "path": "out/report.md",
                                "contains": "alpha"
                            }
                        }
                    ]
                }),
            ),
        ]),
        reply(vec![call("read_file", json!({"path": "out/report.md"}))]),
    ]);
    let (directory, workspace) = workspace();
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let runner = AgentLoop::new(
        Arc::new(gateway.clone()),
        Arc::new(registry(&workspace)),
        "test",
    )
    .with_options(options());

    let report = runner
        .run(
            "Summarise notes/alpha.md into out/report.md",
            Some("out/report.md"),
            &workspace,
            &cancel,
            &mut |_| {},
        )
        .await
        .expect("job runs");

    assert_eq!(report.status, LoopStatus::Passed);
    assert_eq!(report.turns.len(), 4);
    assert_eq!(report.turns[0].phase, "plan");
    assert_eq!(report.turns[1].phase, "work");
    assert_eq!(report.turns[3].phase, "review");
    assert!(report.todos.iter().all(|todo| todo.done));
    assert!(report.todos.iter().all(|todo| todo.check.is_some()));
    assert_eq!(gateway.calls(), 4);

    let requests = gateway.requests();
    assert_eq!(tool_names(&requests[0]), vec!["todo_write"]);
    let worker = tool_names(&requests[1]);
    assert!(worker.contains(&"read_file".to_owned()));
    assert!(worker.contains(&"finish".to_owned()));
    let reviewer = tool_names(&requests[3]);
    assert!(reviewer.contains(&"read_file".to_owned()));
    assert!(reviewer.contains(&"finish".to_owned()));
    let brief = match &requests[3].messages[1].content {
        lfm3_harness::model::MessageContent::Text(text) => text.clone(),
        _ => panic!("expected a text brief"),
    };
    assert!(brief.contains("Phase: review"));
    // The model wrote "24 units" but never read that number anywhere.
    assert!(brief.contains("grounding check"), "{brief}");
    assert!(brief.contains("24"), "{brief}");
    assert_eq!(requests[0].temperature, 0.1);
    assert_eq!(requests[0].top_k, Some(50));
    assert_eq!(requests[0].repetition_penalty, Some(1.1));
    std::fs::remove_dir_all(&directory).ok();
}

#[tokio::test]
async fn the_plan_gate_refuses_workspace_actions_on_the_first_turn() {
    let gateway = ScriptedGateway::new(vec![
        reply(vec![call("read_file", json!({"path": "notes/alpha.md"}))]),
        reply(plan_calls(false)),
        reply(vec![call("read_file", json!({"path": "notes/alpha.md"}))]),
        reply(vec![call(
            "write_file",
            json!({"path": "out/report.md", "content": "# Report\n\nalpha is the measured value.\n"}),
        )]),
        reply(vec![call("read_file", json!({"path": "out/report.md"}))]),
    ]);
    let (directory, workspace) = workspace();
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let runner = AgentLoop::new(Arc::new(gateway), Arc::new(registry(&workspace)), "test")
        .with_options(options());

    let report = runner
        .run(
            "Summarise notes/alpha.md into out/report.md",
            Some("out/report.md"),
            &workspace,
            &cancel,
            &mut |_| {},
        )
        .await
        .expect("job runs");

    assert_eq!(report.status, LoopStatus::Passed);
    assert!(!report.turns[0].calls[0].ok);
    assert!(report.turns[0].calls[0].output.contains("plan phase"));
    std::fs::remove_dir_all(&directory).ok();
}

#[tokio::test]
async fn finish_is_refused_once_until_the_output_is_re_read() {
    let gateway = ScriptedGateway::new(vec![
        reply(plan_calls(false)),
        reply(vec![call("read_file", json!({"path": "notes/alpha.md"}))]),
        reply(vec![call(
            "write_file",
            json!({"path": "out/report.md", "content": "# Report\n\nalpha is the measured value.\n"}),
        )]),
        reply(vec![call("finish", json!({}))]),
        reply(vec![call("finish", json!({}))]),
    ]);
    let (directory, workspace) = workspace();
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let runner = AgentLoop::new(Arc::new(gateway), Arc::new(registry(&workspace)), "test")
        .with_options(options());

    let report = runner
        .run(
            "Summarise notes/alpha.md into out/report.md",
            Some("out/report.md"),
            &workspace,
            &cancel,
            &mut |_| {},
        )
        .await
        .expect("job runs");

    assert_eq!(report.status, LoopStatus::Passed);
    assert_eq!(report.turns.len(), 5);
    assert!(report.turns[3].note.contains("finish refused"));
    assert!(!report.turns[3].calls[0].ok || report.turns[3].calls[0].tool == "finish");
    std::fs::remove_dir_all(&directory).ok();
}

#[tokio::test]
async fn a_stall_counts_unproductive_turns_not_refused_calls() {
    // Three plan-phase read attempts in a row. Each refusal must count once, so
    // the loop stops after the third unproductive turn, not the second.
    let gateway = ScriptedGateway::new(vec![
        reply(vec![call("read_file", json!({"path": "notes/alpha.md"}))]),
        reply(vec![call("read_file", json!({"path": "notes/alpha.md"}))]),
        reply(vec![call("read_file", json!({"path": "notes/alpha.md"}))]),
    ]);
    let (directory, workspace) = workspace();
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let runner = AgentLoop::new(Arc::new(gateway), Arc::new(registry(&workspace)), "test")
        .with_options(options());

    let report = runner
        .run(
            "Summarise notes/alpha.md into out/report.md",
            Some("out/report.md"),
            &workspace,
            &cancel,
            &mut |_| {},
        )
        .await
        .expect("job runs");

    assert_eq!(report.status, LoopStatus::Stalled);
    assert_eq!(report.turns.len(), 3, "one count per unproductive turn");
    assert_eq!(report.stopped, "three turns without a written plan");
    assert!(report
        .turns
        .iter()
        .all(|turn| turn.phase == "plan" && turn.calls[0].output.contains("plan phase")));
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn the_profile_is_the_loop_ceiling() {
    let options = LFM3_TEXT.loop_options(999);
    assert_eq!(options.max_turns, 64);
    assert_eq!(options.max_calls, 3);
    assert!(options.plan_first);
    assert_eq!(LFM3_TEXT.name, "LFM3-1.7B");
}
