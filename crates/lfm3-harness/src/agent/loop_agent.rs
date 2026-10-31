//! Codebuff-style agent loop for a stated job.
//!
//! Instead of freezing a step list up front, the loop lets the acting model
//! decide the next action from what it has actually observed. Each turn:
//!
//! 1. the model sees the goal, the deliverable, the live todo list, the real
//!    workspace files, and the observations of recent turns;
//! 2. it calls one to three tools (`todo_write` to plan, `finish` when done);
//! 3. the harness executes them, feeds the real outputs back, and repeats.
//!
//! Verification is unchanged in spirit: nothing is "done" because the model
//! said so. `finish` is only accepted when the required deliverable exists and
//! is non-empty, and the report carries the measured bytes and digest.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    model::{ChatMessage, ChatRequest, ModelError, ModelGateway, ToolDefinition},
    tools::{ToolName, ToolRegistry, ToolRequest},
    AttemptId, CancellationContext,
};

/// Tools the loop itself answers, without touching the workspace.
pub const TODO_TOOL: &str = "todo_write";
/// Signals that the model believes the job is complete.
pub const FINISH_TOOL: &str = "finish";

/// One item of the model's own plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
}

/// One tool call made during one turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopCall {
    pub tool: String,
    pub arguments: Value,
    pub ok: bool,
    pub output: String,
}

/// Everything that happened in one model turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopTurn {
    pub index: u32,
    pub note: String,
    pub calls: Vec<LoopCall>,
}

/// Why the loop stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopStatus {
    Passed,
    /// The model asked to finish, but the deliverable did not verify.
    Unverified,
    /// Turn cap reached.
    TurnBudget,
    /// The whole-job deadline passed.
    Deadline,
    /// Too many turns in a row without progress.
    Stalled,
}

impl LoopStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Unverified => "unverified",
            Self::TurnBudget => "turn_budget",
            Self::Deadline => "deadline",
            Self::Stalled => "stalled",
        }
    }
}

/// Proof that the required artifact exists, measured rather than claimed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeliverableProof {
    pub path: String,
    pub bytes: u64,
    pub digest: String,
}

/// The whole outcome of one looped job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopReport {
    pub status: LoopStatus,
    pub model: String,
    pub goal: String,
    pub deliverable: Option<DeliverableProof>,
    pub todos: Vec<TodoItem>,
    pub turns: Vec<LoopTurn>,
    pub stopped: String,
}

impl LoopReport {
    /// A compact JSON view for the CLI and the UI timeline.
    #[must_use]
    pub fn report(&self) -> Value {
        json!({
            "status": self.status.as_str(),
            "model": self.model,
            "goal": self.goal,
            "deliverable": self.deliverable,
            "todos": self.todos,
            "turns": self.turns,
            "stopped": self.stopped,
        })
    }
}

/// Tuning for one looped job.
#[derive(Clone, Copy, Debug)]
pub struct LoopOptions {
    /// Hard cap on model turns; the deadline can stop the job earlier.
    pub max_turns: u32,
    /// Tool calls accepted in one turn.
    pub max_calls: u32,
    /// How many recent observations are replayed into the prompt.
    pub replay: usize,
}

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            max_turns: 40,
            max_calls: 3,
            replay: 8,
        }
    }
}

/// What went wrong before the loop could even start a turn.
#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("the job has no tools to act with")]
    NoTools,
    #[error("model call failed: {0}")]
    Model(String),
    #[error("the loop was cancelled: {0}")]
    Cancelled(String),
}

/// Runs one job as a model-driven loop until the deliverable verifies.
pub struct AgentLoop {
    gateway: Arc<dyn ModelGateway>,
    tools: Arc<ToolRegistry>,
    model: String,
    options: LoopOptions,
    max_tokens: u32,
}

