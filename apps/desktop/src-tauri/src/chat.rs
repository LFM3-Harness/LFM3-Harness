//! Chat-first cowork runtime.
//!
//! One bounded conversation turn streams from the local text model, may call a
//! small workspace tool set, and stops at every write for explicit user
//! consent. Chat state never touches the verified run journal.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use lfm3_harness::{
    chat::{
        check_evidence, check_write_gate, fallback_tool_call, route_request,
        system_message_with_route, to_model_messages, tool_definitions, ChatCapabilities,
        ChatRoute, HarnessDecision,
    },
    model::{
        ChatMessage, ChatRequest, ContentPart, InputAudio, ModelError, ModelKey, ModelManager,
    },
    AttemptId, CancellationContext, ChatActionRecord, ChatMessageRecord, ChatRole, ChatSession,
    ChatSessionView, ChatStore, ProcessSupervisor, ToolName, ToolRegistry, ToolRequest,
};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};

use crate::engine::EngineError;

const MAX_TURN_MESSAGES: u32 = 240;
// The text model is served with a 32,000-token window. Roughly four characters
// per token leaves headroom for the system prompt, new tool results, and the
// 1,024-token reply while still letting long sessions keep their history.
const HISTORY_CHARS: usize = 96_000;
const MAX_ROUNDS: usize = 12;
const MAX_INPUT_CHARS: usize = 32_000;
const MAX_AUDIO_BASE64: usize = 48 * 1024 * 1024;
const MAX_IMAGE_BASE64: usize = 32 * 1024 * 1024;
const MAX_SPEECH_CHARS: usize = 1_500;
const APPROVAL_TTL: Duration = Duration::from_secs(600);
const TURN_TIMEOUT: Duration = Duration::from_secs(900);

/// Receives every live chat event. The desktop shell installs a Tauri emitter;
/// tests install a no-op or a channel.
pub type ChatEmitter = Arc<dyn Fn(ChatEnvelope) + Send + Sync>;

/// One live event tagged with its conversation.
#[derive(Clone, Debug, Serialize)]
pub struct ChatEnvelope {
    pub session_id: String,
    #[serde(flatten)]
    pub event: ChatEvent,
}

/// A locally synthesized spoken rendering of one assistant answer.
#[derive(Clone, Debug, Serialize)]
pub struct SpeechReply {
    pub audio_base64: String,
    pub format: String,
    pub backend: String,
    pub note: String,
}

/// Paths for LFM3 speech output. The same audio weight and projector that
/// listen also speak; `llama-tts` renders the WAV locally.
#[derive(Clone, Debug, Default)]
pub struct SpeechConfig {
    pub executable: Option<PathBuf>,
    pub weight: Option<PathBuf>,
    pub projector: Option<PathBuf>,
}

