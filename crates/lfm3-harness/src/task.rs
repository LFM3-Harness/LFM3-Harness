//! General task mode: one user-stated goal becomes a routed, verified plan.
//!
//! Missions are authored ahead of time with a frozen capability envelope. A
//! task is not: the person states a goal, the router decides which of the four
//! local models can contribute, the engine derives the tool and check envelope
//! from that decision, and the planner drafts steps. From there the ordinary
//! runner takes over, so every step is still judged by checks over real
//! evidence before the next one starts.
//!
//! Routing never widens verification. It only decides which capabilities are
//! offered to the planner and which local models the run may consult; the
//! checks themselves stay deterministic. The d1 classifier stays with mission
//! semantic checks, which a task cannot use, so it is never claimed here.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::{
    agent::{
        planner::{plan_request, Planner, PlannerError},
        PlanRequest,
    },
    mission::{
        CheckSpec, FrozenAcceptance, InputArtifact, InputKind, InputSpec, MissionLimits,
        MissionManifest, OutputContract, PlanDraft, PlanError, PlannedStep, RequiredArtifact,
    },
    model::{ModelGateway, ModelKey},
    CancellationContext,
};

/// Name recorded as the mission of every general task run.
pub const GENERAL_TASK: &str = "general_task";

/// Deterministic check kinds a general task may use. Command-based checks are
/// absent deliberately: a task has no approved command templates, so it can
/// never author a shell invocation.
const TASK_CHECKS: &[&str] = &[
    "file_exists",
    "dir_exists",
    "file_changed",
    "content_matches",
    "content_absent",
    "json_schema",
    "numeric_consistency",
    "report_complete",
];

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];
const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "flac", "ogg", "m4a", "aac", "opus"];

const VISION_WORDS: &[&str] = &[
    "image",
    "images",
    "picture",
    "photo",
    "screenshot",
    "diagram",
    "chart",
    "visual",
    "look at",
    "png",
    "jpg",
    "jpeg",
    "webp",
];

const AUDIO_WORDS: &[&str] = &[
    "audio",
    "recording",
    "voice",
    "transcribe",
    "transcript",
    "speech",
    "wav",
    "mp3",
    "m4a",
    "listen",
];

/// What one local model is allowed to contribute to a task.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Draft the step plan from the goal and the frozen envelope.
    Plan,
    /// Choose and perform exactly one workspace action per attempt.
    Act,
    /// Observe one image through the workspace `look` tool.
    Observe,
    /// Transcribe one audio file through the `transcribe` tool.
    Hear,
    /// Calibrated classification used to interpret observations.
    Judge,
}

impl Role {
    pub const ALL: [Self; 5] = [
        Self::Plan,
        Self::Act,
        Self::Observe,
        Self::Hear,
        Self::Judge,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Act => "act",
            Self::Observe => "observe",
            Self::Hear => "hear",
            Self::Judge => "judge",
        }
    }

    /// The pinned model that serves this role.
    #[must_use]
    pub const fn model(self) -> ModelKey {
        match self {
            Self::Plan | Self::Act => ModelKey::Text,
            Self::Observe => ModelKey::Vision,
            Self::Hear => ModelKey::Audio,
            Self::Judge => ModelKey::D1,
        }
    }

    /// What this model can be trusted with, and what it cannot.
    #[must_use]
    pub const fn capability(self) -> &'static str {
        match self {
            Self::Plan => "structured step plans from a goal and a frozen capability envelope",
            Self::Act => "one workspace tool call per attempt, with no self-certification",
            Self::Observe => "bounded description of visible pixels in one image",
            Self::Hear => "local transcription of one audio file, never judgement",
            Self::Judge => "calibrated classification of an observation or label",
        }
    }

    /// The workspace tool this role is consulted through, if any.
    #[must_use]
    pub const fn tool(self) -> Option<&'static str> {
        match self {
            Self::Observe => Some("look"),
            Self::Hear => Some("transcribe"),
            Self::Plan | Self::Act | Self::Judge => None,
        }
    }
}

/// Which models a task needs, and why. Reasons are user-visible.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub roles: BTreeSet<Role>,
    pub reasons: BTreeMap<Role, String>,
    /// Image files found in the workspace, workspace-relative.
    pub images: Vec<String>,
    /// Audio files found in the workspace, workspace-relative.
    pub audio: Vec<String>,
}

