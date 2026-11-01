//! Headless multi-model benchmark.
//!
//! This probe boots the same embedded engine the window uses, then exercises
//! the real decision router, the chat loop, tool calls, write approvals, the
//! vision model, and the audio model. It never launches a webview and never
//! imitates a model.
//!
//! Invoked by the benchmark runner as:
//!
//! ```text
//! lfm3-desktop --benchmark-chat <cases.json> <raw-results.json>
//! ```
//!
//! The runner owns the workspace, data directory, and grading; this probe only
//! drives production code and records what happened.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use lfm3_harness::{
    chat::{route_request, ChatRoute},
    model::ModelKey,
    CancellationContext, ChatRole,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    chat::{ChatEnvelope, ChatEvent, ChatRuntime},
    engine::{Engine, EngineError},
};

const ROUTE_TIMEOUT: Duration = Duration::from_secs(180);
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
const CANCEL_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct Cases {
    #[serde(default)]
    router: Vec<RouterCase>,
    #[serde(default)]
    audio: Vec<AudioCase>,
    #[serde(default)]
    chat: Vec<ChatCase>,
}

#[derive(Debug, Deserialize)]
struct RouterCase {
    id: String,
    prompt: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AudioCase {
    id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct ChatCase {
    id: String,
    prompt: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    approve: Option<String>,
    /// Image file to attach before the turn; `{image}` in the prompt is
    /// replaced with the workspace-relative attachment path.
    #[serde(default)]
    image: Option<PathBuf>,
    /// Id of an audio case whose WAV is referenced by `{audio}`.
    #[serde(default)]
    audio: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

pub fn run() -> ! {
    let arguments: Vec<String> = std::env::args().collect();
    let Some(flag) = arguments
        .iter()
        .position(|argument| argument == "--benchmark-chat")
    else {
        let _ = writeln_stderr("usage: lfm3-desktop --benchmark-chat <cases.json> <results.json>");
        std::process::exit(2);
    };
    let (Some(cases), Some(output)) = (arguments.get(flag + 1), arguments.get(flag + 2)) else {
        let _ = writeln_stderr("usage: lfm3-desktop --benchmark-chat <cases.json> <results.json>");
        std::process::exit(2);
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => fail(format!("could not start the benchmark runtime: {error}")),
    };
    match runtime.block_on(benchmark(Path::new(cases), Path::new(output))) {
        Ok(()) => std::process::exit(0),
        Err(error) => fail(error),
    }
}

async fn benchmark(cases_path: &Path, output_path: &Path) -> Result<(), String> {
    let raw = fs::read_to_string(cases_path)
        .map_err(|error| format!("read {}: {error}", cases_path.display()))?;
    let cases: Cases = serde_json::from_str(&raw)
        .map_err(|error| format!("parse {}: {error}", cases_path.display()))?;
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);

    let engine = Engine::boot(&missions_dir())
        .await
        .map_err(|error| format!("boot engine: {error}"))?;
    let chat = engine.chat();
    let workspace = chat.workspace();
    fs::create_dir_all(workspace.join("assets"))
        .map_err(|error| format!("create asset folder: {error}"))?;

    let models = engine
        .model_manager()
        .ok_or_else(|| "the engine has no model manager".to_owned())?;
    let mut router = Vec::new();
    for case in &cases.router {
        let heuristic = ChatRoute::heuristic(&case.prompt);
        let cancel = CancellationContext::new(ROUTE_TIMEOUT);
        let started_at = Instant::now();
        let route = route_request(&models, &case.prompt, &cancel.child()).await;
        let _ = writeln_stderr(&format!(
            "router {} heuristic={} final={} source={:?} {}ms",
            case.id,
            heuristic.intent.as_str(),
            route.intent.as_str(),
            route.source,
            started_at.elapsed().as_millis()
        ));
        router.push(json!({
            "id": case.id,
            "prompt": case.prompt,
            "note": case.note,
            "heuristic_intent": heuristic.intent.as_str(),
            "route": route,
            "milliseconds": started_at.elapsed().as_millis(),
        }));
    }

    let mut audio = Vec::new();
    let mut audio_paths: HashMap<String, String> = HashMap::new();
    for case in &cases.audio {
        let record = run_audio_case(chat, &workspace, case).await;
        if let Some(path) = record["wav"].as_str() {
            audio_paths.insert(case.id.clone(), path.to_owned());
        }
        let _ = writeln_stderr(&format!(
            "audio {} backend={} {}ms error={:?}",
            case.id,
            record["backend"].as_str().unwrap_or("-"),
            record["milliseconds"].as_u64().unwrap_or(0),
            record["error"]
                .as_str()
                .or(record["transcription_error"].as_str())
        ));
        audio.push(record);
    }

    let cases_root = cases_path.parent().unwrap_or_else(|| Path::new("."));
    let mut sessions: HashMap<String, String> = HashMap::new();
    let mut chat_records = Vec::new();
    for case in &cases.chat {
        let mut prompt = case.prompt.clone();
        let mut attachment = Value::Null;
        if let Some(image) = &case.image {
            let path = if image.is_absolute() {
                image.clone()
            } else {
                cases_root.join(image)
            };
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("image.png");
            match fs::read(&path) {
                Ok(bytes) => match chat.attach_image(name, &BASE64.encode(bytes)) {
                    Ok(relative) => {
                        prompt = prompt.replace("{image}", &relative);
                        attachment =
                            json!({ "path": relative, "source": path.display().to_string() });
                    }
                    Err(error) => {
                        prompt = prompt.replace("{image}", "(attachment failed)");
                        attachment = json!({ "error": error.to_string() });
                    }
                },
                Err(error) => {
                    prompt = prompt.replace("{image}", "(attachment failed)");
                    attachment = json!({ "error": format!("read {}: {error}", path.display()) });
                }
            }
        }
        if let Some(audio_id) = &case.audio {
            if let Some(path) = audio_paths.get(audio_id) {
                prompt = prompt.replace("{audio}", path);
            } else {
                prompt = prompt.replace("{audio}", "(audio unavailable)");
            }
        }

        let session = match case.session.as_deref() {
            Some(group) => {
                if let Some(id) = sessions.get(group) {
                    id.clone()
                } else {
                    let created = chat
                        .create_session(Some(&format!("bench {group}")))
                        .map_err(|error| format!("create session {group}: {error}"))?;
                    sessions.insert(group.to_owned(), created.id.clone());
                    created.id
                }
            }
            None => {
                chat.create_session(Some(&format!("bench {}", case.id)))
                    .map_err(|error| format!("create session {}: {error}", case.id))?
                    .id
            }
        };
        let mut record = run_chat_case(chat, &workspace, &session, case, &prompt).await;
        record["session"] = Value::String(case.session.clone().unwrap_or_default());
        record["prompt_sent"] = Value::String(prompt);
        record["attachment"] = attachment;
        let _ = writeln_stderr(&format!(
            "chat {} route={} tools={} approvals={} gates={} {}ms ok={}",
            case.id,
            record["route"]["intent"].as_str().unwrap_or("-"),
            record["tool_messages"]
                .as_array()
                .map(|items| items.len())
                .unwrap_or(0),
            record["actions"]
                .as_array()
                .map(|items| items.len())
                .unwrap_or(0),
            record["decisions"]
                .as_array()
                .map(|items| items.len())
                .unwrap_or(0),
            record["milliseconds"].as_u64().unwrap_or(0),
            !record["timed_out"].as_bool().unwrap_or(false)
                && record["sent"].as_bool().unwrap_or(false)
        ));
        chat_records.push(record);
    }

    let report = json!({
        "started_at": started,
        "health": engine.health(),
        "models": engine.models(),
        "router_available": models.has_model(ModelKey::D1),
        "workspace": workspace.display().to_string(),
        "router": router,
        "audio": audio,
        "chat": chat_records,
    });
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&report)
        .map_err(|error| format!("serialize results: {error}"))?;
    fs::write(output_path, text)
        .map_err(|error| format!("write {}: {error}", output_path.display()))?;
    drop(engine);
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

async fn run_audio_case(chat: &ChatRuntime, workspace: &Path, case: &AudioCase) -> Value {
    let started = Instant::now();
    let speech = match chat.synthesize_speech(&case.text).await {
        Ok(speech) => speech,
        Err(error) => {
            return json!({
                "id": case.id,
                "text": case.text,
                "error": format!("synthesis: {error}"),
                "milliseconds": started.elapsed().as_millis(),
            });
        }
    };
    let synthesized_ms = started.elapsed().as_millis();
    let bytes = match BASE64.decode(&speech.audio_base64) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json!({
                "id": case.id,
                "text": case.text,
                "error": format!("decode synthesized WAV: {error}"),
                "milliseconds": started.elapsed().as_millis(),
            });
        }
    };
    let relative = format!("assets/audio-{}.wav", case.id);
    let written = fs::write(workspace.join(&relative), &bytes);
    let transcribed = Instant::now();
    let transcript = chat.transcribe_voice(&speech.audio_base64, "wav").await;
    json!({
        "id": case.id,
        "text": case.text,
        "wav": relative,
        "backend": speech.backend,
        "note": speech.note,
        "wav_bytes": bytes.len(),
        "wav_header": bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "write_error": written.err().map(|error| error.to_string()),
        "synthesis_ms": synthesized_ms,
        "transcription_ms": transcribed.elapsed().as_millis(),
        "transcript": transcript.as_ref().ok(),
        "transcription_error": transcript.err().map(|error| error.to_string()),
        "milliseconds": started.elapsed().as_millis(),
    })
}

