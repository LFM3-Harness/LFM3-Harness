//! Opt-in PostHog AI Observability for the harness's own model calls.
//!
//! The harness has no hosted provider: every generation is a loopback call to
//! a llama.cpp server it owns, which PostHog's provider-wrapping AI SDKs
//! cannot intercept. This module therefore uses PostHog's documented manual
//! `$ai_generation` capture: one event per model call carrying the model
//! identity, latency, the token usage the server reported, the tools that were
//! offered, and the outcome.
//!
//! Tracing is off unless `POSTHOG_API_KEY` is set. Without a key there is no
//! client, no socket, and no work on the model path, so behavior is exactly
//! what it was. With a key, events are queued to a background task and every
//! send failure is dropped: a trace can never change, delay, or fail a model
//! call. Prompt and completion text is only attached when
//! `POSTHOG_AI_CAPTURE_CONTENT=1`, because this product promises that local
//! file content stays on the machine.

use std::{sync::OnceLock, time::Duration};

use reqwest::Client;
use serde_json::{json, Map, Value};

use crate::model::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, MessageContent, ModelError, ToolCall,
};

/// PostHog project token. Empty or unset disables tracing entirely.
pub const API_KEY_VAR: &str = "POSTHOG_API_KEY";
/// Ingestion host; the EU region is `https://eu.i.posthog.com`.
pub const HOST_VAR: &str = "POSTHOG_HOST";
/// `1` attaches prompt and completion text to each trace.
pub const CONTENT_VAR: &str = "POSTHOG_AI_CAPTURE_CONTENT";

const DEFAULT_HOST: &str = "https://us.i.posthog.com";
/// Local inference has no per-token price, so a traced generation costs nothing.
const LOCAL_COST_USD: f64 = 0.0;
/// There is no account concept inside a local app; one stable id keeps every
/// trace attributable to this harness installation.
const DISTINCT_ID: &str = "lfm3-harness";
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// A PostHog capture client. Constructed only when a key is configured.
pub struct PosthogTracer {
    api_key: String,
    host: String,
    capture_content: bool,
    http: Client,
}

impl PosthogTracer {
    /// Builds a tracer from the environment, or nothing when tracing is off.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var(API_KEY_VAR).ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        let host = std::env::var(HOST_VAR)
            .ok()
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_owned());
        let capture_content = std::env::var(CONTENT_VAR)
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes"));
        let http = Client::builder().timeout(SEND_TIMEOUT).build().ok()?;
        Some(Self {
            api_key,
            host,
            capture_content,
            http,
        })
    }

    /// Queues one event for ingestion without waiting for it.
    fn capture(&self, event: &str, properties: Value) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let body = json!({
            "api_key": self.api_key,
            "batch": [{ "event": event, "distinct_id": DISTINCT_ID, "properties": properties }],
        });
        let endpoint = format!("{}/batch/", self.host.trim_end_matches('/'));
        let http = self.http.clone();
        runtime.spawn(async move {
            // Observability is best effort; a trace is never allowed to
            // surface as an error in the model path.
            let _ = http.post(endpoint).json(&body).send().await;
        });
    }
}

/// One finished model call, reduced to what PostHog's AI events read.
pub struct Generation<'a> {
    pub span_name: &'a str,
    pub model: &'a str,
    pub base_url: &'a str,
    pub latency: Duration,
    pub time_to_first_token: Option<Duration>,
    pub stream: bool,
    pub request: &'a ChatRequest,
    pub response: Option<&'a ChatResponse>,
    pub error: Option<&'a ModelError>,
}

