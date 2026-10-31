//! One-action executor: turns a step request into exactly one tool call.
//!
//! The executor never parses free-form plans at execution time and never
//! accepts two actions. Anything other than exactly one granted tool call with
//! a JSON object of arguments is a typed invalid-action failure that recovery
//! can classify, so a broken model reply can never touch the workspace.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    mission::{AcceptanceSpec, ExecutionStrategy},
    model::{
        ChatMessage, ChatRequest, ModelError, ModelGateway, ResponseFormat, ToolCall,
        ToolDefinition,
    },
    tools::{ToolError, ToolName, ToolRegistry, ToolRequest},
    CancellationChild,
};

use super::{
    packets::FailurePacket, planner::decode_single_json, DriverError, StepDriver, StepExecution,
    StepRequest,
};

/// The single action one attempt will take.
#[derive(Clone, Debug, PartialEq)]
pub struct StepAction {
    pub call_id: String,
    pub tool: ToolName,
    pub arguments: Value,
}

/// Drives steps by asking the local text model for one tool call at a time.
#[derive(Clone)]
pub struct Executor {
    gateway: Arc<dyn ModelGateway>,
    tools: Arc<ToolRegistry>,
    model: String,
    max_tokens: u32,
    /// Tool calls one attempt may make. Missions keep one; a job step may
    /// write several files or sections before the checks run.
    max_calls: usize,
}

