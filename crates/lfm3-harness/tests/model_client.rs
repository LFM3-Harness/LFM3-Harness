mod support;

use std::time::{Duration, Instant};

use lfm3_harness::{
    labels, CancellationContext, ChatMessage, ChatRequest, DecisionRequest, LlamaClient,
    ModelError, ToolDefinition, TranscribeRequest,
};
use serde_json::json;
use support::{models_fixture, text_reply, FakeConfig, FakeLlamaServer};

const MODEL: &str = "lfm3-1.7b";

fn tool_call_fixture() -> String {
    json!({
        "id": "chatcmpl-tools",
        "object": "chat.completion",
        "model": MODEL,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "reasoning_content": "selecting a tool",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": "{\"path\":\"notes.md\",\"content\":\"done\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 12, "completion_tokens": 6, "total_tokens": 18}
    })
    .to_string()
}

fn tool_request() -> ChatRequest {
    ChatRequest::new(vec![
        ChatMessage::system("Choose exactly one allowed tool."),
        ChatMessage::user("Write the notes file."),
    ])
    .with_tools(vec![ToolDefinition::function(
        "write_file",
        "Write a file inside the workspace",
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
            "required": ["path", "content"]
        }),
    )])
    .with_temperature(0.0)
    .with_max_tokens(256)
}

fn client(server: &FakeLlamaServer) -> LlamaClient {
    LlamaClient::from_endpoint(server.url(), MODEL).expect("loopback client")
}

#[tokio::test]
async fn text_client_preserves_native_tool_calls() {
    let server = FakeLlamaServer::start(FakeConfig::chat(&[MODEL], tool_call_fixture())).await;
    let client = client(&server);
    let cancel = CancellationContext::new(Duration::from_secs(5));

    let response = client
        .chat(tool_request(), &cancel.child())
        .await
        .expect("chat succeeds");
    assert_eq!(response.tool_calls()[0].function.name, "write_file");
    assert_eq!(
        response
            .first_message()
            .unwrap()
            .reasoning_content
            .as_deref(),
        Some("selecting a tool")
    );
    assert!(response.content().is_none());
    assert_eq!(response.usage.unwrap().total_tokens, 18);
    client
        .verify_identity(&cancel.child())
        .await
        .expect("identity matches");

    let request = server.chat_requests().pop().expect("one chat request");
    let sent: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "write_file");
    assert_eq!(sent["messages"][0]["role"], "system");
}

#[tokio::test]
async fn identity_mismatch_is_rejected_before_use() {
    let server = FakeLlamaServer::start(FakeConfig::chat(&["lfm3-vl-2b"], text_reply("hi"))).await;
    let client = client(&server);
    let cancel = CancellationContext::new(Duration::from_secs(5));
    let error = client
        .verify_identity(&cancel.child())
        .await
        .expect_err("wrong model identity");
    assert!(
        matches!(error, ModelError::IdentityMismatch { ref expected, ref actual }
            if expected == MODEL && actual == &vec!["lfm3-vl-2b".to_owned()]),
        "{error:?}"
    );
}

#[tokio::test]
async fn remote_endpoints_are_refused() {
    let error = LlamaClient::from_endpoint("http://192.0.2.10:8080", MODEL)
        .expect_err("remote endpoint must fail");
    assert!(matches!(error, ModelError::RemoteEndpoint(_)));
}

#[tokio::test]
async fn server_errors_surface_instead_of_fake_content() {
    let mut config = FakeConfig::chat(&[MODEL], "{\"error\":\"boom\"}");
    config.chat_status = 500;
    let server = FakeLlamaServer::start(config).await;
    let cancel = CancellationContext::new(Duration::from_secs(5));
    let error = client(&server)
        .chat(tool_request(), &cancel.child())
        .await
        .expect_err("status 500 is an error");
    assert!(
        matches!(error, ModelError::Http { status: 500, .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn cancellation_aborts_a_stalled_response() {
    let mut config = FakeConfig::chat(&[MODEL], tool_call_fixture());
    config.chat_delay = Duration::from_secs(30);
    let server = FakeLlamaServer::start(config).await;
    let cancel = CancellationContext::new(Duration::from_millis(100));

    let started = Instant::now();
    let error = client(&server)
        .chat(tool_request(), &cancel.child())
        .await
        .expect_err("deadline aborts the call");
    assert!(
        matches!(
            error,
            ModelError::Cancelled(lfm3_harness::CancelReason::Deadline)
        ),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn transcriptions_return_server_text() {
    let mut config = FakeConfig::chat(&[MODEL], text_reply("unused"));
    config.transcription = json!({"text": "hello world", "language": "en"}).to_string();
    let server = FakeLlamaServer::start(config).await;
    let cancel = CancellationContext::new(Duration::from_secs(5));

    let directory = tempfile::tempdir().unwrap();
    let audio = directory.path().join("note.wav");
    std::fs::write(&audio, b"RIFF....WAVE").unwrap();
    let transcript = client(&server)
        .transcribe(
            TranscribeRequest {
                path: audio,
                language: Some("en".to_owned()),
                prompt: None,
            },
            &cancel.child(),
        )
        .await
        .expect("transcription");
    assert_eq!(transcript.text, "hello world");
    assert_eq!(transcript.language.as_deref(), Some("en"));
    let request = server
        .requests()
        .into_iter()
        .find(|request| request.path.contains("/v1/audio/transcriptions"))
        .expect("transcription request");
    assert!(request.body.contains("note.wav"));
    assert!(request.body.contains("name=\"file\""));
    assert!(request.body.contains("name=\"model\""));
    assert!(request.body.contains(MODEL));
}

#[tokio::test]
async fn decisions_must_use_a_declared_label() {
    let labels = labels([
        ("pass", "criterion is visibly satisfied"),
        ("fail", "it is not"),
    ]);
    let valid = json!({
        "id": "d1",
        "model": "d1-2b",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "{\"label\":\"pass\",\"confidence\":0.9,\"rationale\":\"visible button\"}"
            }
        }]
    })
    .to_string();
    let server = FakeLlamaServer::start(FakeConfig::chat(&["d1-2b"], valid)).await;
    let cancel = CancellationContext::new(Duration::from_secs(5));
    let decide = LlamaClient::from_endpoint(server.url(), "d1-2b").unwrap();
    let decision = decide
        .decide(
            DecisionRequest::new("Is the button visible?", "screenshot bytes", labels.clone()),
            &cancel.child(),
        )
        .await
        .expect("decision");
    assert_eq!(decision.label, "pass");
    assert!((decision.confidence - 0.9).abs() < f64::EPSILON);

    let invalid = json!({
        "id": "d1",
        "model": "d1-2b",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "{\"label\":\"maybe\",\"confidence\":0.5}"
            }
        }]
    })
    .to_string();
    let server = FakeLlamaServer::start(FakeConfig::chat(&["d1-2b"], invalid)).await;
    let decide = LlamaClient::from_endpoint(server.url(), "d1-2b").unwrap();
    let error = decide
        .decide(
            DecisionRequest::new("Is the button visible?", "screenshot bytes", labels),
            &cancel.child(),
        )
        .await
        .expect_err("undeclared label is a protocol error");
    assert!(matches!(error, ModelError::Protocol(_)), "{error:?}");
}

#[tokio::test]
async fn models_fixture_lists_every_identity() {
    let fixture = models_fixture(&["lfm3-1.7b", "d1-2b"]);
    let value: serde_json::Value = serde_json::from_str(&fixture).unwrap();
    assert_eq!(value["data"][1]["id"], "d1-2b");
}