impl SpeechConfig {
    /// True only when the binary, the weight, and the audio projector all
    /// exist, so the health report never advertises a voice it cannot run.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.executable.as_ref().is_some_and(|path| path.is_file())
            && self.weight.as_ref().is_some_and(|path| path.is_file())
            && self.projector.as_ref().is_some_and(|path| path.is_file())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatEvent {
    UserMessage {
        message: ChatMessageRecord,
    },
    SessionUpdated {
        session: ChatSession,
    },
    AssistantStart {
        message: ChatMessageRecord,
    },
    AssistantDelta {
        message_id: String,
        delta: String,
    },
    AssistantDone {
        message: ChatMessageRecord,
    },
    Route {
        route: ChatRoute,
    },
    /// One typed answer from the decision model inside this turn.
    Decision {
        decision: HarnessDecision,
    },
    ActionRequested {
        action: ChatActionRecord,
    },
    ActionResolved {
        action: ChatActionRecord,
    },
    ToolResult {
        message: ChatMessageRecord,
    },
    Status {
        state: String,
        detail: Option<String>,
    },
    Error {
        message: String,
    },
    Done,
}

struct Inner {
    store: ChatStore,
    workspace: RwLock<PathBuf>,
    models: RwLock<Option<ModelManager>>,
    supervisor: ProcessSupervisor,
    speech: RwLock<SpeechConfig>,
    /// Serializes a whole turn against a runtime swap, so a download can never
    /// stop a server that is still answering.
    runtime_gate: Mutex<()>,
    speech_gate: Mutex<()>,
    active: Mutex<Option<ActiveTurn>>,
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    emitter: std::sync::Mutex<ChatEmitter>,
}

struct ActiveTurn {
    session_id: String,
    cancel: CancellationContext,
}

enum ApprovalOutcome {
    Approved,
    Denied,
    Cancelled,
}

/// Cloneable handle to the one chat runtime on this engine.
#[derive(Clone)]
pub struct ChatRuntime {
    inner: Arc<Inner>,
}

impl ChatRuntime {
    pub fn open(
        data_dir: &Path,
        workspace: PathBuf,
        models: Option<ModelManager>,
        supervisor: ProcessSupervisor,
        speech: SpeechConfig,
    ) -> Result<Self, EngineError> {
        let store = ChatStore::open(data_dir.join("chat.db"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                workspace: RwLock::new(workspace),
                models: RwLock::new(models),
                supervisor,
                speech: RwLock::new(speech),
                runtime_gate: Mutex::new(()),
                speech_gate: Mutex::new(()),
                active: Mutex::new(None),
                approvals: Mutex::new(HashMap::new()),
                emitter: std::sync::Mutex::new(Arc::new(|_| {})),
            }),
        })
    }

    pub fn set_emitter(&self, emitter: ChatEmitter) {
        if let Ok(mut slot) = self.inner.emitter.lock() {
            *slot = emitter;
        }
    }

    #[must_use]
    pub fn store(&self) -> &ChatStore {
        &self.inner.store
    }

    #[must_use]
    pub fn workspace(&self) -> PathBuf {
        read(&self.inner.workspace).clone()
    }

    /// The currently installed manager. A turn copies this once at its start so
    /// a later reload cannot change the servers underneath it.
    #[must_use]
    pub fn models(&self) -> Option<ModelManager> {
        read(&self.inner.models).clone()
    }

    fn speech_config(&self) -> SpeechConfig {
        read(&self.inner.speech).clone()
    }

    #[must_use]
    pub async fn is_busy(&self) -> bool {
        self.inner.active.lock().await.is_some()
    }

    /// Points the next turn at a different folder. Refused while a turn is
    /// live; the running turn keeps the folder it started with.
    pub async fn set_workspace(&self, path: PathBuf) -> Result<(), EngineError> {
        let _gate = self
            .inner
            .runtime_gate
            .try_lock()
            .map_err(|_| EngineError::Conflict("a chat turn is still answering".to_owned()))?;
        *write(&self.inner.workspace) = path;
        Ok(())
    }

    /// Replaces the managers a new turn will use. The gate makes this wait-free
    /// when a turn is live: the caller retries once the turn finishes.
    pub async fn replace_runtime(
        &self,
        models: Option<ModelManager>,
        speech: SpeechConfig,
    ) -> Result<(), EngineError> {
        let _gate = self
            .inner
            .runtime_gate
            .try_lock()
            .map_err(|_| EngineError::Conflict("a chat turn is still answering".to_owned()))?;
        *write(&self.inner.models) = models;
        *write(&self.inner.speech) = speech;
        Ok(())
    }

    pub fn list_sessions(&self, limit: u32) -> Result<Vec<ChatSession>, EngineError> {
        Ok(self.inner.store.list_sessions(limit)?)
    }

    pub fn create_session(&self, title: Option<&str>) -> Result<ChatSession, EngineError> {
        Ok(self.inner.store.create_session(title)?)
    }

    pub fn session_view(&self, session_id: &str) -> Result<ChatSessionView, EngineError> {
        Ok(self.inner.store.view(session_id)?)
    }

    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<(), EngineError> {
        self.inner.store.rename_session(session_id, title)?;
        Ok(())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<(), EngineError> {
        {
            let mut active = self.inner.active.try_lock().map_err(|_| {
                EngineError::Conflict("a chat turn is still active on this engine".to_owned())
            })?;
            if active
                .as_ref()
                .is_some_and(|turn| turn.session_id == session_id)
            {
                turn_cancel(&mut active);
            }
        }
        self.inner.store.delete_session(session_id)?;
        Ok(())
    }

    /// Appends the person's message and starts one bounded turn.
    pub async fn send(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<ChatMessageRecord, EngineError> {
        let content = content.trim();
        if content.is_empty() {
            return Err(EngineError::InvalidInput(
                "a chat message cannot be empty".to_owned(),
            ));
        }
        if content.chars().count() > MAX_INPUT_CHARS {
            return Err(EngineError::InvalidInput(format!(
                "a chat message is limited to {MAX_INPUT_CHARS} characters"
            )));
        }
        self.inner.store.session(session_id)?;
        let cancel = {
            let mut active = self.inner.active.lock().await;
            if let Some(turn) = active.as_ref() {
                return Err(EngineError::Conflict(format!(
                    "chat session {} is still answering; stop it or wait",
                    turn.session_id
                )));
            }
            let cancel = CancellationContext::new(TURN_TIMEOUT);
            *active = Some(ActiveTurn {
                session_id: session_id.to_owned(),
                cancel: cancel.clone(),
            });
            cancel
        };

        let message = match self.inner.store.append_message(
            session_id,
            ChatRole::User,
            content,
            "complete",
            json!({}),
        ) {
            Ok(message) => message,
            Err(error) => {
                clear_active(&self.inner, session_id).await;
                return Err(error.into());
            }
        };
        let _ = self.inner.store.maybe_retitle(session_id, content);
        self.emit(
            session_id,
            ChatEvent::UserMessage {
                message: message.clone(),
            },
        );
        if let Ok(session) = self.inner.store.session(session_id) {
            self.emit(session_id, ChatEvent::SessionUpdated { session });
        }

        let runtime = self.clone();
        let session = session_id.to_owned();
        tokio::spawn(async move {
            runtime.drive_turn(&session, cancel).await;
        });
        Ok(message)
    }

    /// Stops the live turn. The partial answer is preserved as cancelled.
    pub async fn cancel(&self, session_id: &str) -> Result<(), EngineError> {
        let active = self.inner.active.lock().await;
        match active.as_ref() {
            Some(turn) if turn.session_id == session_id => {
                turn.cancel.cancel();
                Ok(())
            }
            Some(_) => Err(EngineError::Conflict(
                "a different chat session is answering".to_owned(),
            )),
            None => Ok(()),
        }
    }

    /// Resolves a pending write/edit card. Unknown or expired cards fail closed.
    pub async fn decide_action(&self, action_id: &str, allow: bool) -> Result<(), EngineError> {
        let sender = self
            .inner
            .approvals
            .lock()
            .await
            .remove(action_id)
            .ok_or_else(|| {
                EngineError::NotFound(format!("chat action {action_id:?} is not pending"))
            })?;
        sender
            .send(allow)
            .map_err(|_| EngineError::Conflict("the chat turn already ended".to_owned()))
    }

    /// Transcribes one recorded WAV clip with the local audio model.
    pub async fn transcribe_voice(
        &self,
        audio_base64: &str,
        format: &str,
    ) -> Result<String, EngineError> {
        let format = format.trim().to_ascii_lowercase();
        if !matches!(format.as_str(), "wav" | "mp3") {
            return Err(EngineError::InvalidInput(
                "voice input must be WAV or MP3".to_owned(),
            ));
        }
        if audio_base64.len() > MAX_AUDIO_BASE64 {
            return Err(EngineError::InvalidInput(
                "the voice clip is too large".to_owned(),
            ));
        }
        let bytes = BASE64.decode(audio_base64).map_err(|error| {
            EngineError::InvalidInput(format!("voice clip is not valid base64: {error}"))
        })?;
        if bytes.is_empty() {
            return Err(EngineError::InvalidInput(
                "the voice clip is empty".to_owned(),
            ));
        }
        let models = self.models().ok_or_else(|| {
            EngineError::Setup(
                "the local audio model is not available; check the model folder".to_owned(),
            )
        })?;
        let cancel = CancellationContext::new(Duration::from_secs(120));
        let lease = models.acquire(ModelKey::Audio, &cancel.child()).await?;
        let request = ChatRequest::new(vec![
            ChatMessage::system(
                "You are a speech transcription engine. Transcribe the supplied audio \
                 verbatim in its original language. Return only the transcript text, with \
                 no commentary, labels, or quotation marks. If the audio is silent, return \
                 an empty string.",
            ),
            ChatMessage::user_parts(vec![ContentPart::InputAudio {
                input_audio: InputAudio {
                    data: audio_base64.to_owned(),
                    format,
                },
            }]),
        ])
        .with_temperature(0.0)
        .with_max_tokens(1024);
        let response = lease.chat(request, &cancel.child()).await?;
        let text = response.content().unwrap_or("").trim().to_owned();
        if text.is_empty() {
            return Err(EngineError::InvalidInput(
                "no speech was recognised in the clip".to_owned(),
            ));
        }
        // The transcript belongs in the composer; it is not written to disk.
        Ok(text)
    }

    /// Stores one user-attached image inside the workspace so the local vision
    /// tool can inspect it. The person chose this file; this is not a model
    /// write and it never touches the verified run journal.
    pub fn attach_image(&self, name: &str, data_base64: &str) -> Result<String, EngineError> {
        if data_base64.len() > MAX_IMAGE_BASE64 {
            return Err(EngineError::InvalidInput(
                "the attached image is too large".to_owned(),
            ));
        }
        let bytes = BASE64.decode(data_base64).map_err(|error| {
            EngineError::InvalidInput(format!("the image is not valid base64: {error}"))
        })?;
        if bytes.is_empty() {
            return Err(EngineError::InvalidInput(
                "the attached image is empty".to_owned(),
            ));
        }
        if bytes.len() > 20 * 1024 * 1024 {
            return Err(EngineError::InvalidInput(
                "the attached image is larger than 20 MB".to_owned(),
            ));
        }
        let requested = Path::new(name);
        let extension = requested
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
            return Err(EngineError::InvalidInput(
                "attach a PNG, JPEG, WebP, or GIF image".to_owned(),
            ));
        }
        let stem: String = requested
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("image")
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            .take(40)
            .collect();
        let stem = if stem.is_empty() {
            "image".to_owned()
        } else {
            stem
        };
        let relative = format!(
            ".attachments/{}-{}.{}",
            uuid::Uuid::new_v4().simple(),
            stem,
            extension
        );
        let path = self.workspace().join(&relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                EngineError::Setup(format!("could not create the attachment folder: {error}"))
            })?;
        }
        std::fs::write(&path, &bytes).map_err(|error| {
            EngineError::Setup(format!("could not store the attached image: {error}"))
        })?;
        Ok(relative)
    }

    /// Reads one assistant answer aloud with the local LFM3 voice.
    ///
    /// LFM3-Audio-2B is an any-to-any model: the same weight and projector that
    /// transcribe speech also synthesize it through `llama-tts`. The pinned
    /// b11960 runtime and the installed files are qualified by the model card.
    /// If that pair is absent, Windows falls back to the operating-system
    /// voice and says so. Nothing is uploaded and no network is consulted.
    pub async fn synthesize_speech(&self, text: &str) -> Result<SpeechReply, EngineError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(EngineError::InvalidInput(
                "there is no text to speak".to_owned(),
            ));
        }
        if text.chars().count() > MAX_SPEECH_CHARS {
            return Err(EngineError::InvalidInput(format!(
                "spoken replies are limited to {MAX_SPEECH_CHARS} characters"
            )));
        }
        let speech = self.speech_config();
        if speech.ready() {
            // One short-lived llama-tts process at a time; it loads its own
            // copy of the audio weight and exits after writing the WAV.
            let _gate = self.inner.speech_gate.lock().await;
            let supervisor = self.inner.supervisor.clone();
            let owned = text.to_owned();
            let audio =
                tokio::task::spawn_blocking(move || synthesize_lfm3(&speech, &supervisor, &owned))
                    .await
                    .map_err(|error| {
                        EngineError::Setup(format!("the LFM3 voice task failed: {error}"))
                    })??;
            return Ok(SpeechReply {
                audio_base64: audio,
                format: "wav".to_owned(),
                backend: "lfm3-tts".to_owned(),
                note: "Local LFM3-Audio-2B voice. Speech is synthesized and played on this \
                       machine; nothing is uploaded."
                    .to_owned(),
            });
        }
        #[cfg(not(windows))]
        {
            return Err(EngineError::Setup(
                "LFM3 speech output is not configured: llama-tts or the audio weights are missing"
                    .to_owned(),
            ));
        }
        #[cfg(windows)]
        {
            let owned = text.to_owned();
            let audio = tokio::task::spawn_blocking(move || synthesize_windows(&owned))
                .await
                .map_err(|error| {
                    EngineError::Setup(format!("the local voice task failed: {error}"))
                })??;
            Ok(SpeechReply {
                audio_base64: audio,
                format: "wav".to_owned(),
                backend: "system-sapi".to_owned(),
                note: "LFM3 speech output is not configured, so this reply uses the local \
                       operating-system voice. Nothing is uploaded."
                    .to_owned(),
            })
        }
    }

    fn capabilities(models: Option<&ModelManager>) -> ChatCapabilities {
        models.map_or(ChatCapabilities::default(), |models| ChatCapabilities {
            vision: models.has_model(ModelKey::Vision),
            audio: models.has_model(ModelKey::Audio),
        })
    }

    fn extra_tool_hint(&self, models: Option<&ModelManager>) -> String {
        let capabilities = Self::capabilities(models);
        let mut hints = Vec::new();
        if capabilities.vision {
            hints.push("look");
        }
        if capabilities.audio {
            hints.push("transcribe");
        }
        if hints.is_empty() {
            String::new()
        } else {
            format!(", {}", hints.join(", "))
        }
    }

    async fn drive_turn(&self, session_id: &str, cancel: CancellationContext) {
        // Hold the swap gate for the whole turn so downloaded weights can only
        // replace servers between turns, never underneath one.
        let _gate = self.inner.runtime_gate.lock().await;
        let outcome = self.run_loop(session_id, &cancel).await;
        if let Err(error) = outcome {
            let message = error.to_string();
            self.emit(session_id, ChatEvent::Error { message });
        }
        self.emit(session_id, ChatEvent::Done);
        clear_active(&self.inner, session_id).await;
    }

    async fn run_loop(
        &self,
        session_id: &str,
        cancel: &CancellationContext,
    ) -> Result<(), EngineError> {
        let Some(models) = self.models() else {
            return Err(EngineError::Setup(
                "the local text model is not available; check the model folder and llama.cpp"
                    .to_owned(),
            ));
        };
        let workspace = self.workspace();
        self.emit(
            session_id,
            ChatEvent::Status {
                state: "loading".to_owned(),
                detail: Some("Starting the local text model".to_owned()),
            },
        );

        let records = self.inner.store.messages(session_id, MAX_TURN_MESSAGES)?;
        let last_user = records
            .iter()
            .rev()
            .find(|record| record.role == ChatRole::User)
            .map(|record| record.content.as_str())
            .unwrap_or_default();
        let capabilities = Self::capabilities(Some(&models));
        let route = if models.has_model(ModelKey::D1) {
            self.emit(
                session_id,
                ChatEvent::Status {
                    state: "routing".to_owned(),
                    detail: Some("Decision model is choosing the task route and tools".to_owned()),
                },
            );
            route_request(&models, last_user, &cancel.child()).await
        } else {
            ChatRoute::heuristic(last_user)
        };
        self.emit(
            session_id,
            ChatEvent::Route {
                route: route.clone(),
            },
        );
        let route_meta = serde_json::to_value(&route).unwrap_or(Value::Null);

        let lease = models.acquire(ModelKey::Text, &cancel.child()).await?;

        let route_note = format!(
            "{} — {}",
            route.summary(),
            route.reason.chars().take(400).collect::<String>()
        );
        let mut messages = vec![system_message_with_route(
            &workspace.display().to_string(),
            &route_note,
        )];
        messages.extend(to_model_messages(&records, HISTORY_CHARS));

        // The evidence gate runs at most once per turn, after the first batch
        // of read-only observations. It never blocks the turn; it only adds a
        // note the acting model can use.
        let mut evidence_checked = false;

        for round in 0..MAX_ROUNDS {
            if cancel.is_cancelled() {
                return Ok(());
            }
            self.emit(
                session_id,
                ChatEvent::Status {
                    state: "thinking".to_owned(),
                    detail: None,
                },
            );
            let meta = if round == 0 {
                json!({ "route": route_meta.clone() })
            } else {
                json!({})
            };
            let assistant = self.inner.store.append_message(
                session_id,
                ChatRole::Assistant,
                "",
                "streaming",
                meta,
            )?;
            self.emit(
                session_id,
                ChatEvent::AssistantStart {
                    message: assistant.clone(),
                },
            );

            let request = ChatRequest::new(messages.clone())
                .with_tools(tool_definitions(capabilities))
                .with_temperature(0.3)
                .with_max_tokens(1024);
            let mut streamed = String::new();
            let mut on_delta = |delta: &str| {
                streamed.push_str(delta);
                self.emit(
                    session_id,
                    ChatEvent::AssistantDelta {
                        message_id: assistant.id.clone(),
                        delta: delta.to_owned(),
                    },
                );
            };
            let response = match lease
                .chat_stream(request, &cancel.child(), &mut on_delta)
                .await
            {
                Ok(response) => response,
                Err(ModelError::Cancelled(_)) => {
                    let record = self.inner.store.finish_message(
                        &assistant.id,
                        &streamed,
                        "cancelled",
                        json!({}),
                    )?;
                    self.emit(session_id, ChatEvent::AssistantDone { message: record });
                    return Ok(());
                }
                Err(error) => {
                    let text = if streamed.is_empty() {
                        format!("The local model failed: {error}")
                    } else {
                        format!("{streamed}\n\n[model error: {error}]")
                    };
                    let record = self.inner.store.finish_message(
                        &assistant.id,
                        &text,
                        "error",
                        json!({}),
                    )?;
                    self.emit(session_id, ChatEvent::AssistantDone { message: record });
                    return Err(error.into());
                }
            };

            let mut content = response.content().unwrap_or("").trim().to_owned();
            let mut tool_calls = response.tool_calls().to_vec();
            if tool_calls.is_empty() {
                if let Some((cleaned, call)) = fallback_tool_call(&content) {
                    content = cleaned;
                    tool_calls.push(call);
                }
            }

            if tool_calls.is_empty() {
                let meta = if round == 0 {
                    json!({ "route": route_meta.clone() })
                } else {
                    json!({})
                };
                let record =
                    self.inner
                        .store
                        .finish_message(&assistant.id, &content, "complete", meta)?;
                self.emit(session_id, ChatEvent::AssistantDone { message: record });
                return Ok(());
            }

            let meta = if round == 0 {
                json!({ "tool_calls": &tool_calls, "route": route_meta.clone() })
            } else {
                json!({ "tool_calls": &tool_calls })
            };
            let record =
                self.inner
                    .store
                    .finish_message(&assistant.id, &content, "complete", meta)?;
            self.emit(session_id, ChatEvent::AssistantDone { message: record });

            messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content: content.clone().into(),
                name: None,
                tool_call_id: None,
                tool_calls: tool_calls.clone(),
            });

            let mut observations = Vec::new();
            for call in tool_calls {
                let read_only = !matches!(call.function.name.as_str(), "write_file" | "edit_file");
                let result = match self
                    .execute_tool_call(
                        session_id,
                        &assistant.id,
                        &call,
                        &workspace,
                        &models,
                        last_user,
                        cancel,
                    )
                    .await
                {
                    Ok(result) => result,
                    Err(_) if cancel.is_cancelled() => return Ok(()),
                    Err(error) => return Err(error),
                };
                let record = self.inner.store.append_message(
                    session_id,
                    ChatRole::Tool,
                    &result,
                    "complete",
                    json!({
                        "tool_call_id": call.id,
                        "tool": call.function.name,
                    }),
                )?;
                self.emit(session_id, ChatEvent::ToolResult { message: record });
                if read_only {
                    observations.push((call.function.name.clone(), result.clone()));
                }
                messages.push(ChatMessage::tool(call.id.clone(), result));
            }

            if !evidence_checked && !observations.is_empty() && models.has_model(ModelKey::D1) {
                evidence_checked = true;
                self.emit(
                    session_id,
                    ChatEvent::Status {
                        state: "checking_evidence".to_owned(),
                        detail: Some(
                            "Decision model is checking whether the observations answer the request"
                                .to_owned(),
                        ),
                    },
                );
                match check_evidence(&models, last_user, &observations, &cancel.child()).await {
                    Ok(Some(decision)) => {
                        self.emit(
                            session_id,
                            ChatEvent::Decision {
                                decision: decision.clone(),
                            },
                        );
                        if let Some(note) = decision.evidence_note() {
                            messages.push(ChatMessage::system(note));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!(error = %error, "evidence gate skipped");
                    }
                }
            }

            if round + 1 == MAX_ROUNDS {
                let notice = format!(
                    "I stopped after {MAX_ROUNDS} tool rounds to keep this turn bounded. \
                     Tell me how you want to continue."
                );
                let record = self.inner.store.append_message(
                    session_id,
                    ChatRole::Assistant,
                    &notice,
                    "complete",
                    json!({}),
                )?;
                self.emit(session_id, ChatEvent::AssistantDone { message: record });
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_call(
        &self,
        session_id: &str,
        assistant_message_id: &str,
        call: &lfm3_harness::ToolCall,
        workspace: &Path,
        models: &ModelManager,
        user_request: &str,
        cancel: &CancellationContext,
    ) -> Result<String, EngineError> {
        let Some(tool) = ToolName::parse(&call.function.name) else {
            return Ok(format!(
                "Tool error: {:?} is not a chat tool. Available tools: list_files, read_file, \
                 search_files, related_files, write_file, edit_file{}.",
                call.function.name,
                self.extra_tool_hint(Some(models))
            ));
        };
        let capabilities = Self::capabilities(Some(models));
        let allowed = matches!(
            tool,
            ToolName::ListFiles
                | ToolName::ReadFile
                | ToolName::WriteFile
                | ToolName::EditFile
                | ToolName::SearchFiles
                | ToolName::RelatedFiles
        ) || (tool == ToolName::Look && capabilities.vision)
            || (tool == ToolName::Transcribe && capabilities.audio);
        if !allowed {
            return Ok(format!(
                "Tool error: {} is not available in this chat{}. \
                 Use the tools that are listed.",
                tool.as_str(),
                if tool == ToolName::Look {
                    " because the local vision model is not installed"
                } else if tool == ToolName::Transcribe {
                    " because the local audio model is not installed"
                } else {
                    ""
                }
            ));
        }
        let arguments: Value = match serde_json::from_str(&call.function.arguments) {
            Ok(value) => value,
            Err(error) => {
                return Ok(format!(
                    "Tool error: arguments were not valid JSON ({error}). Retry with a JSON object."
                ))
            }
        };
        if !arguments.is_object() {
            return Ok("Tool error: arguments must be a JSON object.".to_owned());
        }

        let mut registry = ToolRegistry::new(workspace)?
            .with_supervisor(self.inner.supervisor.clone())
            .with_models(models.clone());
        for granted in [
            ToolName::ListFiles,
            ToolName::ReadFile,
            ToolName::WriteFile,
            ToolName::EditFile,
            ToolName::SearchFiles,
            ToolName::RelatedFiles,
        ] {
            registry = registry.grant(granted);
        }
        if capabilities.vision {
            registry = registry.grant(ToolName::Look);
        }
        if capabilities.audio {
            registry = registry.grant(ToolName::Transcribe);
        }

        if matches!(tool, ToolName::WriteFile | ToolName::EditFile) {
            // The decision model checks whether the proposed change matches the
            // person's request *before* an approval card is created. A hold
            // never overrides consent: it only keeps a mismatched change away
            // from the user in the first place.
            if models.has_model(ModelKey::D1) {
                self.emit(
                    session_id,
                    ChatEvent::Status {
                        state: "preflight".to_owned(),
                        detail: Some(
                            "Decision model is checking the proposed change against your request"
                                .to_owned(),
                        ),
                    },
                );
                match check_write_gate(models, user_request, tool, &arguments, &cancel.child())
                    .await
                {
                    Ok(Some(decision)) => {
                        self.emit(
                            session_id,
                            ChatEvent::Decision {
                                decision: decision.clone(),
                            },
                        );
                        if let Some(hold) = decision.hold_message() {
                            return Ok(hold);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!(error = %error, "write gate skipped");
                    }
                }
            }
            let summary = summarize(tool, &arguments);
            let action = self.inner.store.create_action(
                session_id,
                assistant_message_id,
                tool.as_str(),
                arguments.clone(),
                &summary,
            )?;
            self.emit(
                session_id,
                ChatEvent::ActionRequested {
                    action: action.clone(),
                },
            );
            self.emit(
                session_id,
                ChatEvent::Status {
                    state: "waiting_for_approval".to_owned(),
                    detail: Some(summary.clone()),
                },
            );
            match self.await_approval(&action, cancel).await? {
                ApprovalOutcome::Denied => {
                    let action = self.inner.store.resolve_action(&action.id, "denied")?;
                    self.emit(
                        session_id,
                        ChatEvent::ActionResolved {
                            action: action.clone(),
                        },
                    );
                    return Ok(
                        "The user denied this change. Do not retry it unless they ask.".to_owned(),
                    );
                }
                ApprovalOutcome::Cancelled => {
                    let action = self.inner.store.resolve_action(&action.id, "expired")?;
                    self.emit(
                        session_id,
                        ChatEvent::ActionResolved {
                            action: action.clone(),
                        },
                    );
                    return Err(EngineError::Conflict(
                        "the chat turn was cancelled".to_owned(),
                    ));
                }
                ApprovalOutcome::Approved => {
                    self.inner.store.resolve_action(&action.id, "approved")?;
                }
            }
            self.emit(
                session_id,
                ChatEvent::Status {
                    state: "applying".to_owned(),
                    detail: Some(summary),
                },
            );
            let outcome = registry
                .execute(
                    ToolRequest::new(AttemptId::new(), call.id.clone(), tool, arguments),
                    &cancel.child(),
                )
                .await;
            let status = if outcome.is_ok() { "applied" } else { "failed" };
            if let Ok(action) = self.inner.store.resolve_action(&action.id, status) {
                self.emit(session_id, ChatEvent::ActionResolved { action });
            }
            return Ok(match outcome {
                Ok(result) => format!("{} succeeded. {}", tool.as_str(), result.output),
                Err(error) => format!("{} failed: {error}", tool.as_str()),
            });
        }

        self.emit(
            session_id,
            ChatEvent::Status {
                state: "tool".to_owned(),
                detail: Some(format!(
                    "Using {} on {}",
                    tool.as_str(),
                    arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("the workspace")
                )),
            },
        );
        let outcome = registry
            .execute(
                ToolRequest::new(AttemptId::new(), call.id.clone(), tool, arguments),
                &cancel.child(),
            )
            .await;
        Ok(match outcome {
            Ok(result) => result.output,
            Err(error) => format!("{} failed: {error}", tool.as_str()),
        })
    }

    async fn await_approval(
        &self,
        action: &ChatActionRecord,
        cancel: &CancellationContext,
    ) -> Result<ApprovalOutcome, EngineError> {
        let (sender, receiver) = oneshot::channel();
        self.inner
            .approvals
            .lock()
            .await
            .insert(action.id.clone(), sender);
        let outcome = tokio::select! {
            biased;
            reason = cancel.cancelled_or_expired() => {
                let _ = reason;
                ApprovalOutcome::Cancelled
            }
            result = tokio::time::timeout(APPROVAL_TTL, receiver) => {
                match result {
                    Ok(Ok(true)) => ApprovalOutcome::Approved,
                    Ok(Ok(false)) | Ok(Err(_)) | Err(_) => ApprovalOutcome::Denied,
                }
            }
        };
        self.inner.approvals.lock().await.remove(&action.id);
        Ok(outcome)
    }

    fn emit(&self, session_id: &str, event: ChatEvent) {
        let emitter = match self.inner.emitter.lock() {
            Ok(slot) => Arc::clone(&slot),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        };
        emitter(ChatEnvelope {
            session_id: session_id.to_owned(),
            event,
        });
    }
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn summarize(tool: ToolName, arguments: &Value) -> String {
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("(no path)");
    match tool {
        ToolName::WriteFile => {
            let bytes = arguments
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, |content| content.len());
            format!("Write {path} ({bytes} bytes)")
        }
        ToolName::EditFile => format!("Edit {path}"),
        other => format!("{} {path}", other.as_str()),
    }
}

fn turn_cancel(slot: &mut Option<ActiveTurn>) {
    if let Some(turn) = slot.take() {
        turn.cancel.cancel();
    }
}

async fn clear_active(inner: &Inner, session_id: &str) {
    let mut slot = inner.active.lock().await;
    if slot
        .as_ref()
        .is_some_and(|turn| turn.session_id == session_id)
    {
        *slot = None;
    }
}

/// Renders LFM3 speech with `llama-tts` and returns base64 WAV.
///
/// The model card's sampling parameters for audio are fixed here rather than
/// exposed as a knob: temperature 1.0, top-k 128, repetition penalty 1.1.
/// `female` is the documented default voice.
fn synthesize_lfm3(
    speech: &SpeechConfig,
    supervisor: &ProcessSupervisor,
    text: &str,
) -> Result<String, EngineError> {
    use std::{io::Read, process::Command, thread, time::Instant};

    let executable = speech.executable.as_ref().ok_or_else(|| {
        EngineError::Setup("llama-tts is not configured for LFM3 speech output".to_owned())
    })?;
    let weight = speech
        .weight
        .as_ref()
        .ok_or_else(|| EngineError::Setup("the LFM3 audio weight is not configured".to_owned()))?;
    let projector = speech.projector.as_ref().ok_or_else(|| {
        EngineError::Setup("the LFM3 audio projector is not configured".to_owned())
    })?;

    let directory =
        std::env::temp_dir().join(format!("lfm3-tts-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&directory).map_err(|error| {
        EngineError::Setup(format!("could not create the speech workspace: {error}"))
    })?;
    let output = directory.join("speech.wav");
    let result = (|| {
        let mut command = Command::new(executable);
        command
            .arg("-m")
            .arg(weight)
            .arg("-mm")
            .arg(projector)
            .arg("-p")
            .arg(text)
            .arg("--tts-voice")
            .arg("female")
            .arg("--temp")
            .arg("1.0")
            .arg("--top-k")
            .arg("128")
            .arg("--repeat-penalty")
            .arg("1.1")
            .arg("-o")
            .arg(&output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let child = supervisor.spawn(&mut command).map_err(|error| {
            EngineError::Setup(format!("the LFM3 voice could not start: {error}"))
        })?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    let _ = child.kill();
                    return Err(EngineError::Setup(
                        "the LFM3 voice took longer than 120 seconds".to_owned(),
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    return Err(EngineError::Setup(format!(
                        "the LFM3 voice could not be observed: {error}"
                    )));
                }
            }
        };
        if !status.success() {
            let mut detail = String::new();
            if let Some(mut stderr) = child.take_stderr() {
                let _ = stderr.read_to_string(&mut detail);
            }
            return Err(EngineError::Setup(format!(
                "the LFM3 voice failed ({}): {}",
                status,
                detail.trim().chars().take(400).collect::<String>()
            )));
        }
        let bytes = std::fs::read(&output).map_err(|error| {
            EngineError::Setup(format!("the LFM3 voice produced no audio: {error}"))
        })?;
        if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err(EngineError::Setup(
                "the LFM3 voice produced no valid WAV audio".to_owned(),
            ));
        }
        Ok(BASE64.encode(bytes))
    })();
    let _ = std::fs::remove_dir_all(&directory);
    result
}

#[cfg(windows)]
fn synthesize_windows(text: &str) -> Result<String, EngineError> {
    use std::{io::Read, process::Command, thread, time::Instant};

    const SCRIPT: &str = r#"param([string]$InFile, [string]$OutFile)
Add-Type -AssemblyName System.Speech
$text = [System.IO.File]::ReadAllText($InFile, [System.Text.Encoding]::UTF8)
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
$synth.Rate = 0
$synth.Volume = 100
$synth.SetOutputToWaveFile($OutFile)
$synth.Speak($text)
$synth.Dispose()
"#;

    let directory =
        std::env::temp_dir().join(format!("lfm3-speech-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&directory).map_err(|error| {
        EngineError::Setup(format!("could not create the speech workspace: {error}"))
    })?;
    let script = directory.join("speak.ps1");
    let input = directory.join("speech.txt");
    let output = directory.join("speech.wav");
    let result = (|| {
        std::fs::write(&script, SCRIPT)
            .and_then(|_| std::fs::write(&input, text.as_bytes()))
            .map_err(|error| {
                EngineError::Setup(format!("could not prepare the local voice: {error}"))
            })?;
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg("-InFile")
            .arg(&input)
            .arg("-OutFile")
            .arg(&output)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                EngineError::Setup(format!("the local Windows voice is unavailable: {error}"))
            })?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Ok(None) => {
                    let _ = child.kill();
                    return Err(EngineError::Setup(
                        "the local voice took longer than 60 seconds".to_owned(),
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    return Err(EngineError::Setup(format!(
                        "the local voice could not be observed: {error}"
                    )));
                }
            }
        };
        if !status.success() {
            let mut detail = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut detail);
            }
            return Err(EngineError::Setup(format!(
                "the local voice failed ({}): {}",
                status,
                detail.trim().chars().take(400).collect::<String>()
            )));
        }
        let bytes = std::fs::read(&output).map_err(|error| {
            EngineError::Setup(format!("the local voice produced no audio: {error}"))
        })?;
        if bytes.len() < 44 {
            return Err(EngineError::Setup(
                "the local voice produced an empty recording".to_owned(),
            ));
        }
        Ok(BASE64.encode(bytes))
    })();
    let _ = std::fs::remove_dir_all(&directory);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_summaries_name_the_path_and_size() {
        let write = summarize(
            ToolName::WriteFile,
            &json!({"path": "notes.md", "content": "hello"}),
        );
        assert_eq!(write, "Write notes.md (5 bytes)");
        assert_eq!(
            summarize(ToolName::EditFile, &json!({"path": "notes.md"})),
            "Edit notes.md"
        );
        assert_eq!(
            summarize(ToolName::ReadFile, &json!({"path": "notes.md"})),
            "read_file notes.md"
        );
    }

    #[test]
    fn live_events_flatten_their_kind_inside_the_session_envelope() {
        let envelope = ChatEnvelope {
            session_id: "session-1".to_owned(),
            event: ChatEvent::AssistantDelta {
                message_id: "message-1".to_owned(),
                delta: "hello".to_owned(),
            },
        };
        let value = serde_json::to_value(&envelope).expect("serialize chat envelope");
        assert_eq!(value["session_id"], "session-1");
        assert_eq!(value["kind"], "assistant_delta");
        assert_eq!(value["message_id"], "message-1");
        assert_eq!(value["delta"], "hello");
    }

    #[test]
    fn route_events_carry_the_decision_and_the_model_roles() {
        let envelope = ChatEnvelope {
            session_id: "session-1".to_owned(),
            event: ChatEvent::Route {
                route: ChatRoute::heuristic(
                    "Search this folder for the migration plan and write me a report",
                ),
            },
        };
        let value = serde_json::to_value(&envelope).expect("serialize chat envelope");
        assert_eq!(value["kind"], "route");
        assert_eq!(value["route"]["intent"], "report");
        assert_eq!(value["route"]["source"], "heuristic");
        let roles = value["route"]["roles"]
            .as_array()
            .expect("route roles are an array");
        assert!(roles
            .iter()
            .any(|role| role["model"] == "text" && role["used"] == true));
        assert!(roles
            .iter()
            .any(|role| role["model"] == "d1" && role["used"] == false));
    }

    #[test]
    fn speech_requires_the_binary_the_weight_and_the_projector() {
        let directory = tempfile::tempdir().expect("temp dir");
        let executable = directory.path().join("llama-tts.exe");
        let weight = directory.path().join("LFM3-Audio-2B-Q4_K_M.gguf");
        let projector = directory.path().join("mmproj-LFM3-Audio-2B-Q8_0.gguf");

        assert!(!SpeechConfig::default().ready());

        std::fs::write(&executable, b"binary").expect("executable");
        std::fs::write(&weight, b"weights").expect("weight");
        let partial = SpeechConfig {
            executable: Some(executable.clone()),
            weight: Some(weight.clone()),
            projector: None,
        };
        assert!(!partial.ready());

        std::fs::write(&projector, b"projector").expect("projector");
        let complete = SpeechConfig {
            executable: Some(executable),
            weight: Some(weight),
            projector: Some(projector),
        };
        assert!(complete.ready());
    }

    #[test]
    #[ignore = "requires the installed llama-tts binary and the local LFM3-Audio-2B pair"]
    fn lfm3_voice_supervisor_renders_a_valid_wav() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let speech = SpeechConfig {
            executable: Some(root.join("tools/llama/llama-tts.exe")),
            weight: Some(root.join("models/LFM3-Audio-2B-GGUF/LFM3-Audio-2B-Q4_K_M.gguf")),
            projector: Some(root.join("models/LFM3-Audio-2B-GGUF/mmproj-LFM3-Audio-2B-Q8_0.gguf")),
        };
        assert!(
            speech.ready(),
            "install llama-tts and the LFM3-Audio-2B weight/projector under {}",
            root.display()
        );
        let supervisor = ProcessSupervisor::new().expect("supervisor");
        let audio = synthesize_lfm3(&speech, &supervisor, "Hello from the local LFM3 voice.")
            .expect("LFM3 speech");
        let bytes = BASE64.decode(audio).expect("base64 WAV");
        assert!(bytes.len() > 44);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        supervisor.shutdown().expect("shutdown");
    }
}
