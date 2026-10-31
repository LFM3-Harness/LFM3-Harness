//! Structured planning against a frozen mission capability set.
//!
//! The planner receives everything it needs to propose steps and nothing it
//! may redefine: the mission purpose, typed inputs, the tools and checks the
//! mission grants, the output contract, and the real workspace paths. A reply
//! must be exactly one JSON object; validation failures get exactly one
//! repair. The planner never writes files or chooses executables.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

use crate::{
    mission::{MissionManifest, PlanDraft, PlanError},
    model::{ChatMessage, ChatRequest, ChatResponse, ModelError, ModelGateway, ResponseFormat},
    CancelReason, CancellationContext,
};

/// Everything the planner may legitimately know about one mission run.
#[derive(Clone, Debug)]
pub struct PlanRequest {
    pub mission: MissionManifest,
    pub goal: String,
    /// Verified inputs, by name. Values describe where the bytes live or what
    /// they contain; they are never model-authored commands.
    pub inputs: BTreeMap<String, String>,
    /// Absolute workspace root; plans still use relative paths only.
    pub workspace: PathBuf,
}

/// A plan the planner could not produce honestly.
#[derive(Debug, Error)]
pub enum PlannerError {
    #[error("model call failed: {0}")]
    Model(#[from] ModelError),
    #[error("planning was cancelled: {0}")]
    Cancelled(CancelReason),
    #[error("the model returned no plan content")]
    Empty,
    #[error("the plan is not one complete JSON value: {0}")]
    Decode(String),
    #[error("the plan violates the mission contract: {0}")]
    Invalid(#[from] PlanError),
    #[error("the plan was still invalid after one repair: {0}")]
    RepairExhausted(String),
}

/// Produces plans for one mission capability set.
#[derive(Clone)]
pub struct Planner {
    gateway: Arc<dyn ModelGateway>,
    max_tokens: u32,
    /// Runs before validation on every attempt; task mode installs one.
    rules: Option<fn(&mut PlanDraft)>,
}

impl Planner {
    #[must_use]
    pub fn new(gateway: Arc<dyn ModelGateway>) -> Self {
        Self {
            gateway,
            max_tokens: 1536,
            rules: None,
        }
    }

    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Installs a plan normalizer that runs before validation on every attempt.
    ///
    /// A small model sometimes emits a structurally valid envelope holding a
    /// degenerate check (an empty condition list, or a change check on a read).
    /// Dropping those is not weakening verification: they assert nothing, or
    /// could never be true. What remains is still validated strictly.
    #[must_use]
    pub fn with_rules(mut self, rules: fn(&mut PlanDraft)) -> Self {
        self.rules = Some(rules);
        self
    }

    /// Drafts a validated plan, allowing one repair after a rejection.
    pub async fn draft(
        &self,
        request: PlanRequest,
        cancel: &CancellationContext,
    ) -> Result<PlanDraft, PlannerError> {
        self.draft_repairs(request, cancel, 1).await
    }

    /// Drafts a validated plan with a bounded number of repair attempts.
    pub async fn draft_repairs(
        &self,
        request: PlanRequest,
        cancel: &CancellationContext,
        repairs: u32,
    ) -> Result<PlanDraft, PlannerError> {
        let messages = vec![
            ChatMessage::system(system_brief(&request)),
            ChatMessage::user(user_brief(&request)),
        ];
        let format = plan_format(&request.mission);
        let mut rejected = self.complete(messages, &format, cancel).await?;
        let mut failure = match interpret(&request, &rejected, self.rules) {
            Ok(plan) => return Ok(plan),
            Err(error) => error,
        };

        for _ in 0..repairs {
            if !matches!(failure, PlannerError::Decode(_) | PlannerError::Invalid(_)) {
                return Err(failure);
            }
            rejected = self
                .repair_content(&request, &rejected, &failure.to_string(), cancel)
                .await?;
            failure = match interpret(&request, &rejected, self.rules) {
                Ok(plan) => return Ok(plan),
                Err(error) => error,
            };
        }

        Err(match failure {
            PlannerError::Decode(message) | PlannerError::RepairExhausted(message) => {
                PlannerError::RepairExhausted(message)
            }
            PlannerError::Invalid(invalid) => PlannerError::RepairExhausted(invalid.to_string()),
            other => other,
        })
    }

    /// One repair after a validator error. The rejected text is replayed so
    /// the model edits rather than restarts; the result is validated again.
    pub async fn repair_once(
        &self,
        request: &PlanRequest,
        rejected: &str,
        error: &str,
        cancel: &CancellationContext,
    ) -> Result<PlanDraft, PlannerError> {
        let content = self
            .repair_content(request, rejected, error, cancel)
            .await?;
        interpret(request, &content, self.rules).map_err(|second| {
            PlannerError::RepairExhausted(format!(
                "{error}; retry: {second}; repaired plan: {}",
                truncate(&content, 800)
            ))
        })
    }

    async fn repair_content(
        &self,
        request: &PlanRequest,
        rejected: &str,
        error: &str,
        cancel: &CancellationContext,
    ) -> Result<String, PlannerError> {
        let messages = vec![
            ChatMessage::system(system_brief(request)),
            ChatMessage::user(user_brief(request)),
            ChatMessage::assistant(rejected),
            ChatMessage::user(format!(
                "That plan was rejected by the validator: {error}\n\
                 Keep the corrected plan minimal. It must include a step whose tool is \
                 write_file or edit_file for every required output file.\n\
                 Every check needs its own real arguments: a non-empty path for path checks, a \
                 non-empty `forbidden` list for content_absent, a non-empty `contains` for \
                 content_matches, and no file_changed check on a step that only reads.\n\
                 Reply with exactly one corrected JSON object and nothing else."
            )),
        ];
        self.complete(messages, &plan_format(&request.mission), cancel)
            .await
    }

    async fn complete(
        &self,
        messages: Vec<ChatMessage>,
        format: &ResponseFormat,
        cancel: &CancellationContext,
    ) -> Result<String, PlannerError> {
        ensure_live(cancel)?;
        let request = ChatRequest::new(messages)
            .with_temperature(0.0)
            .with_max_tokens(self.max_tokens)
            .with_response_format(format.clone());
        let child = cancel.child();
        let response = match cancel.run(self.gateway.plan(request, child)).await {
            Ok(Ok(response)) => response,
            Ok(Err(ModelError::Cancelled(reason))) => return Err(PlannerError::Cancelled(reason)),
            Ok(Err(error)) => return Err(PlannerError::Model(error)),
            Err(cancelled) => return Err(PlannerError::Cancelled(cancelled.reason)),
        };
        content_of(&response)
    }
}

fn ensure_live(cancel: &CancellationContext) -> Result<(), PlannerError> {
    if cancel.is_cancelled() {
        return Err(PlannerError::Cancelled(CancelReason::Requested));
    }
    if cancel.remaining().is_zero() {
        return Err(PlannerError::Cancelled(CancelReason::Deadline));
    }
    Ok(())
}

fn content_of(response: &ChatResponse) -> Result<String, PlannerError> {
    match response.content() {
        Some(content) if !content.trim().is_empty() => Ok(content.to_owned()),
        _ => Err(PlannerError::Empty),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_owned(),
    }
}

fn interpret(
    request: &PlanRequest,
    content: &str,
    rules: Option<fn(&mut PlanDraft)>,
) -> Result<PlanDraft, PlannerError> {
    let value = decode_single_json(content)?;
    let mut plan: PlanDraft = serde_json::from_value(value)
        .map_err(|error| PlannerError::Decode(format!("plan fields are invalid: {error}")))?;
    if let Some(rules) = rules {
        rules(&mut plan);
    }
    request.mission.validate_plan(&plan)?;
    Ok(plan)
}

/// Requires exactly one complete JSON value, optionally wrapped in one
/// Markdown code fence. Prose, several values, or trailing text are rejected.
pub fn decode_single_json(raw: &str) -> Result<Value, PlannerError> {
    let body = strip_code_fence(raw)?;
    let body = body.trim();
    if body.is_empty() {
        return Err(PlannerError::Decode("the response is empty".to_owned()));
    }
    let mut deserializer = serde_json::Deserializer::from_str(body);
    let value = Value::deserialize(&mut deserializer)
        .map_err(|error| PlannerError::Decode(format!("expected one JSON value: {error}")))?;
    deserializer
        .end()
        .map_err(|error| PlannerError::Decode(format!("unexpected trailing content: {error}")))?;
    if !value.is_object() {
        return Err(PlannerError::Decode(
            "the response must be a JSON object".to_owned(),
        ));
    }
    Ok(value)
}

fn strip_code_fence(raw: &str) -> Result<&str, PlannerError> {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return Ok(trimmed);
    };
    let Some(newline) = rest.find('\n') else {
        return Err(PlannerError::Decode(
            "unterminated code fence: no opening line".to_owned(),
        ));
    };
    let inner = &rest[newline + 1..];
    let Some(end) = inner.rfind("```") else {
        return Err(PlannerError::Decode(
            "unterminated code fence: no closing fence".to_owned(),
        ));
    };
    if !inner[end + 3..].trim().is_empty() {
        return Err(PlannerError::Decode(
            "content after the closing code fence".to_owned(),
        ));
    }
    Ok(inner[..end].trim())
}

/// The schema a plan must satisfy. Constrained decoding keeps a small model
/// from inventing check kinds or step keys; the validator still checks
/// arguments, indexes, criteria, and the mission contract afterward.
fn plan_format(mission: &MissionManifest) -> ResponseFormat {
    let mutating = mission
        .tools
        .iter()
        .filter(|tool| matches!(tool.as_str(), "write_file" | "edit_file"))
        .map(|tool| json!(tool))
        .collect::<Vec<_>>();
    // A one-step contract with required artifacts can only be satisfied by a
    // writing action; saying so in the schema keeps constrained decoding from
    // spending the step on a read the verifier would reject.
    let writes_only = !mission.output.required_artifacts.is_empty()
        && mission.limits.max_steps == 1
        && !mutating.is_empty();
    let tool_values = if writes_only {
        mutating
    } else {
        let mut values = mission
            .tools
            .iter()
            .map(|tool| json!(tool))
            .collect::<Vec<_>>();
        values.push(Value::Null);
        values
    };
    let mut required = vec!["index", "title", "criterion", "checks"];
    if writes_only {
        required.push("tool");
    }
    ResponseFormat::json_schema(
        "plan",
        json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": mission.limits.max_steps,
                    "items": {
                        "type": "object",
                        "properties": {
                            "index": {"type": "integer"},
                            "title": {"type": "string"},
                            "criterion": {"type": "string"},
                            "tool": {"enum": tool_values},
                            "checks": {
                                "type": "array",
                                "minItems": 1,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "kind": {
                                            "type": "string",
                                            "enum": mission.checks,
                                        }
                                    },
                                    "required": ["kind"],
                                    "additionalProperties": true
                                }
                            }
                        },
                        "required": required,
                        "additionalProperties": false
                    }
                }
            },
            "required": ["steps"],
            "additionalProperties": false
        }),
    )
}