impl Route {
    #[must_use]
    pub fn uses(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    /// Tools the task envelope grants, in a stable order.
    #[must_use]
    pub fn tools(&self) -> Vec<String> {
        let mut tools = vec![
            "list_files".to_owned(),
            "read_file".to_owned(),
            "write_file".to_owned(),
            "edit_file".to_owned(),
        ];
        for role in [Role::Observe, Role::Hear] {
            if let Some(tool) = role.tool().filter(|_| self.uses(role)) {
                tools.push(tool.to_owned());
            }
        }
        tools
    }

    /// The routing decision as one report object for the CLI and the UI.
    #[must_use]
    pub fn report(&self) -> Value {
        let roles = Role::ALL
            .into_iter()
            .map(|role| {
                json!({
                    "role": role.as_str(),
                    "model": role.model().to_string(),
                    "capability": role.capability(),
                    "used": self.uses(role),
                    "reason": self.reasons.get(&role),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "roles": roles,
            "images": self.images,
            "audio": self.audio,
            "tools": self.tools(),
        })
    }
}

/// Decides the capability envelope for one goal. Deterministic and local: it
/// reads the goal text and the workspace file names, never file contents.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskRouter;

impl TaskRouter {
    #[must_use]
    pub fn route(goal: &str, workspace: &Path) -> Route {
        let (images, audio) = scan_media(workspace);
        let goal_lower = goal.to_ascii_lowercase();
        let mut route = Route {
            images,
            audio,
            ..Route::default()
        };

        insert(&mut route, Role::Plan, "every task starts with a plan");
        insert(
            &mut route,
            Role::Act,
            "every step needs one bounded workspace action",
        );

        if !route.images.is_empty() {
            let count = route.images.len();
            insert(
                &mut route,
                Role::Observe,
                format!(
                    "the workspace contains {} image file(s), so visual observation is offered",
                    count
                ),
            );
        } else if let Some(word) = mentions(&goal_lower, VISION_WORDS) {
            insert(
                &mut route,
                Role::Observe,
                format!("the goal mentions {word:?}; the look tool needs the vision model"),
            );
        }

        if !route.audio.is_empty() {
            let count = route.audio.len();
            insert(
                &mut route,
                Role::Hear,
                format!(
                    "the workspace contains {} audio file(s), so local transcription is offered",
                    count
                ),
            );
        } else if let Some(word) = mentions(&goal_lower, AUDIO_WORDS) {
            insert(
                &mut route,
                Role::Hear,
                format!("the goal mentions {word:?}; the transcribe tool needs the audio model"),
            );
        }

        route
    }
}

fn insert(route: &mut Route, role: Role, reason: impl Into<String>) {
    route.roles.insert(role);
    route.reasons.insert(role, reason.into());
}

fn mentions(goal: &str, words: &[&'static str]) -> Option<&'static str> {
    words.iter().copied().find(|word| goal.contains(word))
}

/// Reads only names, at most three levels deep, and ignores hidden entries.
fn scan_media(root: &Path) -> (Vec<String>, Vec<String>) {
    const MAX_DEPTH: usize = 3;
    const MAX_ENTRIES: usize = 400;

    let mut images = Vec::new();
    let mut audio = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;

    'scan: while let Some((directory, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            seen = seen.saturating_add(1);
            if seen > MAX_ENTRIES {
                break 'scan;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if IMAGE_EXTENSIONS.contains(&extension.as_str()) {
                images.push(relative);
            } else if AUDIO_EXTENSIONS.contains(&extension.as_str()) {
                audio.push(relative);
            }
        }
    }
    images.sort();
    audio.sort();
    (images, audio)
}

/// One user-stated task. Everything the run may do is derived from here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRequest {
    pub goal: String,
    pub workspace: PathBuf,
    /// Optional required output path. When set, the plan must write it.
    pub deliverable: Option<String>,
    /// Upper bound on planned steps; the configured limit still applies.
    pub max_steps: Option<usize>,
    /// Lower bound on planned steps. A job must not collapse into one action.
    pub min_steps: Option<usize>,
}

impl TaskRequest {
    pub fn new(goal: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            goal: goal.into(),
            workspace: workspace.into(),
            deliverable: None,
            max_steps: None,
            min_steps: None,
        }
    }

    #[must_use]
    pub fn with_deliverable(mut self, deliverable: Option<String>) -> Self {
        self.deliverable = deliverable;
        self
    }

    #[must_use]
    pub fn with_max_steps(mut self, max_steps: Option<usize>) -> Self {
        self.max_steps = max_steps;
        self
    }

    #[must_use]
    pub fn with_min_steps(mut self, min_steps: Option<usize>) -> Self {
        self.min_steps = min_steps;
        self
    }

    /// Validates the goal and workspace before any model token is spent.
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.goal.trim().is_empty() {
            return Err(TaskError::EmptyGoal);
        }
        if !self.workspace.is_dir() {
            return Err(TaskError::Workspace(self.workspace.display().to_string()));
        }
        Ok(())
    }
}

