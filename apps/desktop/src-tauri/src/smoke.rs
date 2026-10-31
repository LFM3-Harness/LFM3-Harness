//! Installed-application acceptance probes.
//!
//! The probes boot the same embedded engine the window uses, so an installed
//! build can be exercised without a webview or UI automation:
//!
//! - `--smoke`: starts the real `desk_task`, proves a second concurrent start
//!   is refused, cancels that run, starts and cancels another run, then
//!   releases the model. It never approves a plan, so no step can execute.
//! - `--smoke-close`: starts a real run, waits until the harness-owned model
//!   server is serving, then drops the whole engine as an application close
//!   would. The next boot in the same process must report the run honestly as
//!   `interrupted`.
//! - `--smoke-hold`: starts a real run and stays alive until the script
//!   force-terminates the process, proving the application job takes the owned
//!   server with it.
//!
//! Every probe writes a JSON report to `LFM3_SMOKE_REPORT` when that variable
//! is set and exits non-zero on failure.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use lfm3_harness::{ModelKey, RunStatus};
use serde_json::{json, Value};

use crate::engine::{Engine, EngineError};

const GOAL: &str = "Write out/summary.md summarizing the workspace check in one paragraph.";
const HOLD_SECONDS: u64 = 600;

#[derive(Clone, Copy)]
enum Mode {
    Check,
    Close,
    Hold,
}

pub fn run() -> ! {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mode = if arguments.iter().any(|argument| argument == "--smoke-hold") {
        Mode::Hold
    } else if arguments.iter().any(|argument| argument == "--smoke-close") {
        Mode::Close
    } else if arguments.iter().any(|argument| argument == "--smoke") {
        Mode::Check
    } else {
        // A GUI-subsystem release build may have no usable console handle, so
        // never rely on `eprintln!` (which panics on write failure).
        let _ = writeln!(
            std::io::stderr(),
            "usage: lfm3-desktop --smoke | --smoke-close | --smoke-hold"
        );
        std::process::exit(2);
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => fail(mode, format!("could not start the smoke runtime: {error}")),
    };
    let outcome = runtime.block_on(async {
        match mode {
            Mode::Check => check_run().await,
            Mode::Close => close_run().await,
            Mode::Hold => hold_run().await,
        }
    });
    match outcome {
        Ok(report) => {
            emit(&report);
            std::process::exit(0);
        }
        Err(error) => fail(mode, error),
    }
}

async fn check_run() -> Result<Value, String> {
    let engine = boot().await?;
    let pinned = engine.health()["llama"]["pinned"]
        .as_bool()
        .unwrap_or(false);

    let first = start(&engine).await?;
    let first_snapshot = engine
        .snapshot(&first)
        .await
        .map_err(|error| format!("snapshot first run: {error}"))?;
    if first_snapshot.run.status != RunStatus::AwaitingPlanApproval {
        return Err(format!(
            "run stopped at {} instead of awaiting plan approval",
            first_snapshot.run.status.as_str()
        ));
    }
    let acceptance_hash = first_snapshot.run.acceptance_hash.clone();
    if acceptance_hash.is_empty() {
        return Err("the frozen acceptance hash is empty".to_owned());
    }
    let model_ready = text_ready(&engine);

    let overlapping = engine.start_run("desk_task", mission_inputs()).await;
    let overlapping_refused = matches!(overlapping, Err(EngineError::Conflict(_)));

    let first_cancelled = cancel(&engine, &first).await?;

    // A second real run starts only after the first is terminal, and it is
    // cancelled too. This is the "cancel a second run" lifecycle check.
    let second = start(&engine).await?;
    let second_cancelled = cancel(&engine, &second).await?;
    let released = engine
        .release_model(ModelKey::Text)
        .await
        .map_err(|error| format!("release text model: {error}"))?;
    let text_absent = !text_ready(&engine);

    let ok = pinned
        && model_ready
        && overlapping_refused
        && first_cancelled
        && second_cancelled
        && released
        && text_absent;
    Ok(json!({
        "ok": ok,
        "mode": "check",
        "run_id": first.as_str(),
        "second_run_id": second.as_str(),
        "acceptance_hash": acceptance_hash,
        "runtime_pinned": pinned,
        "model_ready": model_ready,
        "overlapping_start_refused": overlapping_refused,
        "first_cancelled": first_cancelled,
        "second_cancelled": second_cancelled,
        "model_released": released,
        "text_model_absent_after_release": text_absent,
    }))
}

