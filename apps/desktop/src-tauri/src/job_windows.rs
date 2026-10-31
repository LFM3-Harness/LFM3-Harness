//! Application-lifetime process containment.
//!
//! One kill-on-close Job Object is created for the desktop process. Model
//! servers, command tools, and any other harness-owned child are spawned
//! through this supervisor, so closing the application (or losing it abruptly)
//! closes the job and terminates exactly those processes. External servers the
//! user started separately are never members of this job and are never touched.

#[cfg(all(windows, debug_assertions))]
use std::process::{Command, Stdio};

use lfm3_harness::ProcessSupervisor;

use crate::engine::EngineError;

/// Owns the single job that contains every process this application starts.
pub struct ApplicationJob {
    supervisor: ProcessSupervisor,
}

impl ApplicationJob {
    pub fn new() -> Result<Self, EngineError> {
        Ok(Self {
            supervisor: ProcessSupervisor::new().map_err(EngineError::Harness)?,
        })
    }

    #[must_use]
    pub fn supervisor(&self) -> ProcessSupervisor {
        self.supervisor.clone()
    }

    /// Terminates every owned child. Idempotent; the job also enforces
    /// kill-on-close when the last handle is dropped.
    pub fn shutdown(&self) {
        let _ = self.supervisor.shutdown();
    }
}

/// Abrupt-termination probe used by the packaged lifecycle tests.
///
/// The helper creates the application job, spawns a long-running child through
/// it, records the child PID, then terminates itself without unwinding. The
/// OS closes the job handle and must take the child with it. Only available in
/// debug builds and only when the environment variable is set.
#[cfg(all(windows, debug_assertions))]
pub fn maybe_run_forced_exit_probe() {
    let Some(pid_file) = std::env::var_os("LFM3_FORCED_EXIT_PROBE") else {
        return;
    };
    let job = ApplicationJob::new().expect("create application job");
    let mut command = Command::new("cmd");
    command
        .args(["/C", "ping", "-n", "120", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = job
        .supervisor()
        .spawn(&mut command)
        .expect("spawn owned probe child");
    std::fs::write(&pid_file, child.pid().to_string()).expect("write probe pid");
    // Simulate forced termination: no destructors, no explicit shutdown.
    std::process::abort();
}

#[cfg(not(all(windows, debug_assertions)))]
pub fn maybe_run_forced_exit_probe() {}