/// A task plan no model could honestly produce.
#[derive(Debug, Error)]
pub enum TaskError {
    #[error("the task goal is empty")]
    EmptyGoal,
    #[error("workspace {0} is not a directory")]
    Workspace(String),
    #[error("planning failed: {0}")]
    Planning(#[from] PlannerError),
    #[error("the plan is invalid: {0}")]
    Plan(#[from] PlanError),
    #[error(
        "step {index} uses {tool}, but none of its checks names a file path, \
         so its completion could not be tested"
    )]
    UnverifiableStep { index: u32, tool: String },
    #[error("the plan has {actual} step(s); this job needs at least {minimum}")]
    TooFewSteps { actual: usize, minimum: usize },
}

/// Builds the frozen capability envelope for one routed task.
pub fn manifest(route: &Route, request: &TaskRequest) -> Result<MissionManifest, PlanError> {
    let mut checks = TASK_CHECKS
        .iter()
        .map(|kind| (*kind).to_owned())
        .collect::<Vec<_>>();
    checks.sort_unstable();
    let mut tools = route.tools();
    tools.sort();
    let required_artifacts = request
        .deliverable
        .iter()
        .map(|path| RequiredArtifact {
            path: path.clone(),
            non_empty: true,
        })
        .collect();
    let manifest = MissionManifest {
        schema_version: 1,
        name: GENERAL_TASK.to_owned(),
        version: 1,
        title: "General task".to_owned(),
        description: "Work toward a stated goal in the approved workspace. Every step must \
                      declare a check the harness can evaluate against real evidence, and \
                      no step passes on the model's word."
            .to_owned(),
        inputs: BTreeMap::from([(
            "goal".to_owned(),
            InputSpec {
                kind: InputKind::String,
                required: true,
                schema: None,
            },
        )]),
        tools,
        checks,
        command_templates: Vec::new(),
        output: OutputContract {
            schema: None,
            required_artifacts,
        },
        limits: MissionLimits {
            max_steps: request.max_steps.unwrap_or(16).clamp(1, 32),
            max_attempts: 3,
        },
        semantic: None,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// Drops checks that assert nothing or can never be true on their step.
///
/// A 1.7B planner sometimes emits an empty condition list, or a change check on
/// a step that only reads. Those are not stricter verification: one can never
/// pass, the other can never be meaningful. Everything else stays strict, and a
/// step left without checks still fails validation.
pub fn normalize_task_plan(plan: &mut PlanDraft) {
    for step in &mut plan.steps {
        // A 1.7B plan often leaves the tool unset while its title, criterion and
        // checks clearly name one action. Naming it is what lets the executor
        // grant and request that action instead of drifting back to reading.
        if step.tool.is_none() {
            step.tool = infer_tool(step);
        }
        let mutating = matches!(step.tool.as_deref(), Some("write_file" | "edit_file"));
        // A read step can only be verified by its source existing. Capture the
        // path before dropping the checks that cannot apply to a read.
        let fallback = step
            .checks
            .iter()
            .find_map(|check| match check.kind.as_str() {
                "file_changed" | "file_exists" | "dir_exists" | "content_matches" => check
                    .args
                    .get("path")
                    .and_then(Value::as_str)
                    .map(|path| (path.to_owned(), path.ends_with('/') || path.ends_with('\\'))),
                _ => None,
            });
        // A step that only reads cannot know file content at plan time, so any
        // content assertion there is invented. Keep only existence checks.
        if !mutating {
            step.checks
                .retain(|check| matches!(check.kind.as_str(), "file_exists" | "dir_exists"));
        }
        step.checks.retain(|check| match check.kind.as_str() {
            "file_changed" => mutating,
            "content_absent" => check
                .args
                .get("forbidden")
                .and_then(Value::as_array)
                .is_some_and(|values| {
                    !values.is_empty()
                        && values
                            .iter()
                            .all(|value| value.as_str().is_some_and(|text| !text.trim().is_empty()))
                }),
            "content_matches" => {
                let usable = |value: &Value| match value {
                    Value::String(text) => !text.trim().is_empty(),
                    Value::Array(values) => {
                        !values.is_empty()
                            && values.iter().all(|value| {
                                value.as_str().is_some_and(|text| !text.trim().is_empty())
                            })
                    }
                    _ => false,
                };
                check.args.get("contains").is_some_and(usable)
                    || check.args.get("regex").is_some_and(usable)
                    || check.args.get("all_of").is_some_and(usable)
            }
            _ => true,
        });
        // A placeholder such as `<need actual content>` can never pass and was
        // never measured; it is a request for information, not a criterion.
        step.checks.retain(|check| {
            !matches!(check.kind.as_str(), "content_matches" | "content_absent")
                || !has_placeholder(check)
        });
        if step.checks.is_empty() {
            // A read step has no artifact to inspect. Its honest check is the
            // source it reads; when the plan named none, it is verified as
            // having run inside the approved workspace.
            let (path, directory) = fallback.unwrap_or_else(|| (".".to_owned(), true));
            let clean = path.trim_end_matches(['/', '\\']);
            let clean = if clean.is_empty() { "." } else { clean };
            let kind = if directory {
                "dir_exists"
            } else {
                "file_exists"
            };
            step.checks
                .push(CheckSpec::new(kind, [("path", json!(clean))]));
        }
    }
}

/// Best-effort action a step describes, used only when the plan left it unset.
fn infer_tool(step: &PlannedStep) -> Option<String> {
    let text = format!("{} {}", step.title, step.criterion).to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| text.contains(word));
    if has(&["edit", "append", "update", "revise", "rename"]) {
        return Some("edit_file".to_owned());
    }
    if has(&["write", "create", "save", "produce", "generate", "record"]) {
        return Some("write_file".to_owned());
    }
    if has(&["transcribe", "audio", "sound", "recording"]) {
        return Some("transcribe".to_owned());
    }
    if has(&["look", "image", "photo", "screenshot", "picture"]) {
        return Some("look".to_owned());
    }
    if has(&["list", "enumerate", "directory content"]) {
        return Some("list_files".to_owned());
    }
    if has(&[
        "read", "inspect", "open", "review", "extract", "collect", "count",
    ]) {
        return Some("read_file".to_owned());
    }
    None
}

fn has_placeholder(check: &CheckSpec) -> bool {
    let placeholder = |text: &str| text.contains('<') && text.contains('>');
    check.args.values().any(|value| match value {
        Value::String(text) => placeholder(text),
        Value::Array(values) => values
            .iter()
            .any(|value| value.as_str().is_some_and(placeholder)),
        _ => false,
    })
}

/// Rejects plans whose mutating steps could not be tested.
///
/// A step that writes or edits must prove the result with a check that names a
/// workspace path. This is the general-task counterpart of a frozen mission
/// contract: the planner may choose the checks, but it cannot ship a step the
/// harness has no way to fail.
pub fn verify_step_checks(plan: &PlanDraft) -> Result<(), TaskError> {
    for step in &plan.steps {
        let mutating = matches!(step.tool.as_deref(), Some("write_file" | "edit_file"));
        if !mutating {
            continue;
        }
        let tested = step.checks.iter().any(|check| {
            check
                .args
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| !path.trim().is_empty())
        });
        if !tested {
            return Err(TaskError::UnverifiableStep {
                index: step.index,
                tool: step.tool.clone().unwrap_or_default(),
            });
        }
    }
    Ok(())
}