async fn run_chat_case(
    chat: &ChatRuntime,
    workspace: &Path,
    session: &str,
    case: &ChatCase,
    prompt: &str,
) -> Value {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    chat.set_emitter(Arc::new(move |event: ChatEnvelope| {
        let _ = sender.send(event);
    }));

    let started = Instant::now();
    let mut route = Value::Null;
    let mut statuses = Vec::new();
    let mut deltas = 0usize;
    let mut assistant_messages = Vec::new();
    let mut tool_messages = Vec::new();
    let mut actions = Vec::new();
    let mut decisions = Vec::new();
    let mut errors = Vec::new();
    let mut final_content = String::new();
    let mut unscripted_action = false;

    let mut sent = false;
    for _ in 0..40 {
        match chat.send(session, prompt).await {
            Ok(_) => {
                sent = true;
                break;
            }
            Err(EngineError::Conflict(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(error) => {
                errors.push(format!("send: {error}"));
                break;
            }
        }
    }

    let mut timed_out = false;
    if sent {
        let receive = async {
            while let Some(envelope) = receiver.recv().await {
                if envelope.session_id != session {
                    continue;
                }
                match envelope.event {
                    ChatEvent::Route { route: value } => {
                        route = serde_json::to_value(value).unwrap_or(Value::Null);
                    }
                    ChatEvent::Status { state, .. } => statuses.push(state),
                    ChatEvent::Decision { decision } => {
                        decisions.push(serde_json::to_value(decision).unwrap_or(Value::Null));
                    }
                    ChatEvent::AssistantDelta { .. } => deltas += 1,
                    ChatEvent::AssistantDone { message } => {
                        final_content = message.content.clone();
                        assistant_messages
                            .push(serde_json::to_value(message).unwrap_or(Value::Null));
                    }
                    ChatEvent::ToolResult { message } => {
                        tool_messages.push(serde_json::to_value(message).unwrap_or(Value::Null));
                    }
                    ChatEvent::ActionRequested { action } => {
                        let allow = case.approve.as_deref() == Some("allow");
                        if case.approve.is_none() {
                            unscripted_action = true;
                        }
                        actions.push(json!({
                            "requested": serde_json::to_value(&action).unwrap_or(Value::Null),
                            "allow": allow,
                        }));
                        if let Err(error) = chat.decide_action(&action.id, allow).await {
                            errors.push(format!("decide action: {error}"));
                        }
                    }
                    ChatEvent::ActionResolved { action } => {
                        actions.push(json!({
                            "resolved": serde_json::to_value(action).unwrap_or(Value::Null),
                        }));
                    }
                    ChatEvent::Error { message } => errors.push(message),
                    ChatEvent::Done => break,
                    _ => {}
                }
            }
        };
        timed_out = tokio::time::timeout(TURN_TIMEOUT, receive).await.is_err();
        if timed_out {
            let _ = chat.cancel(session).await;
            let _ = tokio::time::timeout(CANCEL_GRACE, async {
                while let Some(envelope) = receiver.recv().await {
                    if envelope.session_id == session && matches!(envelope.event, ChatEvent::Done) {
                        break;
                    }
                }
            })
            .await;
        }
    }

    let view = chat.session_view(session).ok();
    let assistant_count = view
        .as_ref()
        .map(|view| {
            view.messages
                .iter()
                .filter(|message| message.role == ChatRole::Assistant)
                .count()
        })
        .unwrap_or(0);
    let tool_count = view
        .as_ref()
        .map(|view| {
            view.messages
                .iter()
                .filter(|message| message.role == ChatRole::Tool)
                .count()
        })
        .unwrap_or(0);
    let statuses_after = view
        .as_ref()
        .map(|view| {
            view.messages
                .iter()
                .map(|message| message.status.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    json!({
        "id": case.id,
        "prompt": case.prompt,
        "note": case.note,
        "approve": case.approve,
        "session": session,
        "sent": sent,
        "timed_out": timed_out,
        "milliseconds": started.elapsed().as_millis(),
        "route": route,
        "statuses": statuses,
        "deltas": deltas,
        "assistant_messages": assistant_messages,
        "assistant_contents": assistant_messages.iter().filter_map(|message| message["content"].as_str()).collect::<Vec<_>>(),
        "tool_messages": tool_messages,
        "actions": actions,
        "decisions": decisions,
        "errors": errors,
        "unscripted_action": unscripted_action,
        "final": final_content,
        "session_assistant_count": assistant_count,
        "session_tool_count": tool_count,
        "message_statuses": statuses_after,
        "workspace": workspace.display().to_string(),
    })
}

fn missions_dir() -> PathBuf {
    if let Some(raw) = std::env::var_os("LFM3_MISSIONS_DIR") {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            return path;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../missions")
}

fn writeln_stderr(message: &str) -> std::io::Result<()> {
    use std::io::Write;
    writeln!(std::io::stderr(), "{message}")
}

fn fail(message: String) -> ! {
    let _ = writeln_stderr(&message);
    std::process::exit(1);
}