impl Generation<'_> {
    /// The `$ai_generation` properties for this call.
    fn properties(&self, capture_content: bool) -> Value {
        let mut properties = Map::new();
        properties.insert("$ai_trace_id".to_owned(), json!(trace_id()));
        properties.insert("$ai_span_id".to_owned(), json!(trace_id()));
        properties.insert("$ai_span_name".to_owned(), json!(self.span_name));
        // A single local workload has no conversation session to group.
        properties.insert("$ai_session_id".to_owned(), Value::Null);
        properties.insert("$ai_model".to_owned(), json!(self.model));
        properties.insert("$ai_provider".to_owned(), json!("llama.cpp"));
        properties.insert("$ai_base_url".to_owned(), json!(self.base_url));
        properties.insert("$ai_latency".to_owned(), json!(self.latency.as_secs_f64()));
        properties.insert("$ai_stream".to_owned(), json!(self.stream));
        properties.insert(
            "$ai_temperature".to_owned(),
            json!(f64::from(self.request.temperature)),
        );
        properties.insert("$ai_max_tokens".to_owned(), json!(self.request.max_tokens));
        properties.insert("$ai_total_cost_usd".to_owned(), json!(LOCAL_COST_USD));
        properties.insert(
            "$ai_tools".to_owned(),
            Value::Array(
                self.request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": { "name": tool.function.name },
                        })
                    })
                    .collect(),
            ),
        );
        if let Some(first_token) = self.time_to_first_token {
            properties.insert(
                "$ai_time_to_first_token".to_owned(),
                json!(first_token.as_secs_f64()),
            );
        }
        if let Some(response) = self.response {
            if let Some(usage) = response.usage {
                properties.insert("$ai_input_tokens".to_owned(), json!(usage.prompt_tokens));
                properties.insert(
                    "$ai_output_tokens".to_owned(),
                    json!(usage.completion_tokens),
                );
            }
            if let Some(reason) = response
                .choices
                .first()
                .and_then(|choice| choice.finish_reason.clone())
            {
                properties.insert("$ai_stop_reason".to_owned(), json!(reason));
            }
            properties.insert(
                "ai_tool_calls".to_owned(),
                json!(response
                    .tool_calls()
                    .iter()
                    .map(|call| call.function.name.clone())
                    .collect::<Vec<_>>()),
            );
            if capture_content {
                properties.insert(
                    "$ai_output_choices".to_owned(),
                    json!([output_choice(response)]),
                );
            }
        }
        if let Some(error) = self.error {
            properties.insert("$ai_is_error".to_owned(), json!(true));
            properties.insert("$ai_error".to_owned(), json!(error.to_string()));
            if let ModelError::Http { status, .. } = error {
                properties.insert("$ai_http_status".to_owned(), json!(status));
            }
        } else {
            properties.insert("$ai_is_error".to_owned(), json!(false));
        }
        if capture_content {
            properties.insert(
                "$ai_input".to_owned(),
                Value::Array(self.request.messages.iter().map(message_json).collect()),
            );
        }
        Value::Object(properties)
    }
}

#[cfg(test)]
impl PosthogTracer {
    fn with_host(api_key: &str, host: &str) -> Self {
        Self {
            api_key: api_key.to_owned(),
            host: host.to_owned(),
            capture_content: false,
            http: Client::builder()
                .timeout(SEND_TIMEOUT)
                .build()
                .expect("test client builds"),
        }
    }
}

/// The process-wide tracer, created on first use from the environment.
#[must_use]
pub fn tracer() -> Option<&'static PosthogTracer> {
    static TRACER: OnceLock<Option<PosthogTracer>> = OnceLock::new();
    TRACER.get_or_init(PosthogTracer::from_env).as_ref()
}

/// Captures one model call. A no-op when no key is configured.
pub fn record_generation(generation: Generation<'_>) {
    let Some(tracer) = tracer() else {
        return;
    };
    let properties = generation.properties(tracer.capture_content);
    tracer.capture("$ai_generation", properties);
}

fn trace_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn message_json(message: &ChatMessage) -> Value {
    let mut json_message = Map::new();
    json_message.insert("role".to_owned(), json!(message.role.as_str()));
    json_message.insert(
        "content".to_owned(),
        match &message.content {
            MessageContent::Text(text) => json!(text),
            MessageContent::Parts(parts) => Value::Array(parts.iter().map(part_json).collect()),
        },
    );
    if let Some(name) = &message.name {
        json_message.insert("name".to_owned(), json!(name));
    }
    if !message.tool_calls.is_empty() {
        json_message.insert(
            "tool_calls".to_owned(),
            Value::Array(message.tool_calls.iter().map(tool_call_json).collect()),
        );
    }
    Value::Object(json_message)
}