impl Executor {
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
            max_tokens: 512,
            max_calls: 1,
        }
    }

    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    #[must_use]
    pub fn with_max_calls(mut self, max_calls: usize) -> Self {
        self.max_calls = max_calls.max(1);
        self
    }

    /// Asks for the tool calls of one step, bounded by `max_calls`.
    async fn next_actions(
        &self,
        request: &StepRequest<'_>,
        cancel: &CancellationChild,
    ) -> Result<Vec<StepAction>, DriverError> {
        ensure_live(cancel)?;
        let definitions = self.step_tool_definitions(request);
        if definitions.is_empty() {
            return Err(DriverError::InvalidAction(
                "the mission grants no tools for this step".to_owned(),
            ));
        }
        let chat = ChatRequest::new(vec![
            ChatMessage::system(self.system_brief()),
            ChatMessage::user(self.user_brief(request)),
        ])
        .with_tools(definitions)
        .with_tool_choice("required")
        .with_temperature(0.1)
        .with_max_tokens(self.max_tokens);
        let response = match cancel.run(self.gateway.act(chat, cancel.clone())).await {
            Ok(Ok(response)) => response,
            Ok(Err(ModelError::Cancelled(reason))) => return Err(DriverError::Cancelled(reason)),
            Ok(Err(error)) => return Err(DriverError::Failed(error.to_string())),
            Err(cancelled) => return Err(DriverError::Cancelled(cancelled.reason)),
        };
        let calls = response.tool_calls();
        if calls.is_empty() {
            return Err(DriverError::InvalidAction(
                "the model returned no tool call".to_owned(),
            ));
        }
        if calls.len() > self.max_calls {
            return Err(DriverError::InvalidAction(format!(
                "at most {} tool call(s) are allowed for one step, got {}",
                self.max_calls,
                calls.len()
            )));
        }
        calls
            .iter()
            .map(|call| self.validate_call(request.acceptance, call))
            .collect()
    }

    #[must_use]
    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// Asks for exactly one granted tool call and validates its arguments.
    pub async fn next_action(
        &self,
        request: &StepRequest<'_>,
        cancel: &CancellationChild,
    ) -> Result<StepAction, DriverError> {
        ensure_live(cancel)?;
        let definitions = self.step_tool_definitions(request);
        if definitions.is_empty() {
            return Err(DriverError::InvalidAction(
                "the mission grants no tools for this step".to_owned(),
            ));
        }
        let chat = ChatRequest::new(vec![
            ChatMessage::system(self.system_brief()),
            ChatMessage::user(self.user_brief(request)),
        ])
        .with_tools(definitions)
        .with_tool_choice("required")
        .with_temperature(0.1)
        .with_max_tokens(self.max_tokens);
        let response = match cancel.run(self.gateway.act(chat, cancel.clone())).await {
            Ok(Ok(response)) => response,
            Ok(Err(ModelError::Cancelled(reason))) => return Err(DriverError::Cancelled(reason)),
            Ok(Err(error)) => return Err(DriverError::Failed(error.to_string())),
            Err(cancelled) => return Err(DriverError::Cancelled(cancelled.reason)),
        };
        let calls = response.tool_calls();
        let call = match calls {
            [call] => call,
            [] => {
                return Err(DriverError::InvalidAction(
                    "the model returned no tool call".to_owned(),
                ));
            }
            _ => {
                return Err(DriverError::InvalidAction(format!(
                    "exactly one tool call is required, got {}",
                    calls.len()
                )));
            }
        };
        self.validate_call(request.acceptance, call)
    }

    fn validate_call(
        &self,
        acceptance: &AcceptanceSpec,
        call: &ToolCall,
    ) -> Result<StepAction, DriverError> {
        let name = &call.function.name;
        let tool = ToolName::parse(name)
            .ok_or_else(|| DriverError::InvalidAction(format!("unknown tool {name:?}")))?;
        if !acceptance.allowed_tools.contains(tool.as_str()) {
            return Err(DriverError::InvalidAction(format!(
                "tool {name:?} is not granted by this mission"
            )));
        }
        if !self.tools.granted().contains(&tool) {
            return Err(DriverError::InvalidAction(format!(
                "tool {name:?} is not installed in this workspace"
            )));
        }
        let arguments: Value = serde_json::from_str(&call.function.arguments).map_err(|error| {
            DriverError::InvalidAction(format!("tool arguments are not JSON: {error}"))
        })?;
        if !arguments.is_object() {
            return Err(DriverError::InvalidAction(
                "tool arguments must be a JSON object".to_owned(),
            ));
        }
        Ok(StepAction {
            call_id: call.id.clone(),
            tool,
            arguments,
        })
    }

    /// Asks the planner role for a replacement strategy after no progress.
    pub async fn replan_strategy(
        &self,
        request: &StepRequest<'_>,
        cancel: &CancellationChild,
    ) -> Result<ExecutionStrategy, DriverError> {
        ensure_live(cancel)?;
        let Some(packet) = request.packet else {
            return Err(DriverError::InvalidAction(
                "replanning needs a failure packet".to_owned(),
            ));
        };
        let chat = ChatRequest::new(vec![
            ChatMessage::system(
                "You are the strategy planner for a local, purpose-bound desktop agent. \
                 The current strategy made no progress. Propose a different strategy that \
                 keeps the accepted goal, steps, tools, and checks unchanged. \
                 Reply with exactly one JSON object: \
                 {\"acceptance_hash\":\"...\",\"actions\":[{\"step_index\":1,\
                 \"tool\":\"allowed tool\",\"args\":{}}]}. No prose, no code fences.",
            ),
            ChatMessage::user(format!(
                "Goal: {}\nStep {}: {}\nCriterion: {}\nFailure: {}\n\
                 Accepted strategy may only choose tools from: {}\n\
                 The acceptance hash must stay exactly {}\n\
                 Reply with the replacement strategy JSON.",
                request.acceptance.goal,
                request.step.index,
                request.step.title,
                request.step.criterion,
                packet_json(packet),
                request
                    .acceptance
                    .allowed_tools
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                packet.acceptance_hash,
            )),
        ])
        .with_temperature(0.0)
        .with_max_tokens(self.max_tokens)
        .with_response_format(ResponseFormat::json_object());
        let response = match cancel.run(self.gateway.plan(chat, cancel.clone())).await {
            Ok(Ok(response)) => response,
            Ok(Err(ModelError::Cancelled(reason))) => return Err(DriverError::Cancelled(reason)),
            Ok(Err(error)) => return Err(DriverError::Failed(error.to_string())),
            Err(cancelled) => return Err(DriverError::Cancelled(cancelled.reason)),
        };
        let content = response.content().ok_or_else(|| {
            DriverError::InvalidAction("the model returned no strategy content".to_owned())
        })?;
        let value = decode_single_json(content)
            .map_err(|error| DriverError::InvalidAction(format!("strategy: {error}")))?;
        let strategy: ExecutionStrategy = serde_json::from_value(value).map_err(|error| {
            DriverError::InvalidAction(format!("strategy fields are invalid: {error}"))
        })?;
        self.validate_strategy(request.acceptance, &strategy)?;
        Ok(strategy)
    }

    fn validate_strategy(
        &self,
        acceptance: &AcceptanceSpec,
        strategy: &ExecutionStrategy,
    ) -> Result<(), DriverError> {
        let frozen = acceptance.clone().freeze().map_err(|error| {
            DriverError::InvalidAction(format!("acceptance is invalid: {error}"))
        })?;
        if strategy.acceptance_hash != *frozen.hash() {
            return Err(DriverError::InvalidAction(
                "the strategy changed the frozen acceptance hash".to_owned(),
            ));
        }
        for action in &strategy.actions {
            if !acceptance.allowed_tools.contains(&action.tool) {
                return Err(DriverError::InvalidAction(format!(
                    "strategy uses tool {:?}, which this mission does not allow",
                    action.tool
                )));
            }
            if !acceptance
                .steps
                .iter()
                .any(|step| step.index == action.step_index)
            {
                return Err(DriverError::InvalidAction(format!(
                    "strategy references unknown step {}",
                    action.step_index
                )));
            }
        }
        Ok(())
    }

    fn system_brief(&self) -> String {
        "You are the acting model for a local, purpose-bound desktop agent. \
         Perform exactly one tool call for the current step. \
         Choose only from the provided tools. Use paths relative to the workspace. \
         Do not explain, do not call more than one tool, and do not repeat an action \
         that already failed."
            .to_owned()
    }

    fn user_brief(&self, request: &StepRequest<'_>) -> String {
        let history = if request.history.is_empty() {
            "(this is the first step)".to_owned()
        } else {
            request
                .history
                .iter()
                .map(|observation| {
                    format!(
                        "Step {} ({}), {} -> {}",
                        observation.step_index,
                        observation.title,
                        observation.tools.join(", "),
                        clip_observation(&observation.output)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let retry = match request.packet {
            Some(packet) => format!(
                "\nThis is attempt {} under strategy {}. Do not repeat the failed action.\n\
                 Failure: {}\n",
                request.attempt_number,
                request.strategy,
                packet_json(packet)
            ),
            None => String::new(),
        };
        format!(
            "Goal: {}\nStep {}: {}\nCriterion: {}\nUse only workspace-relative paths.\n\
             What earlier steps observed:\n{}\n{}",
            request.acceptance.goal,
            request.step.index,
            request.step.title,
            planned_criterion(request),
            history,
            retry
        )
    }

    /// Grants narrow to the planned action on a batching job step, so a write
    /// step cannot fall back to reading instead of producing its artifact.
    fn step_tool_definitions(&self, request: &StepRequest<'_>) -> Vec<ToolDefinition> {
        let definitions = self.tool_definitions(request.acceptance);
        if self.max_calls == 1 {
            return definitions;
        }
        let Some(planned) = request.step.tool.as_deref() else {
            return definitions;
        };
        let narrowed = definitions
            .iter()
            .filter(|definition| definition.function.name == planned)
            .cloned()
            .collect::<Vec<_>>();
        if narrowed.is_empty() {
            definitions
        } else {
            narrowed
        }
    }

    fn tool_definitions(&self, acceptance: &AcceptanceSpec) -> Vec<ToolDefinition> {
        ToolName::ALL
            .into_iter()
            .filter(|tool| {
                acceptance.allowed_tools.contains(tool.as_str())
                    && self.tools.granted().contains(tool)
            })
            .map(tool_definition)
            .collect()
    }
}

impl StepDriver for Executor {
    fn model(&self) -> String {
        self.model.clone()
    }

    fn execute<'a>(
        &'a self,
        request: StepRequest<'a>,
        cancel: &'a CancellationChild,
    ) -> super::BoxFuture<'a, Result<StepExecution, DriverError>> {
        Box::pin(async move {
            let actions = self.next_actions(&request, cancel).await?;
            let single = actions.len() == 1;
            let mut results = Vec::with_capacity(actions.len());
            let mut recorded = Vec::with_capacity(actions.len());
            for action in actions {
                let call = ToolRequest::new(
                    request.attempt.clone(),
                    action.call_id.clone(),
                    action.tool,
                    action.arguments.clone(),
                );
                let result = self
                    .tools
                    .execute(call, cancel)
                    .await
                    .map_err(driver_error)?;
                results.push(result);
                recorded.push(json!({
                    "tool": action.tool.as_str(),
                    "arguments": action.arguments,
                    "call_id": action.call_id,
                }));
            }
            let action = if single {
                recorded.pop().unwrap_or_else(|| json!({}))
            } else {
                json!({ "actions": recorded })
            };
            Ok(StepExecution {
                model: self.model.clone(),
                action,
                tool_results: results,
                command_runs: Vec::new(),
            })
        })
    }

    fn replan_strategy<'a>(
        &'a self,
        request: StepRequest<'a>,
        cancel: &'a CancellationChild,
    ) -> super::BoxFuture<'a, Result<ExecutionStrategy, DriverError>> {
        Box::pin(async move { Executor::replan_strategy(self, &request, cancel).await })
    }
}