async fn close_run() -> Result<Value, String> {
    let run;
    {
        let engine = boot().await?;
        run = start(&engine).await?;
        let snapshot = engine
            .snapshot(&run)
            .await
            .map_err(|error| format!("snapshot: {error}"))?;
        if snapshot.run.status != RunStatus::AwaitingPlanApproval {
            return Err(format!(
                "run stopped at {} instead of awaiting plan approval",
                snapshot.run.status.as_str()
            ));
        }
        if !text_ready(&engine) {
            return Err("the harness-owned text model never became ready".to_owned());
        }
        // Dropping the engine drops the model lease, the manager, and the
        // application job. This is what closing the window does.
    }
    tokio::time::sleep(Duration::from_millis(750)).await;

    let reopened = boot().await?;
    let status = reopened
        .snapshot(&run)
        .await
        .map_err(|error| format!("snapshot after reopen: {error}"))?
        .run
        .status;
    let interrupted = status == RunStatus::Interrupted;
    let active = reopened
        .store()
        .active_run()
        .await
        .map_err(|error| format!("active run after reopen: {error}"))?;
    drop(reopened);

    Ok(json!({
        "ok": interrupted && active.is_none(),
        "mode": "close",
        "run_id": run.as_str(),
        "status_after_reopen": status.as_str(),
        "interrupted": interrupted,
        "active_run_after_reopen": active.map(|id| id.as_str().to_owned()),
    }))
}

async fn hold_run() -> Result<Value, String> {
    let engine = boot().await?;
    let run = start(&engine).await?;
    let snapshot = engine
        .snapshot(&run)
        .await
        .map_err(|error| format!("snapshot: {error}"))?;
    if snapshot.run.status != RunStatus::AwaitingPlanApproval {
        return Err(format!(
            "run stopped at {} instead of awaiting plan approval",
            snapshot.run.status.as_str()
        ));
    }
    if !text_ready(&engine) {
        return Err("the harness-owned text model never became ready".to_owned());
    }
    emit(&json!({
        "ok": true,
        "mode": "hold",
        "run_id": run.as_str(),
        "pid": std::process::id(),
        "status": snapshot.run.status.as_str(),
        "model_ready": true,
    }));
    // Stay alive with the run and its owned server. The script terminates this
    // process, which closes the job handle and must kill the owned child.
    tokio::time::sleep(Duration::from_secs(HOLD_SECONDS)).await;
    std::process::abort();
}

async fn start(engine: &Engine) -> Result<lfm3_harness::RunId, String> {
    engine
        .start_run("desk_task", mission_inputs())
        .await
        .map_err(|error| format!("start desk_task: {error}"))
}

async fn cancel(engine: &Engine, run: &lfm3_harness::RunId) -> Result<bool, String> {
    engine
        .cancel_run(run)
        .await
        .map_err(|error| format!("cancel {run}: {error}"))?;
    Ok(engine
        .snapshot(run)
        .await
        .map_err(|error| format!("snapshot after cancel: {error}"))?
        .run
        .status
        == RunStatus::Cancelled)
}

fn text_ready(engine: &Engine) -> bool {
    engine
        .models()
        .iter()
        .any(|model| model["key"] == "text" && model["state"] == "ready")
}

fn mission_inputs() -> BTreeMap<String, Value> {
    BTreeMap::from([("goal".to_owned(), Value::String(GOAL.to_owned()))])
}

async fn boot() -> Result<Engine, String> {
    let missions = missions_dir()?;
    Engine::boot(&missions)
        .await
        .map_err(|error| format!("boot engine: {error}"))
}

fn missions_dir() -> Result<PathBuf, String> {
    if let Some(raw) = std::env::var_os("LFM3_MISSIONS_DIR") {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            return Ok(path);
        }
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let directory = executable
        .parent()
        .ok_or_else(|| "installed executable has no directory".to_owned())?;
    for candidate in [
        directory.join("missions"),
        directory.join("resources").join("missions"),
        directory.join("_up_").join("missions"),
    ] {
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "no missions directory next to {}",
        directory.display()
    ))
}

fn emit(report: &Value) {
    let _ = writeln!(std::io::stdout(), "{report}");
    if let Some(raw) = std::env::var_os("LFM3_SMOKE_REPORT") {
        let _ = write_report(Path::new(&raw), report);
    }
}

fn write_report(path: &Path, report: &Value) -> std::io::Result<()> {
    let raw = serde_json::to_string_pretty(report).map_err(std::io::Error::other)?;
    fs::write(path, raw)
}

fn fail(mode: Mode, message: String) -> ! {
    let mode = match mode {
        Mode::Check => "check",
        Mode::Close => "close",
        Mode::Hold => "hold",
    };
    let report = json!({ "ok": false, "mode": mode, "error": message });
    let _ = writeln!(std::io::stderr(), "{report}");
    if let Some(raw) = std::env::var_os("LFM3_SMOKE_REPORT") {
        let _ = write_report(Path::new(&raw), &report);
    }
    std::process::exit(1);
}