fn output_choice(response: &ChatResponse) -> Value {
    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = response.content().filter(|text| !text.is_empty()) {
        content.push(json!({ "type": "text", "text": text }));
    }
    content.extend(response.tool_calls().iter().map(tool_call_json));
    json!({ "role": "assistant", "content": content })
}

/// Image and audio parts are described, never uploaded: their bytes are local
/// file content and the harness keeps them on the machine.
fn part_json(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({ "type": "text", "text": text }),
        ContentPart::ImageUrl { .. } => json!({ "type": "image" }),
        ContentPart::InputAudio { .. } => json!({ "type": "audio" }),
    }
}

fn tool_call_json(call: &ToolCall) -> Value {
    let arguments = serde_json::from_str::<Value>(&call.function.arguments)
        .unwrap_or_else(|_| json!(call.function.arguments));
    json!({
        "type": "function",
        "function": { "name": call.function.name, "arguments": arguments },
    })
}

#[cfg(test)]
mod tests {
    use super::{Generation, CONTENT_VAR, DEFAULT_HOST, HOST_VAR};
    use crate::model::{
        AssistantMessage, ChatChoice, ChatRequest, ChatResponse, FunctionCall, ToolCall,
        ToolDefinition, Usage,
    };
    use std::time::Duration;

    fn request() -> ChatRequest {
        let mut request =
            ChatRequest::new(vec![crate::model::ChatMessage::user("summarise notes")]);
        request.tools = vec![ToolDefinition {
            kind: "function".to_owned(),
            function: crate::model::FunctionDefinition {
                name: "read_file".to_owned(),
                description: "read one file".to_owned(),
                parameters: serde_json::json!({ "type": "object" }),
            },
        }];
        request
    }