impl AgentLoop {
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ModelGateway>,
        tools: Arc<ToolRegistry>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            gateway,
            tools,
            model: model.into(),
            options: LoopOptions::default(),
            max_tokens: 2048,
        }
    }

    #[must_use]
    pub fn with_options(mut self, options: LoopOptions) -> Self {
        self.options = options;
        self
    }

    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Runs the job. `on_event` receives every turn and call for the journal.
    pub async fn run(
        &self,
        goal: &str,
        deliverable: Option<&str>,
        workspace: &Path,
        cancel: &CancellationContext,
        on_event: &mut (dyn FnMut(Value) + Send),
    ) -> Result<LoopReport, LoopError> {
        let definitions = self.tool_definitions(false);
        if !definitions.iter().any(|definition| {
            definition.function.name != TODO_TOOL && definition.function.name != FINISH_TOOL
        }) {
            return Err(LoopError::NoTools);
        }
        let mut todos: Vec<TodoItem> = Vec::new();
        let mut turns: Vec<LoopTurn> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stall = 0u32;
        let mut idle = 0u32;
        let mut read_paths: BTreeSet<String> = BTreeSet::new();
        // A small model can read forever. Once every source has been seen, the
        // only useful next action is producing the artifact.
        let mut must_write = false;
        let mut wrote_once = false;
        let mut stopped = String::from("turn budget");
        let mut status = LoopStatus::TurnBudget;

        for index in 1..=self.options.max_turns {
            if cancel.is_cancelled() {
                status = LoopStatus::Deadline;
                stopped = "job deadline reached".to_owned();
                break;
            }
            if stall >= 3 && !must_write {
                status = LoopStatus::Stalled;
                stopped = "three turns without a productive call".to_owned();
                break;
            }
            let offered = self.tool_definitions(must_write);
            let brief = self.brief(
                goal,
                deliverable,
                workspace,
                &todos,
                &turns,
                must_write,
                &read_paths,
            );
            let chat = ChatRequest::new(vec![
                ChatMessage::system(self.system_brief()),
                ChatMessage::user(brief),
            ])
            .with_tools(offered)
            .with_tool_choice("required")
            .with_temperature(0.2)
            .with_max_tokens(self.max_tokens);
            let response = match cancel.run(self.gateway.act(chat, cancel.child())).await {
                Ok(Ok(response)) => response,
                Ok(Err(ModelError::Cancelled(_))) => {
                    status = LoopStatus::Deadline;
                    stopped = "job deadline reached".to_owned();
                    break;
                }
                Ok(Err(error)) => return Err(LoopError::Model(error.to_string())),
                Err(cancelled) => {
                    return Err(LoopError::Cancelled(cancelled.reason.to_string()));
                }
            };
            let calls = response.tool_calls();
            let mut turn = LoopTurn {
                index,
                note: String::new(),
                calls: Vec::new(),
            };
            if calls.is_empty() {
                stall += 1;
                turn.note = "no tool call was made".to_owned();
                turns.push(turn);
                emit(on_event, &turns, &todos);
                continue;
            }
            if calls.len() > self.options.max_calls as usize {
                stall += 1;
                turn.note = format!(
                    "{} tool calls in one turn; at most {} are allowed",
                    calls.len(),
                    self.options.max_calls
                );
                turns.push(turn);
                emit(on_event, &turns, &todos);
                continue;
            }

            let mut productive = false;
            let mut progressed = false;
            let mut wrote = false;
            let mut finished = false;
            for call in calls {
                let name = call.function.name.clone();
                let arguments = decode_arguments(&call.function.arguments);
                if name == TODO_TOOL {
                    match apply_todos(&mut todos, &arguments) {
                        Ok(()) => turn.calls.push(LoopCall {
                            tool: name,
                            arguments,
                            ok: true,
                            output: render_todos(&todos),
                        }),
                        Err(error) => turn.calls.push(LoopCall {
                            tool: name,
                            arguments,
                            ok: false,
                            output: error,
                        }),
                    }
                    continue;
                }
                if name == FINISH_TOOL {
                    finished = true;
                    turn.calls.push(LoopCall {
                        tool: name,
                        arguments,
                        ok: true,
                        output: "finish requested".to_owned(),
                    });
                    continue;
                }
                let signature = format!(
                    "{name}:{}",
                    serde_json::to_string(&arguments).unwrap_or_default()
                );
                if !seen.insert(signature) {
                    stall += 1;
                    let unread = unread_files(workspace, &read_paths, deliverable);
                    let output = if let Some(next) = unread.first() {
                        format!("you already read that one. Read the file you have not opened yet: {next}")
                    } else if deliverable.is_some() {
                        must_write = true;
                        "you have read every source. Write the required output now with write_file."
                            .to_owned()
                    } else {
                        "you already made this exact call; take a different action.".to_owned()
                    };
                    turn.calls.push(LoopCall {
                        tool: name,
                        arguments,
                        ok: false,
                        output,
                    });
                    continue;
                }
                let Some(tool) = ToolName::parse(&name) else {
                    turn.calls.push(LoopCall {
                        tool: name,
                        arguments,
                        ok: false,
                        output: "this tool does not exist".to_owned(),
                    });
                    stall += 1;
                    continue;
                };
                if let Some(problem) = guard_write(tool, &arguments, &read_paths, deliverable) {
                    stall += 1;
                    turn.calls.push(LoopCall {
                        tool: name,
                        arguments,
                        ok: false,
                        output: problem,
                    });
                    continue;
                }
                let request =
                    ToolRequest::new(AttemptId::new(), call.id.clone(), tool, arguments.clone());
                match self.tools.execute(request, &cancel.child()).await {
                    Ok(result) => {
                        let ok = result.succeeded();
                        let output = clip(&result.output, 1_500);
                        if ok {
                            productive = true;
                            if tool == ToolName::ReadFile {
                                if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                                    let clean = path.trim_start_matches("./").replace('\\', "/");
                                    read_paths.insert(clean);
                                    progressed = true;
                                }
                            }
                            if is_write(tool) {
                                let written = arguments
                                    .get("path")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                if non_empty(workspace, written) {
                                    wrote = true;
                                    progressed = true;
                                }
                            }
                            if deliverable.is_some()
                                && !read_paths.is_empty()
                                && unread_files(workspace, &read_paths, deliverable).is_empty()
                            {
                                must_write = true;
                            }
                        }
                        turn.calls.push(LoopCall {
                            tool: name,
                            arguments,
                            ok,
                            output,
                        });
                    }
                    Err(error) => {
                        stall += 1;
                        turn.calls.push(LoopCall {
                            tool: name,
                            arguments,
                            ok: false,
                            output: error.to_string(),
                        });
                    }
                }
            }
            if productive {
                stall = 0;
            } else if !finished {
                stall += 1;
            }
            if wrote {
                wrote_once = true;
            }
            if progressed {
                idle = 0;
            } else {
                idle += 1;
            }
            turns.push(turn);
            emit(on_event, &turns, &todos);

            let proof = deliverable.and_then(|path| verify_deliverable(workspace, path));
            // Once this run has written the required output and a later turn did
            // not produce another write, the artifact is the proof. The model
            // does not need to narrate completion for the job to be done.
            if wrote_once && !wrote {
                if let Some(proof) = &proof {
                    return Ok(LoopReport {
                        status: LoopStatus::Passed,
                        model: self.model.clone(),
                        goal: goal.to_owned(),
                        deliverable: Some(proof.clone()),
                        todos,
                        turns,
                        stopped: "the required output verified".to_owned(),
                    });
                }
            }
            // Reading forever is not progress on a job that must produce an
            // artifact. Six turns without a write is a stalled job, not a pass.
            if deliverable.is_some() && proof.is_none() && idle >= 6 {
                status = LoopStatus::Stalled;
                stopped = "six turns without writing the required output".to_owned();
                break;
            }
            if finished {
                match (&proof, deliverable) {
                    (Some(proof), _) => {
                        status = LoopStatus::Passed;
                        stopped = "the model finished and the deliverable verified".to_owned();
                        return Ok(LoopReport {
                            status,
                            model: self.model.clone(),
                            goal: goal.to_owned(),
                            deliverable: Some(proof.clone()),
                            todos,
                            turns,
                            stopped,
                        });
                    }
                    (None, Some(path)) => {
                        stall += 1;
                        if let Some(last) = turns.last_mut() {
                            last.note = format!(
                                "finish refused: {path} does not exist or is empty; keep working"
                            );
                        }
                    }
                    (None, None) => {
                        status = LoopStatus::Passed;
                        stopped = "the model finished; the job had no required output".to_owned();
                        return Ok(LoopReport {
                            status,
                            model: self.model.clone(),
                            goal: goal.to_owned(),
                            deliverable: None,
                            todos,
                            turns,
                            stopped,
                        });
                    }
                }
            }
            if deliverable.is_none() || status == LoopStatus::Passed {
                continue;
            }
        }

        let proof = deliverable.and_then(|path| verify_deliverable(workspace, path));
        if !matches!(status, LoopStatus::Deadline | LoopStatus::Stalled) {
            if proof.is_some() {
                status = LoopStatus::Passed;
                stopped = "the deliverable verified at the turn cap".to_owned();
            } else {
                status = LoopStatus::Unverified;
                stopped = "the turn cap was reached before the deliverable verified".to_owned();
            }
        }
        Ok(LoopReport {
            status,
            model: self.model.clone(),
            goal: goal.to_owned(),
            deliverable: proof,
            todos,
            turns,
            stopped,
        })
    }

    fn system_brief(&self) -> String {
        "You are the acting model of a local desktop agent working in a loop. \
         Each turn you make one to three tool calls. Call todo_write to keep a short plan \
         and mark items done. Read a file before you summarise it. Write real content, never \
         a placeholder. Keep going until the required output exists and is complete, then \
         call finish. Do not call finish before the output file is written. \
         Use workspace-relative paths only."
            .to_owned()
    }

    #[allow(clippy::too_many_arguments)]
    fn brief(
        &self,
        goal: &str,
        deliverable: Option<&str>,
        workspace: &Path,
        todos: &[TodoItem],
        turns: &[LoopTurn],
        must_write: bool,
        read_paths: &BTreeSet<String>,
    ) -> String {
        let missing = deliverable.is_some()
            && deliverable
                .and_then(|path| verify_deliverable(workspace, path))
                .is_none();
        let deliverable_ref = deliverable;
        let deliverable = match deliverable {
            Some(path) if missing => format!(
                "{path}
(this file does not exist yet: write it before you call finish)"
            ),
            Some(path) => path.to_owned(),
            None => "(no required output; finish when the goal is met)".to_owned(),
        };
        let todos = if todos.is_empty() {
            "(no todo list yet; call todo_write to plan)".to_owned()
        } else {
            render_todos(todos)
        };
        let files = list_workspace(workspace);
        let observations = if turns.is_empty() {
            "(nothing observed yet)".to_owned()
        } else {
            turns
                .iter()
                .rev()
                .take(self.options.replay)
                .rev()
                .map(|turn| {
                    let calls = turn
                        .calls
                        .iter()
                        .map(|call| {
                            format!(
                                "[{}] {} {}{}",
                                if call.ok { "ok" } else { "failed" },
                                call.tool,
                                compact(&call.arguments),
                                if call.output.is_empty() {
                                    String::new()
                                } else {
                                    format!("\n    -> {}", call.output)
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n  ");
                    if turn.calls.is_empty() {
                        format!("Turn {}: {}", turn.index, turn.note)
                    } else {
                        format!("Turn {}:\n  {}", turn.index, calls)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let unread = unread_files(workspace, read_paths, deliverable_ref);
        let pending = if unread.is_empty() {
            String::new()
        } else {
            format!("\nFiles not read yet: {}", unread.join(", "))
        };
        let forced = if must_write {
            format!(
                "\nread_file is unavailable now: write {deliverable} with write_file, using what you observed, then call finish."
            )
        } else {
            String::new()
        };
        let observations = format!("{observations}{pending}{forced}");
        format!(
            "Job: {goal}\nRequired output: {deliverable}\n\n\
             Workspace files:\n{files}\n\nTodo list:\n{todos}\n\n\
             What you have observed so far:\n{observations}\n\n\
             Next action:"
        )
    }

    fn tool_definitions(&self, must_write: bool) -> Vec<ToolDefinition> {
        let mut definitions = ToolName::ALL
            .into_iter()
            .filter(|tool| self.tools.granted().contains(tool))
            // In the write phase the artifact is the only way forward. Offering
            // read or edit here is what lets a small model drift back to its
            // sources, or rewrite them instead of producing the output.
            .filter(|tool| !must_write || matches!(tool, ToolName::WriteFile))
            .map(super::executor::tool_definition)
            .collect::<Vec<_>>();
        definitions.push(ToolDefinition::function(
            TODO_TOOL,
            "Record or update the plan as a short list of items.",
            json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string"},
                                "done": {"type": "boolean"}
                            },
                            "required": ["text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        ));
        definitions.push(ToolDefinition::function(
            FINISH_TOOL,
            "Declare the job complete. Only call this after the required output file is written.",
            json!({
                "type": "object",
                "properties": {"summary": {"type": "string"}},
                "required": [],
                "additionalProperties": false
            }),
        ));
        definitions
    }
}

fn is_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".md", ".txt", ".json", ".csv", ".log"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// Source files the loop has not read yet, excluding the output it must write.
fn unread_files(
    workspace: &Path,
    read: &BTreeSet<String>,
    deliverable: Option<&str>,
) -> Vec<String> {
    list_workspace(workspace)
        .lines()
        .filter(|line| is_source(line))
        .filter(|line| !line.starts_with("out/"))
        .filter(|line| Some(*line) != deliverable)
        .filter(|line| !read.contains(*line))
        .map(str::to_owned)
        .collect()
}

/// Refuses writes that would destroy the job from the inside: an empty file, or
/// a source the model is supposed to read rather than rewrite.
fn guard_write(
    tool: ToolName,
    arguments: &Value,
    read: &BTreeSet<String>,
    deliverable: Option<&str>,
) -> Option<String> {
    if tool != ToolName::WriteFile {
        return None;
    }
    let path = arguments.get("path").and_then(Value::as_str)?.trim();
    let content = arguments
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if content.trim().len() < 16 {
        return Some(format!(
            "refused: the content for {path} is empty or a placeholder. Write the complete file."
        ));
    }
    let clean = path.trim_start_matches("./").replace('\\', "/");
    if read.contains(&clean) && deliverable != Some(clean.as_str()) {
        return Some(format!(
            "refused: {path} is a source file. Write the required output instead of rewriting it."
        ));
    }
    None
}

/// Whether a written path now holds real bytes.
fn non_empty(workspace: &Path, path: &str) -> bool {
    safe_join(workspace, path)
        .and_then(|joined| std::fs::metadata(joined).ok())
        .is_some_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn is_write(tool: ToolName) -> bool {
    matches!(tool, ToolName::WriteFile | ToolName::EditFile)
}

fn emit(on_event: &mut (dyn FnMut(Value) + Send), turns: &[LoopTurn], todos: &[TodoItem]) {
    let Some(turn) = turns.last() else {
        return;
    };
    let calls = &turn.calls;
    on_event(json!({
        "kind": "loop.turn",
        "index": turn.index,
        "note": turn.note,
        "calls": calls,
        "todos": todos,
    }));
}

fn decode_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
}

fn apply_todos(todos: &mut Vec<TodoItem>, arguments: &Value) -> Result<(), String> {
    let items = arguments
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "todo_write needs an items array".to_owned())?;
    if items.len() > 16 {
        return Err("todo_write accepts at most 16 items".to_owned());
    }
    let mut next = Vec::with_capacity(items.len());
    for item in items {
        if let Some(text) = item.as_str() {
            next.push(TodoItem {
                text: text.to_owned(),
                done: false,
            });
            continue;
        }
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "every todo item needs text".to_owned())?;
        next.push(TodoItem {
            text: text.to_owned(),
            done: item.get("done").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    *todos = next;
    Ok(())
}

fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "(empty)".to_owned();
    }
    todos
        .iter()
        .map(|todo| format!("{} {}", if todo.done { "[x]" } else { "[ ]" }, todo.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact(arguments: &Value) -> String {
    let text = serde_json::to_string(arguments).unwrap_or_default();
    clip(&text, 160)
}

fn clip(text: &str, limit: usize) -> String {
    let clipped: String = text.chars().take(limit).collect();
    if clipped.chars().count() < text.chars().count() {
        format!("{clipped} [truncated]")
    } else {
        clipped
    }
}

/// A bounded listing of the real workspace, so the model acts on files that exist.
fn list_workspace(root: &Path) -> String {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < 2 {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push(relative);
        }
    }
    files.sort();
    files.truncate(60);
    if files.is_empty() {
        "(empty workspace)".to_owned()
    } else {
        files.join("\n")
    }
}

/// Measures the required artifact instead of trusting the model's claim.
#[must_use]
pub fn verify_deliverable(workspace: &Path, path: &str) -> Option<DeliverableProof> {
    let joined = safe_join(workspace, path)?;
    let metadata = std::fs::metadata(&joined).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    let bytes = std::fs::read(&joined).ok()?;
    let digest = digest(&bytes);
    Some(DeliverableProof {
        path: path.replace('\\', "/"),
        bytes: metadata.len(),
        digest,
    })
}

fn safe_join(workspace: &Path, relative: &str) -> Option<PathBuf> {
    let trimmed = relative.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.contains(':') {
        return None;
    }
    let mut path = workspace.to_path_buf();
    for part in trimmed.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

/// FNV-1a over the artifact bytes: a cheap, stable proof of what was verified.
fn digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The tool list this loop would offer, for callers that print a capability report.
#[must_use]
pub fn loop_tools(registry: &ToolRegistry) -> BTreeMap<String, bool> {
    ToolName::ALL
        .into_iter()
        .map(|tool| (tool.as_str().to_owned(), registry.granted().contains(&tool)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todos_accept_objects_and_plain_strings() {
        let mut todos = Vec::new();
        apply_todos(
            &mut todos,
            &json!({"items": [{"text": "read notes", "done": true}, "write report"]}),
        )
        .expect("valid todos");
        assert_eq!(todos.len(), 2);
        assert!(todos[0].done);
        assert_eq!(todos[1].text, "write report");
        assert!(render_todos(&todos).contains("[x] read notes"));
    }

    #[test]
    fn a_deliverable_must_be_non_empty_to_verify() {
        let root = std::env::temp_dir().join(format!("lfm3-loop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("out")).expect("workspace");
        std::fs::write(root.join("out/report.md"), "").expect("empty file");
        assert!(verify_deliverable(&root, "out/report.md").is_none());

        std::fs::write(root.join("out/report.md"), "# Report\n24 crates\n").expect("report");
        let proof = verify_deliverable(&root, "out/report.md").expect("verified");
        assert_eq!(proof.path, "out/report.md");
        assert!(proof.bytes > 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_deliverable_cannot_escape_the_workspace() {
        let root = std::env::temp_dir();
        assert!(verify_deliverable(&root, "../etc/passwd").is_none());
        assert!(verify_deliverable(&root, "C:/Windows/win.ini").is_none());
        assert!(verify_deliverable(&root, "/etc/hosts").is_none());
    }

    #[test]
    fn an_empty_write_is_refused() {
        let problem = guard_write(
            ToolName::WriteFile,
            &json!({"path": "out/report.md", "content": ""}),
            &BTreeSet::new(),
            Some("out/report.md"),
        );
        assert!(problem.expect("refused").contains("empty"));
    }

    #[test]
    fn rewriting_a_source_file_is_refused() {
        let mut read = BTreeSet::new();
        read.insert("notes/alpha.md".to_owned());
        let problem = guard_write(
            ToolName::WriteFile,
            &json!({"path": "notes/alpha.md", "content": "a full replacement text"}),
            &read,
            Some("out/report.md"),
        );
        assert!(problem.expect("refused").contains("source file"));
    }

    #[test]
    fn unread_files_exclude_the_output_and_read_sources() {
        let root = std::env::temp_dir().join(format!("lfm3-unread-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("notes")).expect("workspace");
        std::fs::create_dir_all(root.join("out")).expect("out");
        std::fs::write(root.join("notes/alpha.md"), "a").expect("alpha");
        std::fs::write(root.join("notes/beta.md"), "b").expect("beta");

        let mut read = BTreeSet::new();
        read.insert("notes/alpha.md".to_owned());
        let unread = unread_files(&root, &read, Some("out/report.md"));
        assert_eq!(unread, vec!["notes/beta.md".to_owned()]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn malformed_tool_arguments_become_an_empty_object() {
        assert_eq!(decode_arguments("not json"), json!({}));
        assert_eq!(decode_arguments(""), json!({}));
    }
}
