//! Model-driven agent loop for a stated job.
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
    /// A deterministic check the harness runs before accepting `done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<TodoCheck>,
}

/// A check a plan step may attach to itself. It is evaluated against the real
/// workspace, so "done" means measured rather than asserted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoCheck {
    pub kind: TodoCheckKind,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoCheckKind {
    FileExists,
    DirExists,
    ContentMatches,
}

impl TodoCheckKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileExists => "file_exists",
            Self::DirExists => "dir_exists",
            Self::ContentMatches => "content_matches",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "file_exists" => Some(Self::FileExists),
            "dir_exists" => Some(Self::DirExists),
            "content_matches" => Some(Self::ContentMatches),
            _ => None,
        }
    }
}

/// Which part of the loop a turn belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopPhase {
    /// Write the short plan. No workspace side effect is offered yet.
    Plan,
    /// Work through the plan with one bounded action per turn.
    Work,
    /// Re-read the written output, fix it, or finish.
    Review,
}

impl LoopPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Work => "work",
            Self::Review => "review",
        }
    }
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
    #[serde(default)]
    pub phase: String,
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
    /// Require a short written plan before any workspace action.
    pub plan_first: bool,
    /// Maximum items accepted in the written plan.
    pub max_plan_steps: usize,
}

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            max_turns: 40,
            max_calls: 3,
            replay: 8,
            plan_first: true,
            max_plan_steps: 8,
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
        if !ToolName::ALL
            .iter()
            .any(|tool| self.tools.granted().contains(tool))
        {
            return Err(LoopError::NoTools);
        }
        let requested = deliverable.map(clean_relative);
        let deliverable = requested.as_deref();
        let mut todos: Vec<TodoItem> = Vec::new();
        let mut turns: Vec<LoopTurn> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stall = 0u32;
        let mut idle = 0u32;
        let mut read_paths: BTreeSet<String> = BTreeSet::new();
        // A small model can read forever. Once every source has been seen, the
        // only useful next action is producing the artifact.
        let mut must_write = false;
        let mut reviewed = false;
        let mut review_refusals = 0u32;
        // The plan gate is the strongest compensation for a 1.7B model's weak
        // long-horizon planning: it must externalize a short plan before it can
        // touch the workspace.
        let mut phase = if self.options.plan_first && self.options.max_turns > 1 {
            LoopPhase::Plan
        } else {
            LoopPhase::Work
        };
        let mut stopped = String::from("turn budget");
        let mut status = LoopStatus::TurnBudget;

        for index in 1..=self.options.max_turns {
            if cancel.is_cancelled() {
                status = LoopStatus::Deadline;
                stopped = "job deadline reached".to_owned();
                break;
            }
            if stall >= 3 && !must_write {
                // A measured artifact outranks a stalled review turn: the model
                // had its chance to re-read and repair the output.
                if phase == LoopPhase::Review {
                    if let Some(proof) =
                        deliverable.and_then(|path| verify_deliverable(workspace, path))
                    {
                        return Ok(self.report(
                            LoopStatus::Passed,
                            goal,
                            Some(proof),
                            todos,
                            turns,
                            "the output verified; the review turn was skipped",
                        ));
                    }
                }
                status = LoopStatus::Stalled;
                stopped = if phase == LoopPhase::Plan {
                    "three turns without a written plan".to_owned()
                } else {
                    "three turns without a productive call".to_owned()
                };
                break;
            }
            let offered = self.tool_definitions(phase, must_write);
            let brief = self.brief(
                goal,
                deliverable,
                workspace,
                phase,
                &todos,
                &turns,
                must_write,
                &read_paths,
                reviewed,
            );
            let chat = ChatRequest::new(vec![
                ChatMessage::system(self.system_brief()),
                ChatMessage::user(brief),
            ])
            .with_tools(offered)
            .with_tool_choice("required")
            .with_sampling(super::profile::LFM3_TEXT.sampling)
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
                phase: phase.as_str().to_owned(),
                note: String::new(),
                calls: Vec::new(),
            };
            if calls.is_empty() {
                stall += 1;
                turn.note = if phase == LoopPhase::Plan {
                    "no plan was written".to_owned()
                } else {
                    "no tool call was made".to_owned()
                };
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
            let mut wrote_deliverable = false;
            let mut reviewed_this_turn = false;
            let mut finished = false;
            for call in calls {
                let name = call.function.name.clone();
                let arguments = decode_arguments(&call.function.arguments);
                if name == TODO_TOOL {
                    match apply_todos(
                        &mut todos,
                        &arguments,
                        workspace,
                        phase == LoopPhase::Plan,
                        self.options.max_plan_steps,
                    ) {
                        Ok(outcome) => {
                            let rejected = !outcome.failed.is_empty();
                            let output = outcome.render(&todos);
                            turn.calls.push(LoopCall {
                                tool: name,
                                arguments,
                                ok: !rejected,
                                output,
                            });
                            if phase == LoopPhase::Plan && !todos.is_empty() {
                                phase = LoopPhase::Work;
                                productive = true;
                                progressed = true;
                            } else if !rejected {
                                productive = true;
                            }
                        }
                        Err(error) => {
                            turn.calls.push(LoopCall {
                                tool: name,
                                arguments,
                                ok: false,
                                output: error,
                            });
                        }
                    }
                    continue;
                }
                if name == FINISH_TOOL {
                    if phase == LoopPhase::Plan {
                        turn.calls.push(LoopCall {
                            tool: name,
                            arguments,
                            ok: false,
                            output: "finish is unavailable until you write the plan".to_owned(),
                        });
                        continue;
                    }
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
                // A call refused before execution has not been "seen": after a
                // phase change the same call can become the right action.
                if seen.contains(&signature) {
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
                    continue;
                };
                // The offered tool list is a harness rule, not a suggestion.
                // A model that emits a tool outside the current phase is
                // refused before anything touches the workspace.
                if !self.tool_allowed(tool, phase, must_write) {
                    turn.calls.push(LoopCall {
                        tool: name,
                        arguments,
                        ok: false,
                        output: format!(
                            "this tool is not available in the {} phase",
                            phase.as_str()
                        ),
                    });
                    continue;
                }
                if let Some(problem) = guard_write(tool, &arguments, &read_paths, deliverable) {
                    turn.calls.push(LoopCall {
                        tool: name,
                        arguments,
                        ok: false,
                        output: problem,
                    });
                    continue;
                }
                // In the write phase nothing but the required output is
                // useful; a small model will otherwise keep producing side
                // files instead of the artifact the job is measured on.
                if must_write {
                    if let Some(required) = deliverable {
                        let written = arguments
                            .get("path")
                            .and_then(Value::as_str)
                            .map(clean_relative)
                            .unwrap_or_default();
                        if written != required {
                            turn.calls.push(LoopCall {
                                tool: name,
                                arguments,
                                ok: false,
                                output: format!(
                                    "refused: write the required output {required}, not another file"
                                ),
                            });
                            continue;
                        }
                    }
                }
                let request =
                    ToolRequest::new(AttemptId::new(), call.id.clone(), tool, arguments.clone());
                seen.insert(signature);
                match self.tools.execute(request, &cancel.child()).await {
                    Ok(result) => {
                        let ok = result.succeeded();
                        let output = clip(&result.output, 1_500);
                        if ok {
                            productive = true;
                            if tool == ToolName::ReadFile {
                                if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                                    let clean = clean_relative(path);
                                    read_paths.insert(clean.clone());
                                    progressed = true;
                                    if phase == LoopPhase::Review
                                        && deliverable == Some(clean.as_str())
                                    {
                                        reviewed_this_turn = true;
                                    }
                                }
                            }
                            if is_write(tool) {
                                let written = arguments
                                    .get("path")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                let clean = clean_relative(written);
                                if non_empty(workspace, &clean) {
                                    progressed = true;
                                    if deliverable == Some(clean.as_str()) {
                                        wrote_deliverable = true;
                                    }
                                }
                            }
                            if phase == LoopPhase::Work
                                && deliverable.is_some()
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
            if wrote_deliverable {
                // Writing the artifact opens a review turn: the model can
                // re-read it, repair it, or finish. `must_write` is cleared so
                // the review turn can actually read.
                phase = LoopPhase::Review;
                must_write = false;
                reviewed = reviewed_this_turn;
                review_refusals = 0;
            } else if reviewed_this_turn {
                reviewed = true;
            }
            if progressed {
                idle = 0;
            } else {
                idle += 1;
            }
            turns.push(turn);
            emit(on_event, &turns, &todos);

            let proof = deliverable.and_then(|path| verify_deliverable(workspace, path));
            let mut completion_handled = false;
            if phase == LoopPhase::Review {
                if wrote_deliverable {
                    if reviewed {
                        if let Some(proof) = &proof {
                            return Ok(self.report(
                                LoopStatus::Passed,
                                goal,
                                Some(proof.clone()),
                                todos,
                                turns,
                                "the output verified after you re-read it",
                            ));
                        }
                    } else {
                        if finished {
                            stall += 1;
                            if let Some(last) = turns.last_mut() {
                                last.note = "finish deferred: re-read the output before finishing"
                                    .to_owned();
                            }
                        }
                        completion_handled = true;
                    }
                } else if proof.is_some() {
                    if finished && !reviewed && review_refusals == 0 {
                        review_refusals += 1;
                        stall += 1;
                        if let Some(last) = turns.last_mut() {
                            last.note = "finish refused: re-read the output first".to_owned();
                        }
                        completion_handled = true;
                    } else {
                        let stop = if reviewed {
                            "the output verified after a review turn"
                        } else {
                            "the output verified; finish accepted after the review turn"
                        };
                        return Ok(self.report(
                            LoopStatus::Passed,
                            goal,
                            proof,
                            todos,
                            turns,
                            stop,
                        ));
                    }
                }
            }
            // Reading forever is not progress on a job that must produce an
            // artifact. Six turns without a write is a stalled job, not a pass.
            if !completion_handled && deliverable.is_some() && proof.is_none() && idle >= 6 {
                status = LoopStatus::Stalled;
                stopped = "six turns without writing the required output".to_owned();
                break;
            }
            if !completion_handled && finished {
                match (&proof, deliverable) {
                    (Some(proof), _) => {
                        return Ok(self.report(
                            LoopStatus::Passed,
                            goal,
                            Some(proof.clone()),
                            todos,
                            turns,
                            "the model finished and the deliverable verified",
                        ));
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
                        return Ok(self.report(
                            LoopStatus::Passed,
                            goal,
                            None,
                            todos,
                            turns,
                            "the model finished; the job had no required output",
                        ));
                    }
                }
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
        Ok(self.report(status, goal, proof, todos, turns, stopped))
    }

    #[allow(clippy::too_many_arguments)]
    fn report(
        &self,
        status: LoopStatus,
        goal: &str,
        deliverable: Option<DeliverableProof>,
        todos: Vec<TodoItem>,
        turns: Vec<LoopTurn>,
        stopped: impl Into<String>,
    ) -> LoopReport {
        LoopReport {
            status,
            model: self.model.clone(),
            goal: goal.to_owned(),
            deliverable,
            todos,
            turns,
            stopped: stopped.into(),
        }
    }

    fn system_brief(&self) -> String {
        format!(
            "You are the acting model of a local desktop agent working in a loop. {guidance} \
             Each turn you make one to three tool calls. Call todo_write to keep a short plan \
             and mark an item done only after its check passes. Read a file before you summarize \
             it. Write real content, never a placeholder. The harness enforces the plan gate, \
             checks every claimed step against the workspace, proves the output, and flags \
             numbers no source contains; it never accepts your word alone. Keep going until the \
             required output exists and is complete, then call finish. Do not call finish before \
             the output file is written and re-read. Use workspace-relative paths only.",
            guidance = super::profile::LFM3_TEXT.guidance,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn brief(
        &self,
        goal: &str,
        deliverable: Option<&str>,
        workspace: &Path,
        phase: LoopPhase,
        todos: &[TodoItem],
        turns: &[LoopTurn],
        must_write: bool,
        read_paths: &BTreeSet<String>,
        reviewed: bool,
    ) -> String {
        let missing = deliverable.is_some()
            && deliverable
                .and_then(|path| verify_deliverable(workspace, path))
                .is_none();
        let deliverable_ref = deliverable;
        let grounding = deliverable
            .and_then(|path| grounding_note(goal, workspace, path, todos, turns))
            .map(|note| format!("\n{note}"))
            .unwrap_or_default();
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
        let phase_rule = match phase {
            LoopPhase::Plan => format!(
                "Phase: plan. Do not touch the workspace yet. Call todo_write with one to {} \
                 short steps. Every step that creates or changes a file should carry a check: \
                 {{\"kind\":\"file_exists|dir_exists|content_matches\",\"path\":\"relative/path\"}}; \
                 content_matches also needs a non-empty \"contains\". A step is marked done only \
                 when its check passes in the real workspace.",
                self.options.max_plan_steps
            ),
            LoopPhase::Work if must_write => format!(
                "Phase: work. read_file is unavailable now: write {deliverable} with write_file, \
                 using what you observed, then call finish. Mark plan items done only when their \
                 checks pass."
            ),
            LoopPhase::Work => "Phase: work. Take one bounded action that advances the next open \
                                plan item. Read before you summarize. Mark an item done only when \
                                its check passes; a failed check leaves it open."
                .to_owned(),
            LoopPhase::Review => format!(
                "Phase: review. You wrote {deliverable}. Re-read that file now and compare it with \
                 every requirement in the goal. If anything is missing or wrong, fix it with \
                 write_file. If it is complete, call finish. {}",
                if reviewed {
                    "You have already re-read it this run."
                } else {
                    "It has not been re-read yet."
                }
            ),
        };
        let forced = if must_write && phase == LoopPhase::Work {
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
             {phase_rule}{grounding}\n\n\
             Next action:"
        )
    }

    /// Whether the current phase may execute this tool at all.
    fn tool_allowed(&self, tool: ToolName, phase: LoopPhase, must_write: bool) -> bool {
        if !self.tools.granted().contains(&tool) {
            return false;
        }
        match phase {
            // The plan phase has no workspace tools at all: the model must
            // externalize its short plan before it can act.
            LoopPhase::Plan => false,
            LoopPhase::Review => matches!(
                tool,
                ToolName::ReadFile | ToolName::WriteFile | ToolName::EditFile
            ),
            // In the write phase the artifact is the only way forward.
            // Offering read or edit here is what lets a small model drift
            // back to its sources, or rewrite them instead of producing the
            // output.
            LoopPhase::Work => !must_write || matches!(tool, ToolName::WriteFile),
        }
    }

    fn tool_definitions(&self, phase: LoopPhase, must_write: bool) -> Vec<ToolDefinition> {
        let mut definitions = ToolName::ALL
            .into_iter()
            .filter(|tool| self.tool_allowed(*tool, phase, must_write))
            .map(super::executor::tool_definition)
            .collect::<Vec<_>>();
        definitions.push(ToolDefinition::function(
            TODO_TOOL,
            "Record or update the plan. A done item with a check must pass that check against the real workspace.",
            json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string"},
                                "done": {"type": "boolean"},
                                "check": {
                                    "type": "object",
                                    "properties": {
                                        "kind": {
                                            "type": "string",
                                            "enum": ["file_exists", "dir_exists", "content_matches"]
                                        },
                                        "path": {"type": "string"},
                                        "contains": {"type": "string"}
                                    },
                                    "required": ["kind", "path"],
                                    "additionalProperties": false
                                }
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
        if phase != LoopPhase::Plan {
            definitions.push(ToolDefinition::function(
                FINISH_TOOL,
                "Declare the job complete. Only call this after the required output file is written and re-read.",
                json!({
                    "type": "object",
                    "properties": {"summary": {"type": "string"}},
                    "required": [],
                    "additionalProperties": false
                }),
            ));
        }
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

/// Advisory grounding for the review turn: numbers in the written output that
/// no observation, the goal, or the plan contains. It never changes the loop
/// status; it only gives the model a concrete thing to verify.
fn grounding_note(
    goal: &str,
    workspace: &Path,
    deliverable: &str,
    todos: &[TodoItem],
    turns: &[LoopTurn],
) -> Option<String> {
    let observations = turns
        .iter()
        .flat_map(|turn| turn.calls.iter())
        .filter(|call| grounds(call, deliverable))
        .collect::<Vec<_>>();
    if observations.is_empty() {
        return None;
    }
    let output = std::fs::read_to_string(safe_join(workspace, deliverable)?).ok()?;
    let mut sources = Vec::with_capacity(1 + todos.len() + observations.len());
    sources.push(goal.to_owned());
    sources.extend(todos.iter().map(|todo| todo.text.clone()));
    sources.extend(observations.iter().map(|call| call.output.clone()));
    let refs = sources.iter().map(String::as_str).collect::<Vec<_>>();
    let grounding = super::grounding::check(&output, &refs);
    if grounding.unsupported.is_empty() {
        return None;
    }
    let mut listed = grounding
        .unsupported
        .iter()
        .take(8)
        .cloned()
        .collect::<Vec<_>>();
    if grounding.unsupported.len() > 8 {
        listed.push(format!("and {} more", grounding.unsupported.len() - 8));
    }
    Some(format!(
        "Harness grounding check (advisory): these numbers in {deliverable} do not appear in \
         anything you read or were told: {}. Verify each against a source, or remove it; never \
         invent a figure.",
        listed.join(", ")
    ))
}

/// Whether a successful call produced independent evidence the model could
/// have learned from. Re-reading its own output is not independent evidence.
fn grounds(call: &LoopCall, deliverable: &str) -> bool {
    if !call.ok {
        return false;
    }
    match call.tool.as_str() {
        "look" | "transcribe" => true,
        "read_file" => {
            call.arguments
                .get("path")
                .and_then(Value::as_str)
                .map(clean_relative)
                .as_deref()
                != Some(deliverable)
        }
        _ => false,
    }
}

/// Normalizes a model-supplied workspace path for comparison and journaling.
fn clean_relative(path: &str) -> String {
    path.trim().trim_start_matches("./").replace('\\', "/")
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
    let clean = clean_relative(path);
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
        "phase": turn.phase,
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

/// What happened when the model rewrote its plan.
#[derive(Debug)]
struct TodoOutcome {
    verified: Vec<String>,
    failed: Vec<String>,
    unverified: Vec<String>,
}

impl TodoOutcome {
    fn render(&self, todos: &[TodoItem]) -> String {
        let mut text = render_todos(todos);
        for item in &self.verified {
            text.push_str(&format!("\nverified: {item}"));
        }
        for item in &self.failed {
            text.push_str(&format!("\nnot done: {item}"));
        }
        for item in &self.unverified {
            text.push_str(&format!("\ndone without a check: {item}"));
        }
        text
    }
}

fn apply_todos(
    todos: &mut Vec<TodoItem>,
    arguments: &Value,
    workspace: &Path,
    plan_phase: bool,
    max_plan_steps: usize,
) -> Result<TodoOutcome, String> {
    let items = arguments
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "todo_write needs an items array".to_owned())?;
    if plan_phase && items.is_empty() {
        return Err("the plan needs at least one step".to_owned());
    }
    let maximum = if plan_phase {
        max_plan_steps.clamp(1, 16)
    } else {
        16
    };
    if items.len() > maximum {
        return Err(if plan_phase {
            format!("the first plan accepts at most {maximum} steps")
        } else {
            "todo_write accepts at most 16 items".to_owned()
        });
    }
    let mut next = Vec::with_capacity(items.len());
    for item in items {
        if let Some(text) = item.as_str() {
            next.push(TodoItem {
                text: text.to_owned(),
                done: false,
                check: None,
            });
            continue;
        }
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| "every todo item needs non-empty text".to_owned())?;
        next.push(TodoItem {
            text: text.to_owned(),
            done: item.get("done").and_then(Value::as_bool).unwrap_or(false),
            check: parse_todo_check(item)?,
        });
    }

    let mut outcome = TodoOutcome {
        verified: Vec::new(),
        failed: Vec::new(),
        unverified: Vec::new(),
    };
    for item in &mut next {
        if !item.done {
            continue;
        }
        match &item.check {
            Some(check) => match verify_todo_check(workspace, check) {
                Ok(()) => outcome.verified.push(item.text.clone()),
                Err(problem) => {
                    item.done = false;
                    outcome.failed.push(format!("{} ({problem})", item.text));
                }
            },
            None => outcome.unverified.push(item.text.clone()),
        }
    }
    *todos = next;
    Ok(outcome)
}

/// Parses one optional plan check, rejecting a check the harness could not run.
fn parse_todo_check(item: &Value) -> Result<Option<TodoCheck>, String> {
    let Some(raw) = item.get("check").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if !raw.is_object() {
        return Err("a step check must be an object".to_owned());
    }
    let kind_raw = raw
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "a step check needs a kind".to_owned())?;
    let kind = TodoCheckKind::parse(kind_raw).ok_or_else(|| {
        "a step check kind must be file_exists, dir_exists, or content_matches".to_owned()
    })?;
    let path = raw
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| "a step check needs a workspace-relative path".to_owned())?;
    if safe_join(Path::new("."), path).is_none() {
        return Err(format!("the check path {path:?} leaves the workspace"));
    }
    let contains = raw
        .get("contains")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    if kind == TodoCheckKind::ContentMatches && contains.is_none() {
        return Err("content_matches needs a non-empty contains value".to_owned());
    }
    Ok(Some(TodoCheck {
        kind,
        path: clean_relative(path),
        contains,
    }))
}

/// Evaluates one plan check against the real workspace.
fn verify_todo_check(workspace: &Path, check: &TodoCheck) -> Result<(), String> {
    let joined = safe_join(workspace, &check.path)
        .ok_or_else(|| format!("{} leaves the workspace", check.path))?;
    match check.kind {
        TodoCheckKind::FileExists => {
            if joined.is_file() {
                Ok(())
            } else {
                Err(format!("{} does not exist as a file", check.path))
            }
        }
        TodoCheckKind::DirExists => {
            if joined.is_dir() {
                Ok(())
            } else {
                Err(format!("{} does not exist as a directory", check.path))
            }
        }
        TodoCheckKind::ContentMatches => {
            const MAX_CHECK_BYTES: u64 = 4_000_000;
            let metadata = std::fs::metadata(&joined)
                .map_err(|error| format!("{} cannot be checked: {error}", check.path))?;
            if !metadata.is_file() {
                return Err(format!("{} is not a file", check.path));
            }
            if metadata.len() > MAX_CHECK_BYTES {
                return Err(format!(
                    "{} is larger than the {MAX_CHECK_BYTES}-byte check limit",
                    check.path
                ));
            }
            let text = std::fs::read_to_string(&joined)
                .map_err(|error| format!("{} cannot be read as text: {error}", check.path))?;
            let needle = check.contains.as_deref().unwrap_or_default();
            if text.contains(needle) {
                Ok(())
            } else {
                Err(format!("{} does not contain {needle:?}", check.path))
            }
        }
    }
}

fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "(empty)".to_owned();
    }
    todos
        .iter()
        .map(|todo| {
            let check = todo
                .check
                .as_ref()
                .map(|check| match check.kind {
                    TodoCheckKind::ContentMatches => format!(
                        " [check: content_matches {} {:?}]",
                        check.path,
                        check.contains.as_deref().unwrap_or_default()
                    ),
                    kind => format!(" [check: {} {}]", kind.as_str(), check.path),
                })
                .unwrap_or_default();
            format!(
                "{} {}{}",
                if todo.done { "[x]" } else { "[ ]" },
                todo.text,
                check
            )
        })
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
        let root = std::env::temp_dir().join(format!("lfm3-todos-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("workspace");
        let mut todos = Vec::new();
        apply_todos(
            &mut todos,
            &json!({"items": [{"text": "read notes", "done": true}, "write report"]}),
            &root,
            false,
            8,
        )
        .expect("valid todos");
        assert_eq!(todos.len(), 2);
        assert!(todos[0].done);
        assert!(todos[0].check.is_none());
        assert_eq!(todos[1].text, "write report");
        assert!(render_todos(&todos).contains("[x] read notes"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_plan_cannot_be_empty() {
        let root = std::env::temp_dir();
        let mut todos = Vec::new();
        let error = apply_todos(&mut todos, &json!({"items": []}), &root, true, 8)
            .expect_err("an empty plan is not a plan");
        assert!(error.contains("at least one step"));
    }

    #[test]
    fn a_finished_step_must_pass_its_real_check() {
        let root = std::env::temp_dir().join(format!("lfm3-checks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("out")).expect("workspace");
        std::fs::write(root.join("out/report.md"), "# Report\nalpha beta\n").expect("report");
        let mut todos = Vec::new();
        let outcome = apply_todos(
            &mut todos,
            &json!({"items": [{
                "text": "write the report",
                "done": true,
                "check": {"kind": "content_matches", "path": "out/report.md", "contains": "alpha"}
            }]}),
            &root,
            false,
            8,
        )
        .expect("valid plan");
        assert!(todos[0].done);
        assert_eq!(outcome.verified.len(), 1);
        assert!(outcome.failed.is_empty());

        let outcome = apply_todos(
            &mut todos,
            &json!({"items": [{
                "text": "write the report",
                "done": true,
                "check": {"kind": "content_matches", "path": "out/report.md", "contains": "gamma"}
            }]}),
            &root,
            false,
            8,
        )
        .expect("valid plan");
        assert!(!todos[0].done, "a failed check cannot leave the step done");
        assert_eq!(outcome.failed.len(), 1);
        assert!(outcome.failed[0].contains("does not contain"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_check_cannot_escape_the_workspace() {
        let mut todos = Vec::new();
        let error = apply_todos(
            &mut todos,
            &json!({"items": [{
                "text": "read the host",
                "done": true,
                "check": {"kind": "file_exists", "path": "../etc/passwd"}
            }]}),
            &std::env::temp_dir(),
            false,
            8,
        )
        .expect_err("an escaping check is not a check");
        assert!(error.contains("leaves the workspace"));
    }

    #[test]
    fn content_checks_need_a_needle() {
        let mut todos = Vec::new();
        let error = apply_todos(
            &mut todos,
            &json!({"items": [{
                "text": "write the report",
                "done": false,
                "check": {"kind": "content_matches", "path": "out/report.md"}
            }]}),
            &std::env::temp_dir(),
            false,
            8,
        )
        .expect_err("content_matches without contains asserts nothing");
        assert!(error.contains("contains"));
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

    #[test]
    fn the_review_note_flags_a_number_no_source_contains() {
        let root = std::env::temp_dir().join(format!("lfm3-grounding-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("out")).expect("workspace");
        std::fs::write(
            root.join("out/report.md"),
            "# Report\n\n24 crates in 2019.\n",
        )
        .expect("report");
        let turns = vec![LoopTurn {
            index: 1,
            phase: "work".to_owned(),
            note: String::new(),
            calls: vec![LoopCall {
                tool: "read_file".to_owned(),
                arguments: json!({"path": "notes/alpha.md"}),
                ok: true,
                output: "The workspace holds 24 crates.".to_owned(),
            }],
        }];
        let note =
            grounding_note("Write a report", &root, "out/report.md", &[], &turns).expect("a note");
        assert!(note.contains("2019"), "{note}");
        assert!(
            !note.contains("24"),
            "a sourced number is not flagged: {note}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn re_reading_the_output_is_not_independent_evidence() {
        let root = std::env::temp_dir().join(format!("lfm3-grounding-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("out")).expect("workspace");
        std::fs::write(root.join("out/report.md"), "# Report\n\n2019 was a year.\n")
            .expect("report");
        let turns = vec![LoopTurn {
            index: 1,
            phase: "review".to_owned(),
            note: String::new(),
            calls: vec![LoopCall {
                tool: "read_file".to_owned(),
                arguments: json!({"path": "out/report.md"}),
                ok: true,
                output: "# Report\n\n2019 was a year.\n".to_owned(),
            }],
        }];
        assert!(grounding_note("Write a report", &root, "out/report.md", &[], &turns).is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
