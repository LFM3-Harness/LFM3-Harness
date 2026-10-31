//! Test-only scripted model used by the fake-model CLI gate.
//!
//! This module models a model's *wire replies*, not a production executor.
//! It exists so the CLI wiring, acceptance freezing, verification, retries,
//! and reporting can be tested without weights. Packaged builds disable the
//! `fake-model` feature entirely.

use std::sync::atomic::{AtomicUsize, Ordering};

use lfm3_harness::{
    model::{
        AssistantMessage, ChatChoice, ChatRequest, ChatResponse, Decision, DecisionRequest,
        FunctionCall, ModelError, ModelFuture, ToolCall, TranscribeRequest, Transcript,
    },
    CancellationChild, ModelGateway,
};
use serde_json::json;

use crate::fixture::DeskFixture;

pub struct FakeGateway {
    fixture: DeskFixture,
    writes: AtomicUsize,
    calls: AtomicUsize,
}

impl FakeGateway {
    #[must_use]
    pub fn new(fixture: DeskFixture) -> Self {
        Self {
            fixture,
            writes: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        }
    }

    fn plan_reply(&self) -> ChatResponse {
        let plan = json!({
            "steps": [
                {
                    "index": 1,
                    "title": "Write the summary",
                    "criterion": format!(
                        "{} contains every required fact from the note",
                        self.fixture.output
                    ),
                    "tool": "write_file",
                    "checks": [{"kind": "file_exists", "path": self.fixture.output}]
                }
            ]
        });
        text_reply(&plan.to_string())
    }

    fn act_reply(&self, _request: &ChatRequest) -> Result<ChatResponse, ModelError> {
        let attempt = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        let content = self
            .fixture
            .scripted_write(attempt)
            .map_err(|error| ModelError::Configuration(format!("scripted write: {error}")))?;
        let arguments = json!({"path": self.fixture.output, "content": content}).to_string();
        let call_id = format!(
            "fake-call-{}",
            self.calls.fetch_add(1, Ordering::SeqCst) + 1
        );
        Ok(ChatResponse {
            id: "fake".to_owned(),
            model: "fake-model".to_owned(),
            choices: vec![ChatChoice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant".to_owned(),
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: call_id,
                        kind: "function".to_owned(),
                        function: FunctionCall {
                            name: "write_file".to_owned(),
                            arguments,
                        },
                    }],
                },
                finish_reason: Some("tool_calls".to_owned()),
            }],
            usage: None,
        })
    }
}

impl ModelGateway for FakeGateway {
    fn chat(
        &self,
        request: ChatRequest,
        _cancel: CancellationChild,
    ) -> ModelFuture<'_, ChatResponse> {
        Box::pin(async move {
            if !request.tools.is_empty() {
                return self.act_reply(&request);
            }
            if request.response_format.is_some() {
                // Planning or strategy replanning. A strategy request is
                // rejected honestly; the run must stop rather than guess.
                let system = request
                    .messages
                    .first()
                    .and_then(|message| match &message.content {
                        lfm3_harness::model::MessageContent::Text(text) => Some(text.as_str()),
                        lfm3_harness::model::MessageContent::Parts(_) => None,
                    })
                    .unwrap_or_default();
                if system.contains("strategy planner") {
                    return Err(ModelError::Protocol(
                        "the fake model cannot propose a new strategy".to_owned(),
                    ));
                }
                return Ok(self.plan_reply());
            }
            Err(ModelError::Protocol(
                "the fake model only answers planning and action requests".to_owned(),
            ))
        })
    }

    fn transcribe(
        &self,
        _request: TranscribeRequest,
        _cancel: CancellationChild,
    ) -> ModelFuture<'_, Transcript> {
        Box::pin(async {
            Err(ModelError::Configuration(
                "fake mode has no transcription model".to_owned(),
            ))
        })
    }

    fn decide(
        &self,
        _request: DecisionRequest,
        _cancel: CancellationChild,
    ) -> ModelFuture<'_, Decision> {
        Box::pin(async {
            Err(ModelError::Configuration(
                "fake mode has no decision model".to_owned(),
            ))
        })
    }
}

fn text_reply(content: &str) -> ChatResponse {
    ChatResponse {
        id: "fake".to_owned(),
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
    }
}
