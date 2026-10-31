//! Loopback-only client for the pinned llama.cpp server.

use std::{collections::BTreeMap, future::Future, pin::Pin, time::Duration};

use futures_util::StreamExt;

use reqwest::{multipart, Client, StatusCode};
use serde::{Deserialize, Serialize};
use url::Url;

use super::types::{
    AssistantMessage, ChatChoice, ChatRequest, ChatResponse, Decision, DecisionRequest,
    FunctionCall, ModelsResponse, ToolCall, TranscribeRequest, Transcript,
};
use crate::{is_loopback, CancelReason, CancellationChild};

/// Failure of a model call, a lease, or server startup.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model endpoint must be loopback: {0}")]
    RemoteEndpoint(String),
    #[error("unknown model: {0}")]
    UnknownModel(String),
    #[error("model identity mismatch: expected {expected}, server reported {actual:?}")]
    IdentityMismatch {
        expected: String,
        actual: Vec<String>,
    },
    #[error("insufficient memory for {model}: need {required} bytes, {available} available")]
    MemoryExhausted {
        model: String,
        required: u64,
        available: u64,
    },
    #[error("model server did not become ready: {0}")]
    Startup(String),
    #[error("model manager is shut down")]
    Shutdown,
    #[error("model {0} is leased by an active run")]
    Leased(String),
    #[error("model call was cancelled: {0}")]
    Cancelled(CancelReason),
    #[error("model endpoint returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("model transport failed: {0}")]
    Transport(String),
    #[error("model protocol violation: {0}")]
    Protocol(String),
    #[error("invalid model configuration: {0}")]
    Configuration(String),
}

/// Object-safe gateway used by the agent loop and replaces easily in tests.
pub type ModelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ModelError>> + Send + 'a>>;

pub trait ModelGateway: Send + Sync {
    fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, ChatResponse>;