fn ensure_live(cancel: &CancellationChild) -> Result<(), DriverError> {
    if cancel.is_cancelled() {
        return Err(DriverError::Cancelled(crate::CancelReason::Requested));
    }
    if cancel.remaining().is_zero() {
        return Err(DriverError::Cancelled(crate::CancelReason::Deadline));
    }
    Ok(())
}

fn driver_error(error: ToolError) -> DriverError {
    match error {
        ToolError::Cancelled(reason) => DriverError::Cancelled(reason),
        ToolError::Io { .. } | ToolError::Process(_) => DriverError::Failed(error.to_string()),
        other => DriverError::InvalidAction(other.to_string()),
    }
}

fn clip_observation(output: &str) -> String {
    let clipped: String = output.chars().take(1_200).collect();
    if clipped.len() < output.len() {
        format!("{clipped} [truncated]")
    } else {
        clipped
    }
}

fn packet_json(packet: &FailurePacket) -> String {
    serde_json::to_string(packet).unwrap_or_else(|_| "{}".to_owned())
}

/// States the planned action next to the criterion, so a small model does not
/// drift to a different tool that merely mentions the same files.
fn planned_criterion(request: &StepRequest<'_>) -> String {
    match request.step.tool.as_deref() {
        Some(tool) => format!(
            "{}\nPlanned action: call {tool} once with arguments that satisfy the criterion.",
            request.step.criterion
        ),
        None => request.step.criterion.to_owned(),
    }
}

