//! Host-authored command templates with typed argument slots.
//!
//! A template fixes the executable and every literal argument. The planner
//! can only fill declared named slots, so it can never compose a command line
//! or choose a different program.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{check_cancelled, clip, ToolError, ToolRegistry, ToolStatus, MAX_OUTPUT_CHARS};
use crate::{AttemptId, CancellationChild};

const MAX_SLOT_CHARS: usize = 4_096;

/// One argument of a fixed template: a literal or a named slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandArg {
    Fixed { value: String },
    Slot { name: String },
}

/// A predefined command the harness may run. Never assembled by the model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandTemplate {
    pub id: String,
    pub executable: PathBuf,
    pub args: Vec<CommandArg>,
}

impl CommandTemplate {
    pub fn new(
        id: impl Into<String>,
        executable: impl Into<PathBuf>,
        args: Vec<CommandArg>,
    ) -> Result<Self, ToolError> {
        let template = Self {
            id: id.into(),
            executable: executable.into(),
            args,
        };
        template.validate()?;
        Ok(template)
    }

    fn validate(&self) -> Result<(), ToolError> {
        let fail = |message: &str| ToolError::InvalidTemplate {
            id: self.id.clone(),
            message: message.to_owned(),
        };
        if self.id.trim().is_empty() {
            return Err(fail("the template id must not be empty"));
        }
        if !self.executable.is_absolute() {
            return Err(fail("the executable must be an absolute, fixed path"));
        }
        let mut names = BTreeSet::new();
        for arg in &self.args {
            match arg {
                CommandArg::Fixed { value } => {
                    if value.is_empty() {
                        return Err(fail("literal arguments must not be empty"));
                    }
                }
                CommandArg::Slot { name } => {
                    if name.is_empty()
                        || !name
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '_')
                    {
                        return Err(fail(
                            "slot names must be non-empty ASCII words using letters, digits, or underscores",
                        ));
                    }
                    if !names.insert(name.clone()) {
                        return Err(fail("slot names must be unique"));
                    }
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn slot_names(&self) -> Vec<&str> {
        self.args
            .iter()
            .filter_map(|arg| match arg {
                CommandArg::Slot { name } => Some(name.as_str()),
                CommandArg::Fixed { .. } => None,
            })
            .collect()
    }

    /// Validates that the arguments contain exactly the declared slots.
    pub(crate) fn values(&self, arguments: &Value) -> Result<BTreeMap<String, String>, ToolError> {
        let invalid = |message: String| ToolError::InvalidCommandArguments {
            template: self.id.clone(),
            message,
        };
        let object = arguments.as_object().ok_or_else(|| {
            invalid("arguments must be an object containing the declared slots".to_owned())
        })?;
        let declared: BTreeSet<&str> = self.slot_names().into_iter().collect();
        for key in object.keys() {
            if !declared.contains(key.as_str()) {
                return Err(invalid(format!("unexpected slot {key:?}")));
            }
        }
        let mut values = BTreeMap::new();
        for name in declared {
            let value = object
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(format!("slot {name:?} must be a string")))?;
            if value.chars().count() > MAX_SLOT_CHARS {
                return Err(invalid(format!(
                    "slot {name:?} exceeds {MAX_SLOT_CHARS} characters"
                )));
            }
            if value.chars().any(char::is_control) {
                return Err(invalid(format!(
                    "slot {name:?} must not contain control characters"
                )));
            }
            values.insert(name.to_owned(), value.to_owned());
        }
        Ok(values)
    }

    pub(crate) fn argv(&self, values: &BTreeMap<String, String>) -> Vec<String> {
        self.args
            .iter()
            .map(|arg| match arg {
                CommandArg::Fixed { value } => value.clone(),
                CommandArg::Slot { name } => values.get(name).cloned().unwrap_or_default(),
            })
            .collect()
    }
}

/// Observable result of one template execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandRun {
    pub attempt: AttemptId,
    pub template: String,
    pub status: ToolStatus,
    pub exit_code: Option<i32>,
    pub output: String,
    pub duration: Duration,
}

impl ToolRegistry {
    /// Runs one declared template. The program and literals come only from the
    /// template; the arguments only fill its declared slots.
    pub async fn run_command(
        &self,
        template_id: &str,
        attempt: AttemptId,
        arguments: &Value,
        cancel: &CancellationChild,
    ) -> Result<CommandRun, ToolError> {
        check_cancelled(cancel)?;
        let template = self
            .command(template_id)
            .ok_or_else(|| ToolError::UnknownTemplate(template_id.to_owned()))?;
        let values = template.values(arguments)?;
        let argv = template.argv(&values);
        let mut command = std::process::Command::new(&template.executable);
        command
            .args(&argv)
            .current_dir(self.workspace())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let child = self
            .supervisor()
            .spawn(&mut command)
            .map_err(|error| ToolError::Process(error.to_string()))?;

        let stdout = child.take_stdout().map(drain);
        let stderr = child.take_stderr().map(drain);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    stop(&child, stdout, stderr);
                    return Err(ToolError::Process(error.to_string()));
                }
            }
            if let Err(cancelled) = cancel
                .run(tokio::time::sleep(Duration::from_millis(25)))
                .await
            {
                stop(&child, stdout, stderr);
                return Err(ToolError::Cancelled(cancelled.reason));
            }
        };

        let mut output = String::from_utf8_lossy(&join(stdout)).into_owned();
        let error_output = String::from_utf8_lossy(&join(stderr)).into_owned();
        if !error_output.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("[stderr]\n");
            output.push_str(&error_output);
        }
        let succeeded = status.success();
        let exit_code = status.code();
        let status = if succeeded {
            ToolStatus::Succeeded
        } else {
            ToolStatus::Failed
        };
        Ok(CommandRun {
            attempt,
            template: template.id.clone(),
            status,
            exit_code,
            output: clip(output, MAX_OUTPUT_CHARS),
            duration: started.elapsed(),
        })
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

fn join(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

fn stop(
    child: &crate::OwnedChild,
    stdout: Option<std::thread::JoinHandle<Vec<u8>>>,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = join(stdout);
    let _ = join(stderr);
}