    fn response() -> ChatResponse {
        ChatResponse {
            id: "chatcmpl-1".to_owned(),
            model: "lfm3-1.7b".to_owned(),
            choices: vec![ChatChoice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant".to_owned(),
                    content: Some("here is the summary".to_owned()),
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".to_owned(),
                        kind: "function".to_owned(),
                        function: FunctionCall {
                            name: "read_file".to_owned(),
                            arguments: r#"{"path":"notes.md"}"#.to_owned(),
                        },
                    }],
                },
                finish_reason: Some("stop".to_owned()),
            }],
            usage: Some(Usage {
                prompt_tokens: 21,
                completion_tokens: 8,
                total_tokens: 29,
            }),
        }
    }

    fn generation<'a>(request: &'a ChatRequest, response: &'a ChatResponse) -> Generation<'a> {
        Generation {
            span_name: "chat",
            model: "lfm3-1.7b",
            base_url: "http://127.0.0.1:8080/v1",
            latency: Duration::from_millis(1250),
            time_to_first_token: None,
            stream: false,
            request,
            response: Some(response),
            error: None,
        }
    }

    #[test]
    fn a_generation_carries_model_latency_tokens_tools_and_cost() {
        let request = request();
        let response = response();
        let properties = generation(&request, &response).properties(false);

        assert_eq!(properties["$ai_model"], serde_json::json!("lfm3-1.7b"));
        assert_eq!(properties["$ai_provider"], serde_json::json!("llama.cpp"));
        assert_eq!(properties["$ai_latency"], serde_json::json!(1.25));
        assert_eq!(properties["$ai_input_tokens"], serde_json::json!(21));
        assert_eq!(properties["$ai_output_tokens"], serde_json::json!(8));
        assert_eq!(properties["$ai_stop_reason"], serde_json::json!("stop"));
        assert_eq!(properties["$ai_is_error"], serde_json::json!(false));
        assert_eq!(properties["$ai_total_cost_usd"], serde_json::json!(0.0));
        assert_eq!(
            properties["$ai_tools"][0]["function"]["name"],
            serde_json::json!("read_file")
        );
        assert_eq!(
            properties["ai_tool_calls"],
            serde_json::json!(["read_file"])
        );
        assert!(properties["$ai_trace_id"].is_string());
    }

    #[test]
    fn prompt_and_completion_text_stay_local_until_content_capture_is_on() {
        let request = request();
        let response = response();
        assert!(generation(&request, &response)
            .properties(false)
            .get("$ai_input")
            .is_none());
        assert!(generation(&request, &response)
            .properties(false)
            .get("$ai_output_choices")
            .is_none());

        let properties = generation(&request, &response).properties(true);
        assert_eq!(
            properties["$ai_input"][0]["content"],
            serde_json::json!("summarise notes")
        );
        assert_eq!(
            properties["$ai_output_choices"][0]["content"][0]["text"],
            serde_json::json!("here is the summary")
        );
        // Audio and image bytes are described, never uploaded.
        let audio =
            crate::model::ChatMessage::user_parts(vec![crate::model::ContentPart::InputAudio {
                input_audio: crate::model::InputAudio {
                    data: "AAAA".to_owned(),
                    format: "wav".to_owned(),
                },
            }]);
        let mut with_audio = request.clone();
        with_audio.messages.push(audio);
        let properties = Generation {
            request: &with_audio,
            ..generation(&request, &response)
        }
        .properties(true);
        assert_eq!(
            properties["$ai_input"][1]["content"][0]["type"],
            serde_json::json!("audio")
        );
    }

    #[test]
    fn a_failed_call_is_traced_as_an_error_with_its_status() {
        let request = request();
        let error = crate::model::ModelError::Http {
            status: 501,
            body: "not supported".to_owned(),
        };
        let properties = Generation {
            span_name: "chat",
            model: "d1-2b",
            base_url: "http://127.0.0.1:8083/v1",
            latency: Duration::from_millis(4),
            time_to_first_token: None,
            stream: false,
            request: &request,
            response: None,
            error: Some(&error),
        }
        .properties(false);

        assert_eq!(properties["$ai_is_error"], serde_json::json!(true));
        assert_eq!(properties["$ai_http_status"], serde_json::json!(501));
        assert!(properties["$ai_error"]
            .as_str()
            .is_some_and(|message| message.contains("501")));
    }

    /// True once a full HTTP request, headers and body, has been read.
    fn request_complete(buffer: &[u8]) -> bool {
        let Some(head_end) = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        else {
            return false;
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        buffer.len() >= head_end + length
    }

    #[tokio::test]
    async fn a_generation_is_sent_to_the_configured_ingestion_host() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let address = listener.local_addr().expect("local address");
        let ingest = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept one request");
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("read timeout");
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut socket, &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        buffer.extend_from_slice(&chunk[..read]);
                        if request_complete(&buffer) {
                            break;
                        }
                    }
                }
            }
            let _ = std::io::Write::write_all(
                &mut socket,
                b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n",
            );
            String::from_utf8_lossy(&buffer).into_owned()
        });

        let tracer = super::PosthogTracer::with_host("ph_test_key", &format!("http://{address}"));
        tracer.capture(
            "$ai_generation",
            serde_json::json!({ "$ai_model": "lfm3-1.7b" }),
        );
        let request = tokio::task::spawn_blocking(move || ingest.join())
            .await
            .expect("ingest thread joins")
            .expect("ingest thread finishes");

        assert!(request.starts_with("POST /batch/ HTTP/1.1"), "{request}");
        assert!(
            request.contains("\"event\":\"$ai_generation\""),
            "{request}"
        );
        assert!(request.contains("ph_test_key"), "{request}");
        assert!(request.contains("lfm3-1.7b"), "{request}");
    }

    #[test]
    fn host_and_content_flags_have_documented_defaults() {
        assert_eq!(DEFAULT_HOST, "https://us.i.posthog.com");
        assert!(HOST_VAR.starts_with("POSTHOG_"));
        assert!(CONTENT_VAR.starts_with("POSTHOG_"));
    }
}