pub(crate) fn tool_definition(tool: ToolName) -> ToolDefinition {
    let (description, parameters) = match tool {
        ToolName::ListFiles => (
            "List files under the workspace.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": [],
                "additionalProperties": false,
            }),
        ),
        ToolName::ReadFile => (
            "Read one UTF-8 file from the workspace.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false,
            }),
        ),
        ToolName::WriteFile => (
            "Write one UTF-8 file into the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        ),
        ToolName::EditFile => (
            "Replace exactly one occurrence of old_text with new_text in a file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false,
            }),
        ),
        ToolName::SearchFiles => (
            "Search workspace names and text for words.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"},
                    "mode": {"type": "string", "enum": ["name", "content", "both"]},
                    "max_results": {"type": "integer"},
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
        ),
        ToolName::RelatedFiles => (
            "Find workspace files that share distinctive terms with one source file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_results": {"type": "integer"},
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        ),
        ToolName::Look => (
            "Ask the vision model one bounded question about one image. Returns measured \
             file facts plus an observation; it cannot plan or judge.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "instruction": {"type": "string"},
                },
                "required": ["path", "instruction"],
                "additionalProperties": false,
            }),
        ),
        ToolName::Transcribe => (
            "Transcribe one local recording verbatim. Returns measured file facts plus the \
             transcript; it cannot summarize or label speakers.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "language": {"type": "string"},
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        ),
        ToolName::Speak => (
            "Synthesize speech from text into a workspace file.",
            json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "path": {"type": "string"},
                },
                "required": ["text"],
                "additionalProperties": false,
            }),
        ),
    };
    ToolDefinition::function(tool.as_str(), description, parameters)
}
