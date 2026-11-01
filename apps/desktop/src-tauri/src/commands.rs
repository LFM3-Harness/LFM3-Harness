//! Typed Tauri commands mirroring `EngineBridge`.
//!
//! Every command validates through [`Engine`]; none can select a different
//! step driver, widen a mission, or bypass a frozen acceptance hash.

use std::{collections::BTreeMap, sync::Arc};

use lfm3_harness::{ModelKey, RunId};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::{
    downloads::{DownloadEvent, DOWNLOAD_EVENT_CHANNEL},
    engine::{Engine, EngineError},
};

pub struct AppState {
    pub engine: Arc<Engine>,
}

impl AppState {
    #[must_use]
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }
}

#[tauri::command]
pub async fn health(state: State<'_, AppState>) -> Result<Value, String> {
    Ok(state.engine.health())
}

#[tauri::command]
pub async fn list_missions(state: State<'_, AppState>) -> Result<Value, String> {
    serde_json::to_value(state.engine.missions()).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn list_runs(state: State<'_, AppState>, limit: u32) -> Result<Value, String> {
    let runs = state.engine.list_runs(limit).await.map_err(to_message)?;
    serde_json::to_value(runs).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn start_run(
    state: State<'_, AppState>,
    mission: String,
    inputs: BTreeMap<String, Value>,
) -> Result<String, String> {
    let run = state
        .engine
        .start_run(&mission, inputs)
        .await
        .map_err(to_message)?;
    Ok(run.as_str().to_owned())
}

/// Starts one stated job as a model-driven turn loop. The response carries the
/// routing decision so the UI can show which local models the job consults.
#[tauri::command]
pub async fn start_job(
    state: State<'_, AppState>,
    goal: String,
    deliverable: Option<String>,
    minutes: Option<u64>,
    max_turns: Option<u32>,
) -> Result<Value, String> {
    let (job, route) = state
        .engine
        .start_job(&goal, deliverable, minutes, max_turns)
        .await
        .map_err(to_message)?;
    let job_id = job.id.clone();
    Ok(serde_json::json!({ "job_id": job_id, "job": job, "route": route }))
}

#[tauri::command]
pub async fn get_job(state: State<'_, AppState>, job_id: String) -> Result<Value, String> {
    let view = state.engine.jobs().view(&job_id).map_err(to_message)?;
    serde_json::to_value(view).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_job_events(
    state: State<'_, AppState>,
    job_id: String,
    after: i64,
) -> Result<Value, String> {
    let events = state
        .engine
        .jobs()
        .events_after(&job_id, after)
        .map_err(to_message)?;
    serde_json::to_value(events).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn list_jobs(state: State<'_, AppState>, limit: u32) -> Result<Value, String> {
    let jobs = state.engine.jobs().list(limit).map_err(to_message)?;
    serde_json::to_value(jobs).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cancel_job(state: State<'_, AppState>, job_id: String) -> Result<(), String> {
    state
        .engine
        .jobs()
        .cancel(&job_id)
        .await
        .map_err(to_message)
}

#[tauri::command]
pub async fn get_acceptance(state: State<'_, AppState>, run_id: String) -> Result<Value, String> {
    let run = parse_run(&run_id)?;
    let acceptance = state.engine.acceptance(&run).await.map_err(to_message)?;
    serde_json::to_value(acceptance).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_run_snapshot(state: State<'_, AppState>, run_id: String) -> Result<Value, String> {
    let run = parse_run(&run_id)?;
    let snapshot = state.engine.snapshot(&run).await.map_err(to_message)?;
    serde_json::to_value(snapshot).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_run_events(
    state: State<'_, AppState>,
    run_id: String,
    after: i64,
) -> Result<Value, String> {
    let run = parse_run(&run_id)?;
    let events = state
        .engine
        .events_after(&run, after)
        .await
        .map_err(to_message)?;
    serde_json::to_value(events).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn approve_plan(
    state: State<'_, AppState>,
    run_id: String,
    acceptance_hash: String,
) -> Result<(), String> {
    let run = parse_run(&run_id)?;
    state
        .engine
        .approve_plan(&run, &acceptance_hash)
        .await
        .map_err(to_message)?;
    Ok(())
}

#[tauri::command]
pub async fn decide_approval(
    state: State<'_, AppState>,
    approval_id: String,
    allow: bool,
) -> Result<(), String> {
    state
        .engine
        .decide_approval(&approval_id, allow)
        .await
        .map_err(to_message)
}

#[tauri::command]
pub async fn cancel_run(state: State<'_, AppState>, run_id: String) -> Result<(), String> {
    let run = parse_run(&run_id)?;
    state.engine.cancel_run(&run).await.map_err(to_message)
}

#[tauri::command]
pub async fn retry_run(state: State<'_, AppState>, run_id: String) -> Result<String, String> {
    let run = parse_run(&run_id)?;
    let child = state.engine.retry_run(&run).await.map_err(to_message)?;
    Ok(child.as_str().to_owned())
}

#[tauri::command]
pub async fn list_models(state: State<'_, AppState>) -> Result<Value, String> {
    Ok(Value::Array(state.engine.models()))
}

#[tauri::command]
pub async fn release_model(state: State<'_, AppState>, model_key: String) -> Result<(), String> {
    let key = parse_model(&model_key)?;
    state.engine.release_model(key).await.map_err(to_message)?;
    Ok(())
}

// ------------------------------------------------------------------ setup

/// Opens the folder picker and switches chat and jobs to the chosen folder.
/// Returns `null` when the person cancels; nothing changes in that case.
#[tauri::command]
pub async fn choose_workspace(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let Some(folder) = rfd::AsyncFileDialog::new()
        .set_title("Choose the folder chat and jobs work in")
        .pick_folder()
        .await
    else {
        return Ok(None);
    };
    let path = state
        .engine
        .set_workspace(folder.path().to_path_buf())
        .await
        .map_err(to_message)?;
    Ok(Some(path.display().to_string()))
}

/// What can be downloaded, where it lands, and what is installed already.
#[tauri::command]
pub async fn model_catalog(state: State<'_, AppState>) -> Result<Value, String> {
    state.engine.apply_pending_reload().await;
    Ok(state.engine.model_catalog())
}

/// Starts one explicit download. Progress arrives on the download channel;
/// nothing is fetched unless this command is invoked by a person.
#[tauri::command]
pub async fn download_model(
    app: AppHandle,
    state: State<'_, AppState>,
    model_key: String,
) -> Result<(), String> {
    let key = parse_model(&model_key)?;
    let emitter = Arc::new(move |event: DownloadEvent| {
        if let Err(error) = app.emit(DOWNLOAD_EVENT_CHANNEL, &event) {
            tracing::warn!(%error, "could not deliver download event");
        }
    });
    state
        .engine
        .start_download(key, emitter)
        .map_err(to_message)
}

#[tauri::command]
pub async fn cancel_download(state: State<'_, AppState>, model_key: String) -> Result<(), String> {
    let key = parse_model(&model_key)?;
    state.engine.cancel_download(key).map_err(to_message)
}

/// Swaps downloaded weights into the running engine after a turn ends.
#[tauri::command]
pub async fn reload_models(state: State<'_, AppState>) -> Result<Value, String> {
    let models = state.engine.reload_models().await.map_err(to_message)?;
    Ok(Value::Array(models))
}

#[tauri::command]
pub async fn choose_input_files(
    state: State<'_, AppState>,
    mission: String,
    input_key: String,
) -> Result<Vec<String>, String> {
    state
        .engine
        .validate_choosable(&mission, &input_key)
        .map_err(to_message)?;
    let files = rfd::AsyncFileDialog::new().pick_files().await;
    Ok(files
        .unwrap_or_default()
        .into_iter()
        .map(|file| file.path().display().to_string())
        .collect())
}

#[tauri::command]
pub async fn open_artifact(state: State<'_, AppState>, artifact_id: String) -> Result<(), String> {
    state
        .engine
        .open_artifact(&artifact_id)
        .await
        .map_err(to_message)
}

// ------------------------------------------------------------------- chat

#[tauri::command]
pub async fn list_chat_sessions(state: State<'_, AppState>, limit: u32) -> Result<Value, String> {
    let sessions = state
        .engine
        .chat()
        .list_sessions(limit)
        .map_err(to_message)?;
    serde_json::to_value(sessions).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn create_chat_session(
    state: State<'_, AppState>,
    title: Option<String>,
) -> Result<Value, String> {
    let session = state
        .engine
        .chat()
        .create_session(title.as_deref())
        .map_err(to_message)?;
    serde_json::to_value(session).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_chat_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Value, String> {
    let view = state
        .engine
        .chat()
        .session_view(&session_id)
        .map_err(to_message)?;
    serde_json::to_value(view).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn rename_chat_session(
    state: State<'_, AppState>,
    session_id: String,
    title: String,
) -> Result<(), String> {
    state
        .engine
        .chat()
        .rename_session(&session_id, &title)
        .map_err(to_message)
}

#[tauri::command]
pub async fn delete_chat_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    state
        .engine
        .chat()
        .delete_session(&session_id)
        .map_err(to_message)
}

#[tauri::command]
pub async fn send_chat_message(
    state: State<'_, AppState>,
    session_id: String,
    content: String,
) -> Result<Value, String> {
    let message = state
        .engine
        .chat()
        .send(&session_id, &content)
        .await
        .map_err(to_message)?;
    serde_json::to_value(message).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cancel_chat(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    state
        .engine
        .chat()
        .cancel(&session_id)
        .await
        .map_err(to_message)
}

#[tauri::command]
pub async fn decide_chat_action(
    state: State<'_, AppState>,
    action_id: String,
    allow: bool,
) -> Result<(), String> {
    state
        .engine
        .chat()
        .decide_action(&action_id, allow)
        .await
        .map_err(to_message)
}

#[tauri::command]
pub async fn transcribe_voice(
    state: State<'_, AppState>,
    audio_base64: String,
    format: String,
) -> Result<String, String> {
    state
        .engine
        .chat()
        .transcribe_voice(&audio_base64, &format)
        .await
        .map_err(to_message)
}

/// Reads one assistant answer aloud with the local voice. No audio or text
/// leaves the machine.
#[tauri::command]
pub async fn synthesize_speech(state: State<'_, AppState>, text: String) -> Result<Value, String> {
    let reply = state
        .engine
        .chat()
        .synthesize_speech(&text)
        .await
        .map_err(to_message)?;
    serde_json::to_value(reply).map_err(|error| error.to_string())
}

/// Stores a user-attached image inside the workspace so the vision tool can
/// inspect it, and returns the workspace-relative path.
#[tauri::command]
pub fn attach_image(
    state: State<'_, AppState>,
    name: String,
    data_base64: String,
) -> Result<String, String> {
    state
        .engine
        .chat()
        .attach_image(&name, &data_base64)
        .map_err(to_message)
}

fn parse_run(raw: &str) -> Result<RunId, String> {
    raw.parse::<RunId>().map_err(|error| error.to_string())
}

fn parse_model(raw: &str) -> Result<ModelKey, String> {
    match raw {
        "text" => Ok(ModelKey::Text),
        "vision" => Ok(ModelKey::Vision),
        "audio" => Ok(ModelKey::Audio),
        "d1" => Ok(ModelKey::D1),
        other => Err(format!("unknown model key {other:?}")),
    }
}

fn to_message(error: EngineError) -> String {
    error.to_string()
}