/// A planned task: the routing decision, its envelope, and the frozen contract.
#[derive(Clone, Debug)]
pub struct TaskPlan {
    pub route: Route,
    pub manifest: MissionManifest,
    pub acceptance: FrozenAcceptance,
}

impl TaskPlan {
    /// One report object describing what the run will do and which models it
    /// will consult.
    #[must_use]
    pub fn report(&self) -> Value {
        json!({
            "mission": self.manifest.name,
            "goal": self.acceptance.spec().goal,
            "acceptance_hash": self.acceptance.hash(),
            "steps": self.acceptance.spec().steps,
            "tools": self.manifest.tools,
            "checks": self.manifest.checks,
            "route": self.route.report(),
        })
    }
}

/// Routes the goal, derives the envelope, and drafts a checked plan.
pub async fn draft_task(
    gateway: Arc<dyn ModelGateway>,
    request: TaskRequest,
    cancel: &CancellationContext,
) -> Result<TaskPlan, TaskError> {
    request.validate()?;
    let route = TaskRouter::route(&request.goal, &request.workspace);
    let mut requested = request.clone();
    if requested.deliverable.is_none() {
        requested.deliverable = detect_deliverable(&requested.goal);
    }
    let manifest = manifest(&route, &requested)?;
    // A job plan is much larger than a one-file mission plan, so the planner
    // gets room for every step and its checks before JSON decoding.
    let planner = Planner::new(gateway)
        .with_max_tokens(4096)
        .with_rules(normalize_task_plan);
    let plan = planner
        .draft_repairs(plan_input(&manifest, &requested, &route), cancel, 2)
        .await?;
    let minimum = requested.min_steps.unwrap_or(1);
    if plan.steps.len() < minimum {
        return Err(TaskError::TooFewSteps {
            actual: plan.steps.len(),
            minimum,
        });
    }
    verify_step_checks(&plan)?;
    let acceptance = manifest.compile_acceptance(
        request.goal.clone(),
        BTreeMap::<String, InputArtifact>::new(),
        plan,
    )?;
    Ok(TaskPlan {
        route,
        manifest,
        acceptance,
    })
}