/// The constraint brief: mission purpose, tools, check schemas, output.
fn system_brief(request: &PlanRequest) -> String {
    let mission = &request.mission;
    let tools = mission.tools.join(", ");
    let checks = mission
        .checks
        .iter()
        .map(|kind| format!("- {kind}: {}", check_help(kind)))
        .collect::<Vec<_>>()
        .join("\n");
    let output = serde_json::to_string(&mission.output).unwrap_or_else(|_| "{}".to_owned());
    format!(
        "You are the planner for a local, purpose-bound desktop agent.\n\
         Mission purpose: {description}\n\
         You produce steps a deterministic verifier can check. You never execute \
         anything, never choose commands or executables, and never change the \
         output contract.\n\n\
         Allowed tools (use only these names): {tools}\n\
         Allowed checks and required arguments:\n{checks}\n\n\
         Output contract (relative to the workspace): {output}\n\n\
         Reply with exactly one JSON object of the form:\n\
         {{\"steps\":[{{\"index\":1,\"title\":\"short title\",\"criterion\":\"how completion is observed\",\
         \"tool\":\"one allowed tool or null\",\"checks\":[{{\"kind\":\"allowed check\", ...required arguments}}]}}]}}\n\
         Every step has exactly the keys index, title, criterion, tool, checks. Every check has \
         `kind` plus only the arguments listed for that kind: no extra keys, no empty arrays, \
         and no null placeholders.\n\
         Rules: between 1 and {max_steps} steps; indexes start at 1 and are contiguous; \
         every step has at least one allowed check with valid arguments; every criterion is \
         observable and at least 10 characters; every step should leave a verifiable artifact \
         in the workspace. Prefer the fewest steps: read each verified input once, then write \
         each required output. Include a step whose tool is write_file or edit_file that \
         creates every required output file; a plan that only reads does not satisfy the \
         contract. No prose. No code fences.",
        description = mission.description,
        max_steps = mission.limits.max_steps,
    )
}

