//! Launching harness-owned model servers and waiting for readiness.

use std::{process::Stdio, time::Instant};

use super::{client::LlamaClient, types::LaunchSpec, ModelError};
use crate::{CancellationChild, HarnessError, OwnedChild, ProcessSupervisor};

/// Builds the fixed command line; no planner-provided text is accepted.
pub(crate) fn spawn(
    supervisor: &ProcessSupervisor,
    launch: &LaunchSpec,
) -> Result<OwnedChild, HarnessError> {
    if launch.executable.as_os_str().is_empty() {
        return Err(HarnessError::Process(
            "model launch executable is empty".to_owned(),
        ));
    }
    let mut command = std::process::Command::new(&launch.executable);
    command
        .args(&launch.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    supervisor.spawn(&mut command)
}

/// Polls the endpoint until the expected model identity answers or the
/// startup budget is exhausted. Cancelling aborts the wait promptly.
pub(crate) async fn wait_until_ready(
    client: &LlamaClient,
    cancel: &CancellationChild,
    startup_timeout: std::time::Duration,
    health_interval: std::time::Duration,
) -> Result<(), ModelError> {
    let deadline = Instant::now() + startup_timeout;
    loop {
        match client.verify_identity(cancel).await {
            Ok(()) => return Ok(()),
            Err(ModelError::Cancelled(reason)) => return Err(ModelError::Cancelled(reason)),
            Err(error @ ModelError::IdentityMismatch { .. }) => return Err(error),
            Err(error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(ModelError::Startup(error.to_string()));
                }
                let pause = health_interval.min(remaining);
                if let Err(cancelled) = cancel.run(tokio::time::sleep(pause)).await {
                    return Err(ModelError::Cancelled(cancelled.reason));
                }
            }
        }
    }
}