    fn plan(
        &self,
        request: ChatRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, ChatResponse> {
        self.chat(request, cancel)
    }

    fn act(
        &self,
        request: ChatRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, ChatResponse> {
        self.chat(request, cancel)
    }

    fn look(
        &self,
        request: ChatRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, ChatResponse> {
        self.chat(request, cancel)
    }

    fn transcribe(
        &self,
        request: TranscribeRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, Transcript>;

    fn decide(
        &self,
        request: DecisionRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, Decision>;
}

#[derive(Clone, Debug)]
pub struct LlamaClient {
    http: Client,
    base: Url,
    model: String,
}

impl LlamaClient {
    pub fn new(base: Url, model: impl Into<String>) -> Result<Self, ModelError> {
        if !is_loopback(&base) {
            return Err(ModelError::RemoteEndpoint(base.to_string()));
        }
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .map_err(|error| ModelError::Configuration(error.to_string()))?;
        Ok(Self {
            http,
            base,
            model: model.into(),
        })
    }

    pub fn from_endpoint(raw: &str, model: impl Into<String>) -> Result<Self, ModelError> {
        let base = Url::parse(raw)
            .map_err(|error| ModelError::Configuration(format!("{raw:?}: {error}")))?;
        Self::new(base, model)
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn endpoint(&self) -> &Url {
        &self.base
    }

    /// Lists the identities the endpoint currently serves.
    pub async fn list_models(&self, cancel: &CancellationChild) -> Result<Vec<String>, ModelError> {
        let url = self.endpoint_for("v1/models");
        let (status, body) = self.send(cancel, self.http.get(url)).await?;
        self.ensure_success(status, &body)?;
        let models: ModelsResponse = serde_json::from_str(&body)
            .map_err(|error| ModelError::Protocol(format!("models response: {error}")))?;
        Ok(models.data.into_iter().map(|card| card.id).collect())
    }

    /// Confirms the server serves the expected model identity.
    pub async fn verify_identity(&self, cancel: &CancellationChild) -> Result<(), ModelError> {
        let served = self.list_models(cancel).await?;
        if served.iter().any(|id| id == &self.model) {
            return Ok(());
        }
        Err(ModelError::IdentityMismatch {
            expected: self.model.clone(),
            actual: served,
        })
    }

    pub async fn chat(
        &self,
        request: ChatRequest,
        cancel: &CancellationChild,
    ) -> Result<ChatResponse, ModelError> {
        let url = self.endpoint_for("v1/chat/completions");
        let wire = self.wire(&request, false);
        let (status, body) = self.send(cancel, self.http.post(url).json(&wire)).await?;
        self.ensure_success(status, &body)?;
        serde_json::from_str(&body)
            .map_err(|error| ModelError::Protocol(format!("chat response: {error}")))
    }

    /// Streams one completion over SSE, forwarding text deltas as they arrive.
    ///
    /// Tool calls are accumulated from streamed fragments and returned in the
    /// same shape as the buffered call, so the orchestration loop does not care
    /// which transport produced them.
    pub async fn chat_stream<F>(
        &self,
        request: ChatRequest,
        cancel: &CancellationChild,
        on_delta: &mut F,
    ) -> Result<ChatResponse, ModelError>
    where
        F: FnMut(&str) + Send + ?Sized,
    {
        let url = self.endpoint_for("v1/chat/completions");
        let wire = self.wire(&request, true);
        let response = cancel
            .run(self.http.post(url).json(&wire).send())
            .await
            .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?
            .map_err(|error| ModelError::Transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = cancel
                .run(response.text())
                .await
                .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?
                .map_err(|error| ModelError::Transport(error.to_string()))?;
            return Err(ModelError::Http {
                status: status.as_u16(),
                body: body.chars().take(2048).collect(),
            });
        }

        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut model = String::new();
        let mut calls: BTreeMap<usize, StreamAccum> = BTreeMap::new();
        'stream: loop {
            let next = cancel
                .run(stream.next())
                .await
                .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?;
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk.map_err(|error| ModelError::Transport(error.to_string()))?;
            buffer.extend_from_slice(&chunk);
            while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=position).collect::<Vec<u8>>();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    break 'stream;
                }
                let parsed: StreamChunk = match serde_json::from_str(data) {
                    Ok(parsed) => parsed,
                    Err(_) => continue,
                };
                if !parsed.model.is_empty() {
                    model = parsed.model;
                }
                for choice in parsed.choices {
                    if let Some(text) = choice.delta.content {
                        if !text.is_empty() {
                            content.push_str(&text);
                            on_delta(&text);
                        }
                    }
                    for fragment in choice.delta.tool_calls {
                        let entry = calls.entry(fragment.index).or_default();
                        if let Some(id) = fragment.id.filter(|value| !value.is_empty()) {
                            entry.id = Some(id);
                        }
                        if let Some(name) = fragment.function.name.filter(|value| !value.is_empty())
                        {
                            entry.name = name;
                        }
                        if let Some(arguments) = fragment.function.arguments {
                            entry.arguments.push_str(&arguments);
                        }
                    }
                }
            }
        }
        // Some servers flush the final line without a trailing newline.
        let tail = String::from_utf8_lossy(&buffer);
        if let Some(data) = tail.trim().strip_prefix("data:") {
            if let Ok(parsed) = serde_json::from_str::<StreamChunk>(data.trim()) {
                for choice in parsed.choices {
                    if let Some(text) = choice.delta.content {
                        if !text.is_empty() {
                            content.push_str(&text);
                            on_delta(&text);
                        }
                    }
                }
            }
        }

        let tool_calls = calls
            .into_values()
            .filter(|call| !call.name.is_empty())
            .map(|call| ToolCall {
                id: call
                    .id
                    .unwrap_or_else(|| format!("call-{}", uuid::Uuid::new_v4().simple())),
                kind: "function".to_owned(),
                function: FunctionCall {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
            .collect::<Vec<_>>();

        Ok(ChatResponse {
            id: String::new(),
            model,
            choices: vec![ChatChoice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant".to_owned(),
                    content: (!content.is_empty()).then_some(content),
                    reasoning_content: None,
                    tool_calls,
                },
                finish_reason: None,
            }],
            usage: None,
        })
    }

    fn wire<'a>(&'a self, request: &'a ChatRequest, stream: bool) -> WireChatRequest<'a> {
        WireChatRequest {
            model: &self.model,
            messages: &request.messages,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            top_k: request.top_k,
            repetition_penalty: request.repetition_penalty,
            stream,
            tools: &request.tools,
            tool_choice: request.tool_choice.as_deref(),
            response_format: request.response_format.as_ref(),
            chat_template_kwargs: disabled_thinking(),
        }
    }

    pub async fn transcribe(
        &self,
        request: TranscribeRequest,
        cancel: &CancellationChild,
    ) -> Result<Transcript, ModelError> {
        let bytes = tokio::fs::read(&request.path).await.map_err(|error| {
            ModelError::Configuration(format!("read {}: {error}", request.path.display()))
        })?;
        let file_name = request
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("audio")
            .to_owned();
        let mut form = multipart::Form::new()
            .part("file", multipart::Part::bytes(bytes).file_name(file_name))
            .text("model", self.model.clone())
            .text("response_format", "json");
        if let Some(language) = &request.language {
            form = form.text("language", language.clone());
        }
        if let Some(prompt) = &request.prompt {
            form = form.text("prompt", prompt.clone());
        }
        let url = self.endpoint_for("v1/audio/transcriptions");
        let (status, body) = self
            .send(cancel, self.http.post(url).multipart(form))
            .await?;
        self.ensure_success(status, &body)?;
        serde_json::from_str(&body)
            .map_err(|error| ModelError::Protocol(format!("transcription response: {error}")))
    }

    /// Asks the native decision endpoint for one label chosen from the supplied set.
    ///
    /// d1-2B is served as a llama.cpp decision model: it answers
    /// `/v1/systemone` with a probability distribution over typed choice
    /// labels and does not generate chat text. Calling the chat route against
    /// this server returns `501 not_supported_error`, so decisions must use
    /// the decision protocol directly.
    pub async fn decide(
        &self,
        request: DecisionRequest,
        cancel: &CancellationChild,
    ) -> Result<Decision, ModelError> {
        if request.labels.is_empty() {
            return Err(ModelError::Protocol(
                "a decision needs at least one label".to_owned(),
            ));
        }
        let criteria = request
            .labels
            .iter()
            .map(|(label, description)| {
                (label.clone(), serde_json::Value::from(description.clone()))
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        let wire = serde_json::json!({
            "model": self.model,
            "state": request.evidence,
            "questions": {
                "route": {
                    "type": "choice",
                    "instructions": request.instruction,
                    "criteria": criteria,
                }
            }
        });
        let url = self.endpoint_for("v1/systemone");
        let (status, body) = self.send(cancel, self.http.post(url).json(&wire)).await?;
        self.ensure_success(status, &body)?;
        let response: SystemOneResponse = serde_json::from_str(&body)
            .map_err(|error| ModelError::Protocol(format!("decision response: {error}")))?;
        let answer = response.answers.get("route").ok_or_else(|| {
            ModelError::Protocol("decision response has no route answer".to_owned())
        })?;
        if let Some(kind) = answer.kind.as_deref() {
            if kind != "choice" {
                return Err(ModelError::Protocol(format!(
                    "decision answer is {kind:?}, not a choice"
                )));
            }
        }
        let label = answer
            .choice
            .clone()
            .ok_or_else(|| ModelError::Protocol("decision response has no choice".to_owned()))?;
        if !request.labels.contains_key(&label) {
            return Err(ModelError::Protocol(format!(
                "decision label {label:?} is outside the declared set"
            )));
        }
        let confidence = answer
            .confidence
            .or_else(|| {
                answer.probabilities.as_ref().and_then(|probabilities| {
                    probabilities
                        .values()
                        .copied()
                        .fold(None, |best: Option<f64>, probability| {
                            Some(best.map_or(probability, |best| best.max(probability)))
                        })
                })
            })
            .ok_or_else(|| {
                ModelError::Protocol("decision response has no confidence".to_owned())
            })?;
        if !(0.0..=1.0).contains(&confidence) || !confidence.is_finite() {
            return Err(ModelError::Protocol(format!(
                "decision confidence {confidence} is outside [0,1]"
            )));
        }
        Ok(Decision {
            rationale: answer.rationale(&label, confidence),
            label,
            confidence,
            model: response
                .model
                .filter(|model| !model.trim().is_empty())
                .unwrap_or_else(|| self.model.clone()),
        })
    }

    fn endpoint_for(&self, path: &str) -> Url {
        let base = self.base.as_str().trim_end_matches('/');
        // Accept both a server root and an OpenAI-style `/v1` base without
        // duplicating the version segment.
        let path = match base.strip_suffix("/v1") {
            Some(_) => path.strip_prefix("v1/").unwrap_or(path),
            None => path,
        };
        Url::parse(&format!("{base}/{path}")).unwrap_or_else(|_| self.base.clone())
    }

    async fn send(
        &self,
        cancel: &CancellationChild,
        request: reqwest::RequestBuilder,
    ) -> Result<(StatusCode, String), ModelError> {
        let response = cancel
            .run(request.send())
            .await
            .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?;
        let response = response.map_err(|error| ModelError::Transport(error.to_string()))?;
        let status = response.status();
        let body = cancel
            .run(response.text())
            .await
            .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?;
        let body = body.map_err(|error| ModelError::Transport(error.to_string()))?;
        Ok((status, body))
    }

    fn ensure_success(&self, status: StatusCode, body: &str) -> Result<(), ModelError> {
        if status.is_success() {
            return Ok(());
        }
        Err(ModelError::Http {
            status: status.as_u16(),
            body: body.chars().take(2048).collect(),
        })
    }
}

impl ModelGateway for LlamaClient {
    fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, ChatResponse> {
        Box::pin(async move { LlamaClient::chat(self, request, &cancel).await })
    }

    fn transcribe(
        &self,
        request: TranscribeRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, Transcript> {
        Box::pin(async move { LlamaClient::transcribe(self, request, &cancel).await })
    }

    fn decide(
        &self,
        request: DecisionRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, Decision> {
        Box::pin(async move { LlamaClient::decide(self, request, &cancel).await })
    }
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    model: String,
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: StreamFunction,
}

#[derive(Default, Deserialize)]
struct StreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Default)]
struct StreamAccum {
    id: Option<String>,
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct WireChatRequest<'a> {
    model: &'a str,
    messages: &'a [crate::model::ChatMessage],
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repetition_penalty: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "slice_is_empty")]
    tools: &'a [crate::model::ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<&'a crate::model::ResponseFormat>,
    /// The LFM3 chat template thinks by default; a verifier-driven harness
    /// needs the answer inside the token budget, not a truncated thought.
    /// Templates that do not read this key ignore it.
    chat_template_kwargs: &'a serde_json::Value,
}

const DISABLED_THINKING_INIT: &str = r#"{"enable_thinking":false}"#;

fn disabled_thinking() -> &'static serde_json::Value {
    static KWARGS: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    KWARGS.get_or_init(|| {
        serde_json::from_str(DISABLED_THINKING_INIT)
            .expect("disabled-thinking template kwargs are valid JSON")
    })
}

#[derive(Deserialize)]
struct SystemOneResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    answers: BTreeMap<String, SystemOneAnswer>,
}

#[derive(Deserialize)]
struct SystemOneAnswer {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    probabilities: Option<BTreeMap<String, f64>>,
}

impl SystemOneAnswer {
    /// A one-line account of the distribution, for the route status line.
    fn rationale(&self, label: &str, confidence: f64) -> String {
        let Some(probabilities) = &self.probabilities else {
            return format!(
                "systemone chose {label} at {:.0}% confidence",
                confidence * 100.0
            );
        };
        let mut ranked = probabilities.iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let next = ranked
            .iter()
            .find(|(candidate, _)| candidate.as_str() != label)
            .map(|(candidate, probability)| {
                format!("; next {candidate} {:.0}%", *probability * 100.0)
            })
            .unwrap_or_default();
        format!(
            "systemone chose {label} at {:.0}% confidence{next}",
            confidence * 100.0
        )
    }
}

/// Convenience for tests and tools that need an owned label map.
#[must_use]
pub fn labels(
    entries: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> BTreeMap<String, String> {
    entries
        .into_iter()
        .map(|(label, description)| (label.to_owned(), description.to_owned()))
        .collect()
}

fn slice_is_empty<T>(slice: &[T]) -> bool {
    slice.is_empty()
}

#[cfg(test)]
mod tests {
    use super::LlamaClient;

    #[test]
    fn endpoints_do_not_duplicate_the_version_segment() {
        let rooted = LlamaClient::new("http://127.0.0.1:8080".parse().unwrap(), "model").unwrap();
        let versioned =
            LlamaClient::new("http://127.0.0.1:8080/v1".parse().unwrap(), "model").unwrap();
        assert_eq!(
            rooted.endpoint_for("v1/chat/completions").as_str(),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
        assert_eq!(
            versioned.endpoint_for("v1/chat/completions").as_str(),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
        assert_eq!(
            versioned.endpoint_for("v1/models").as_str(),
            "http://127.0.0.1:8080/v1/models"
        );
    }
}
