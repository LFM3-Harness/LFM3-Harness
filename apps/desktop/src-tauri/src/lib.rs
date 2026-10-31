//! The embedded desktop shell.
//!
//! One [`engine::Engine`] lives in the Tauri managed state. Commands validate
//! through it and the forwarder relays durable, post-commit journal events to
//! the webview. There is no sidecar process, loopback engine listener, or shell
//! command surface in this crate.

pub mod chat;
pub mod chat_bench;
pub mod commands;
pub mod engine;
pub mod events;
pub mod job_windows;
pub mod jobs;
pub mod setup;
pub mod smoke;

use std::{path::PathBuf, sync::Arc};

use tauri::{Emitter, Manager};

pub fn run() {
    init_tracing();
    tauri::Builder::default()
        .setup(|app| {
            let missions = resolve_missions(app.handle());
            let engine = tauri::async_runtime::block_on(engine::Engine::boot(&missions))
                .map_err(|error| -> Box<dyn std::error::Error> { Box::new(error) })?;
            let store = engine.store().clone();
            let chat_handle = app.handle().clone();
            engine.chat().set_emitter(Arc::new(move |event| {
                if let Err(error) = chat_handle.emit("lfm3:chat-event", &event) {
                    tracing::warn!(%error, "could not deliver chat event");
                }
            }));
            let job_handle = app.handle().clone();
            engine.jobs().set_emitter(Arc::new(move |event| {
                if let Err(error) = job_handle.emit(jobs::JOB_EVENT_CHANNEL, &event) {
                    tracing::warn!(%error, "could not deliver job event");
                }
            }));
            let engine = Arc::new(engine);
            app.manage(commands::AppState::new(Arc::clone(&engine)));
            events::spawn(app.handle().clone(), store);
            tracing::info!(
                missions = %missions.display(),
                "local harness engine ready"
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::health,
            commands::list_missions,
            commands::list_runs,
            commands::start_run,
            commands::start_job,
            commands::get_job,
            commands::get_job_events,
            commands::list_jobs,
            commands::cancel_job,
            commands::get_acceptance,
            commands::get_run_snapshot,
            commands::get_run_events,
            commands::approve_plan,
            commands::decide_approval,
            commands::cancel_run,
            commands::retry_run,
            commands::list_models,
            commands::release_model,
            commands::choose_input_files,
            commands::open_artifact,
            commands::list_chat_sessions,
            commands::create_chat_session,
            commands::get_chat_session,
            commands::rename_chat_session,
            commands::delete_chat_session,
            commands::send_chat_message,
            commands::cancel_chat,
            commands::decide_chat_action,
            commands::transcribe_voice,
            commands::synthesize_speech,
            commands::attach_image,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run the LFM3 desktop shell");
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Finds mission definitions without assuming the process working directory:
/// an explicit override, then the packaged resource, then the source tree, then
/// an ancestor directory named `missions`.
fn resolve_missions(app: &tauri::AppHandle) -> PathBuf {
    if let Some(raw) = std::env::var_os("LFM3_MISSIONS_DIR") {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            return path;
        }
    }
    if let Ok(resource) = app.path().resource_dir() {
        let candidate = resource.join("missions");
        if candidate.is_dir() {
            return candidate;
        }
    }
    #[cfg(debug_assertions)]
    {
        let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../missions");
        if candidate.is_dir() {
            return candidate;
        }
    }
    let mut directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for _ in 0..8 {
        let candidate = directory.join("missions");
        if candidate.is_dir() {
            return candidate;
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        directory = parent.to_path_buf();
    }
    PathBuf::from("missions")
}