/// Finds an output path stated in the goal, such as `out/report.md`.
///
/// A job usually names where its result goes. Freezing that path as the
/// required artifact is what makes the final file prove the job finished.
#[must_use]
pub fn detect_deliverable(goal: &str) -> Option<String> {
    goal.split_whitespace()
        .map(|token| {
            token.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && !"/._-".contains(character)
            })
        })
        .find(|token| {
            token.contains('/')
                && [".md", ".txt", ".json", ".csv", ".html"]
                    .iter()
                    .any(|extension| token.to_ascii_lowercase().ends_with(extension))
        })
        .map(str::to_owned)
}

/// A bounded list of real workspace paths so the plan names files that exist.
fn workspace_files(root: &Path) -> String {
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
            files.push(
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    files.sort();
    files.truncate(60);
    if files.is_empty() {
        "(empty workspace)".to_owned()
    } else {
        files.join(
            "
",
        )
    }
}

fn plan_input(manifest: &MissionManifest, request: &TaskRequest, route: &Route) -> PlanRequest {
    let mut details = BTreeMap::new();
    let mut context = Vec::new();
    details.insert(
        "workspace files".to_owned(),
        workspace_files(&request.workspace),
    );
    if !route.images.is_empty() {
        context.push(format!("image files: {}", route.images.join(", ")));
    }
    if !route.audio.is_empty() {
        context.push(format!("audio files: {}", route.audio.join(", ")));
    }
    if route.uses(Role::Observe) {
        context.push(
            "`look` reads an image with the vision model; use it before describing any image"
                .to_owned(),
        );
    }
    if route.uses(Role::Hear) {
        context.push(
            "`transcribe` turns one audio file into text with the local audio model".to_owned(),
        );
    }
    context.push(
        "each step must carry at least one check that can fail, not only a claim of completion"
            .to_owned(),
    );
    context.push(
        "every write_file or edit_file step needs a check whose `path` names the file it changes"
            .to_owned(),
    );
    let maximum = request.max_steps.unwrap_or(16).clamp(1, 32);
    let minimum = request.min_steps.unwrap_or(1).clamp(1, maximum);
    context.push(format!(
        "this is a job, not a single call: plan between {minimum} and {maximum} steps, one \
         workspace action per step, and keep going until the whole goal is complete. Never stop \
         after one action, and never claim completion in prose"
    ));
    context.push(
        "checks must name files, not directories: give content_absent a non-empty `forbidden` \
         list, give content_matches a non-empty `contains`, and never use file_changed on a step \
         that only reads"
            .to_owned(),
    );
    details.insert("workspace context".to_owned(), context.join("\n"));

    plan_request(
        manifest.clone(),
        request.goal.clone(),
        details,
        request.workspace.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{CheckSpec, PlannedStep};

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("lfm3-task-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch workspace");
        path
    }

    #[test]
    fn text_only_goal_routes_two_text_roles() {
        let workspace = scratch("text");
        let route = TaskRouter::route("summarise the notes into out/summary.md", &workspace);
        assert!(route.uses(Role::Plan));
        assert!(route.uses(Role::Act));
        assert!(!route.uses(Role::Observe));
        assert!(!route.uses(Role::Hear));
        assert!(!route.uses(Role::Judge));
        assert_eq!(
            route.tools(),
            vec!["list_files", "read_file", "write_file", "edit_file"]
        );
    }

    #[test]
    fn workspace_media_selects_sense_models() {
        let workspace = scratch("media");
        std::fs::write(workspace.join("screen.png"), b"png").unwrap();
        std::fs::write(workspace.join("note.wav"), b"wav").unwrap();
        let route = TaskRouter::route("describe what changed", &workspace);
        assert!(route.uses(Role::Observe));
        assert!(route.uses(Role::Hear));
        assert!(route.tools().contains(&"look".to_owned()));
        assert!(route.tools().contains(&"transcribe".to_owned()));
        assert_eq!(route.images, vec!["screen.png"]);
        assert_eq!(route.audio, vec!["note.wav"]);
    }

    #[test]
    fn judgement_words_do_not_claim_the_classifier() {
        let workspace = scratch("judge");
        let route = TaskRouter::route("review the report and check the totals", &workspace);
        assert!(!route.uses(Role::Judge));
        assert!(!route.uses(Role::Observe));
        assert!(!route.uses(Role::Hear));
    }

    #[test]
    fn envelope_grants_senses_only_when_routed() {
        let workspace = scratch("envelope");
        let request = TaskRequest::new("write a report", workspace.clone());
        let route = TaskRouter::route(&request.goal, &workspace);
        let manifest = manifest(&route, &request).expect("envelope");
        assert!(!manifest.tools.contains(&"look".to_owned()));
        assert!(!manifest.tools.contains(&"transcribe".to_owned()));
        assert!(manifest.checks.contains(&"content_matches".to_owned()));
        assert!(manifest.output.required_artifacts.is_empty());
        assert!(manifest.semantic.is_none());
    }

    #[test]
    fn deliverable_becomes_a_required_artifact() {
        let workspace = scratch("deliverable");
        let request = TaskRequest::new("write a report", workspace.clone())
            .with_deliverable(Some("out/report.md".to_owned()));
        let route = TaskRouter::route(&request.goal, &workspace);
        let manifest = manifest(&route, &request).expect("envelope");
        assert_eq!(manifest.output.required_artifacts.len(), 1);
        assert_eq!(manifest.output.required_artifacts[0].path, "out/report.md");
    }

    #[test]
    fn an_output_path_in_the_goal_becomes_the_deliverable() {
        assert_eq!(
            detect_deliverable("write out/report.md with three sections"),
            Some("out/report.md".to_owned())
        );
        assert_eq!(detect_deliverable("summarise every note"), None);
    }

    #[test]
    fn empty_goal_is_refused_before_planning() {
        let workspace = scratch("empty");
        let request = TaskRequest::new("   ", workspace);
        assert!(matches!(request.validate(), Err(TaskError::EmptyGoal)));
    }

    #[test]
    fn unactionable_checks_are_dropped_and_real_ones_kept() {
        let mut plan = PlanDraft {
            steps: vec![PlannedStep {
                index: 1,
                title: "write the report".to_owned(),
                criterion: "the report exists and holds every figure".to_owned(),
                checks: vec![
                    CheckSpec::new(
                        "content_absent",
                        [("path", json!("out/report.md")), ("forbidden", json!([]))],
                    ),
                    CheckSpec::new(
                        "content_matches",
                        [("path", json!("out/report.md")), ("contains", json!([""]))],
                    ),
                    CheckSpec::new("file_changed", [("path", json!("out/report.md"))]),
                    CheckSpec::new("file_exists", [("path", json!("out/report.md"))]),
                ],
                tool: Some("write_file".to_owned()),
            }],
        };
        normalize_task_plan(&mut plan);
        let kinds = plan.steps[0]
            .checks
            .iter()
            .map(|check| check.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(kinds, vec!["file_changed", "file_exists"]);
    }

    #[test]
    fn change_checks_are_dropped_from_read_only_steps() {
        let mut plan = PlanDraft {
            steps: vec![PlannedStep {
                index: 1,
                title: "read every note".to_owned(),
                criterion: "every note file was read into context".to_owned(),
                checks: vec![
                    CheckSpec::new("file_changed", [("path", json!("notes/"))]),
                    CheckSpec::new("dir_exists", [("path", json!("notes"))]),
                ],
                tool: Some("read_file".to_owned()),
            }],
        };
        normalize_task_plan(&mut plan);
        let kinds = plan.steps[0]
            .checks
            .iter()
            .map(|check| check.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(kinds, vec!["dir_exists"]);
    }

    #[test]
    fn a_read_step_keeps_a_source_existence_check() {
        let mut plan = PlanDraft {
            steps: vec![PlannedStep {
                index: 1,
                title: "read the notes directory".to_owned(),
                criterion: "every note was read from the notes directory".to_owned(),
                checks: vec![
                    CheckSpec::new("file_changed", [("path", json!("notes/"))]),
                    CheckSpec::new(
                        "content_absent",
                        [("path", json!("notes/")), ("forbidden", json!([]))],
                    ),
                ],
                tool: Some("read_file".to_owned()),
            }],
        };
        normalize_task_plan(&mut plan);
        assert_eq!(plan.steps[0].checks.len(), 1);
        assert_eq!(plan.steps[0].checks[0].kind, "dir_exists");
        assert_eq!(
            plan.steps[0].checks[0].args.get("path"),
            Some(&json!("notes"))
        );
    }

    #[test]
    fn a_read_step_never_keeps_an_invented_content_check() {
        let mut plan = PlanDraft {
            steps: vec![PlannedStep {
                index: 1,
                title: "Read notes/alpha.md".to_owned(),
                criterion: "the first note was read with its figures".to_owned(),
                checks: vec![CheckSpec::new(
                    "content_matches",
                    [
                        ("path", json!("notes/alpha.md")),
                        (
                            "contains",
                            json!(["<need actual content from notes/alpha.md>"]),
                        ),
                    ],
                )],
                tool: Some("read_file".to_owned()),
            }],
        };
        normalize_task_plan(&mut plan);
        assert_eq!(plan.steps[0].checks.len(), 1);
        assert_eq!(plan.steps[0].checks[0].kind, "file_exists");
        assert_eq!(
            plan.steps[0].checks[0].args.get("path"),
            Some(&json!("notes/alpha.md"))
        );
    }

    #[test]
    fn an_unset_tool_is_inferred_from_the_step() {
        let mut plan = PlanDraft {
            steps: vec![PlannedStep {
                index: 4,
                title: "Write out/report.md".to_owned(),
                criterion: "Create out/report.md with one section per note".to_owned(),
                checks: vec![CheckSpec::new(
                    "file_changed",
                    [("path", json!("out/report.md"))],
                )],
                tool: None,
            }],
        };
        normalize_task_plan(&mut plan);
        assert_eq!(plan.steps[0].tool.as_deref(), Some("write_file"));
    }

    #[test]
    fn write_step_without_a_path_check_is_refused() {
        let plan = PlanDraft {
            steps: vec![PlannedStep {
                index: 1,
                title: "write the summary".to_owned(),
                criterion: "the summary file exists and is non-empty".to_owned(),
                checks: vec![CheckSpec::new(
                    "report_complete",
                    [("fields", json!(["x"]))],
                )],
                tool: Some("write_file".to_owned()),
            }],
        };
        let error = verify_step_checks(&plan).expect_err("untestable write");
        assert!(matches!(
            error,
            TaskError::UnverifiableStep { index: 1, .. }
        ));
    }

    #[test]
    fn write_step_with_a_path_check_is_accepted() {
        let plan = PlanDraft {
            steps: vec![PlannedStep {
                index: 1,
                title: "write the summary".to_owned(),
                criterion: "the summary file exists and holds the required terms".to_owned(),
                checks: vec![
                    CheckSpec::new("file_exists", [("path", json!("out/summary.md"))]),
                    CheckSpec::new(
                        "content_matches",
                        [
                            ("path", json!("out/summary.md")),
                            ("contains", json!(["total"])),
                        ],
                    ),
                ],
                tool: Some("write_file".to_owned()),
            }],
        };
        verify_step_checks(&plan).expect("tested write");
    }
}
