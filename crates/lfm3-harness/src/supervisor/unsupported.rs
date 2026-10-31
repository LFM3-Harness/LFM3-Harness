//! Placeholder for non-Windows hosts. The harness only ships owned-process
//! containment on Windows; other platforms fail closed.

use std::process::{Command, ExitStatus};

use crate::HarnessError;

#[derive(Clone, Debug)]
pub struct ProcessSupervisor;

impl ProcessSupervisor {
    pub fn new() -> Result<Self, HarnessError> {
        Err(unsupported())
    }

    pub fn spawn(&self, _command: &mut Command) -> Result<OwnedChild, HarnessError> {
        Err(unsupported())
    }

    pub fn terminate_owned(&self, _child: &OwnedChild) -> Result<(), HarnessError> {
        Err(unsupported())
    }

    pub fn shutdown(&self) -> Result<(), HarnessError> {
        Ok(())
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct OwnedChild;

impl OwnedChild {
    #[must_use]
    pub const fn pid(&self) -> u32 {
        0
    }

    pub fn kill(&self) -> Result<(), HarnessError> {
        Err(unsupported())
    }

    pub fn wait(&self) -> Result<ExitStatus, HarnessError> {
        Err(unsupported())
    }

    pub fn try_wait(&self) -> Result<Option<ExitStatus>, HarnessError> {
        Err(unsupported())
    }

    pub fn take_stdout(&self) -> Option<std::process::ChildStdout> {
        None
    }

    pub fn take_stderr(&self) -> Option<std::process::ChildStderr> {
        None
    }
}

#[must_use]
pub fn is_process_alive(_pid: u32) -> bool {
    false
}

fn unsupported() -> HarnessError {
    HarnessError::Process("owned process supervision requires Windows".to_owned())
}