/// The concrete brief: goal, verified inputs, and real target paths.
fn user_brief(request: &PlanRequest) -> String {
    let inputs = if request.inputs.is_empty() {
        "(none)".to_owned()
    } else {
        request
            .inputs
            .iter()
            .map(|(name, detail)| format!("- {name}: {detail}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let artifacts = if request.mission.output.required_artifacts.is_empty() {
        "(no required files)".to_owned()
    } else {
        request
            .mission
            .output
            .required_artifacts
            .iter()
            .map(|artifact| {
                format!(
                    "- {} ({})",
                    artifact.path,
                    if artifact.non_empty {
                        "non-empty"
                    } else {
                        "may be empty"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Goal: {goal}\n\
         Verified inputs:\n{inputs}\n\
         Use only workspace-relative paths in steps; never write an absolute path.\n\
         Required output files:\n{artifacts}\n\
         Create every required output file with a write_file step. That step's title and \
         criterion must describe creating the output, and at least one of its checks must \
         name the output file's path.\n\
         Mission JSON name: {name}",
        goal = request.goal,
        name = request.mission.name,
    )
}

fn check_help(kind: &str) -> &'static str {
    match kind {
        "file_exists" => "`path`: relative file path that must exist",
        "dir_exists" => "`path`: relative directory path that must exist",
        "file_changed" => {
            "`path`: file whose content must differ from before the step; use only in a step whose tool is write_file or edit_file"
        }
        "content_matches" => {
            "`path` plus `contains` (one string or an array of required strings), `all_of`, or `regex`"
        }
        "content_absent" => "`path` plus `forbidden` (array of strings that must not appear)",
        "json_schema" => "`path` plus `schema` (JSON Schema object)",
        "numeric_consistency" => "`path`, `total`, and `sum` (array of numeric field names)",
        "report_complete" => "`fields`: array of required report fields",
        "transcript_nonempty" => "optional `min_words` (positive integer)",
        "command_exit" | "tests_pass" => "`template`: ID of a mission command template",
        "stdout_contains" => "`contains` or `regex`",
        "vl_criterion" => "`criterion`: visual statement the vision model must confirm",
        "d1_noul" => "`criterion`: decision statement with no unlisted assumptions",
        "d1_choice" => "`criterion`, `options` (object), and `expect`",
        "d1_grounded" => "`fields`: object mapping output fields to evidence",
        _ => "arguments depend on the check kind",
    }
}

/// Convenience for callers that build a plan request from a manifest.
#[must_use]
pub fn plan_request(
    mission: MissionManifest,
    goal: impl Into<String>,
    inputs: BTreeMap<String, String>,
    workspace: impl Into<PathBuf>,
) -> PlanRequest {
    PlanRequest {
        mission,
        goal: goal.into(),
        inputs,
        workspace: workspace.into(),
    }
}
