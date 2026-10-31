use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mission::{
    acceptance::validate_relative_path, AcceptanceSpec, CheckSpec, ExecutionStrategy,
    FrozenAcceptance, InputArtifact, OutputContract, PlanDraft, PlanError,
};
use crate::verify::{is_semantic_kind, SemanticPolicy};

const KNOWN_TOOLS: &[&str] = &[
    "list_files",
    "read_file",
    "write_file",
    "edit_file",
    "search_files",
    "related_files",
    "look",
    "transcribe",
    "speak",
];

const KNOWN_CHECKS: &[&str] = &[
    "file_exists",
    "dir_exists",
    "file_changed",
    "content_matches",
    "content_absent",
    "json_schema",
    "numeric_consistency",
    "report_complete",
    "transcript_nonempty",
    "command_exit",
    "tests_pass",
    "stdout_contains",
    "vl_criterion",
    "d1_noul",
    "d1_choice",
    "d1_grounded",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    String,
    Image,
    Audio,
    Json,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    pub kind: InputKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub schema: Option<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTemplateDef {
    pub id: String,
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionLimits {
    pub max_steps: usize,
    pub max_attempts: usize,
}

impl Default for MissionLimits {
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_attempts: 3,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionManifest {
    pub schema_version: u32,
    pub name: String,
    pub version: u32,
    pub title: String,
    pub description: String,
    pub inputs: BTreeMap<String, InputSpec>,
    pub tools: Vec<String>,
    pub checks: Vec<String>,
    #[serde(default)]
    pub command_templates: Vec<CommandTemplateDef>,
    pub output: OutputContract,
    pub limits: MissionLimits,
    /// Semantic judging capability. Required whenever `checks` names a
    /// semantic kind; an `automatic` policy additionally needs a frozen
    /// holdout qualification.
    #[serde(default)]
    pub semantic: Option<SemanticPolicy>,
}

impl MissionManifest {
    pub fn from_json(raw: &str) -> Result<Self, PlanError> {
        let manifest: Self =
            serde_json::from_str(raw).map_err(|error| PlanError::Json(error.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), PlanError> {
        if self.schema_version != 1 {
            return Err(PlanError::InvalidManifest(format!(
                "schema_version must be 1, got {}",
                self.schema_version
            )));
        }
        if !valid_name(&self.name) {
            return Err(PlanError::InvalidManifest(
                "name must use lowercase letters, digits, and underscores".to_owned(),
            ));
        }
        if self.version == 0 {
            return Err(PlanError::InvalidManifest(
                "version must be at least 1".to_owned(),
            ));
        }
        if self.title.trim().is_empty() || self.description.trim().is_empty() {
            return Err(PlanError::InvalidManifest(
                "title and description must be non-empty".to_owned(),
            ));
        }
        if self.inputs.is_empty() {
            return Err(PlanError::InvalidManifest(
                "a mission needs at least one input".to_owned(),
            ));
        }
        for (name, input) in &self.inputs {
            if name.trim().is_empty() {
                return Err(PlanError::InvalidManifest(
                    "input names must be non-empty".to_owned(),
                ));
            }
            if input.kind == InputKind::Json && input.schema.is_none() {
                return Err(PlanError::InvalidManifest(format!(
                    "JSON input {name:?} needs a schema"
                )));
            }
        }
        validate_unique(&self.tools, "tool")?;
        validate_unique(&self.checks, "check")?;
        for tool in &self.tools {
            if !KNOWN_TOOLS.contains(&tool.as_str()) {
                return Err(PlanError::InvalidManifest(format!(
                    "tool {tool:?} is not implemented by this engine"
                )));
            }
        }
        for check in &self.checks {
            if !KNOWN_CHECKS.contains(&check.as_str()) {
                return Err(PlanError::InvalidManifest(format!(
                    "check {check:?} is not implemented by this engine"
                )));
            }
        }
        let uses_semantic = self.checks.iter().any(|check| is_semantic_kind(check));
        match (&self.semantic, uses_semantic) {
            (None, true) => {
                return Err(PlanError::InvalidManifest(format!(
                    "mission {:?} declares semantic checks but has no semantic policy",
                    self.name
                )));
            }
            (Some(policy), _) => policy.validate().map_err(PlanError::InvalidManifest)?,
            (None, false) => {}
        }
        if !(1..=32).contains(&self.limits.max_steps) {
            return Err(PlanError::InvalidManifest(
                "max_steps must be between 1 and 32".to_owned(),
            ));
        }
        if !(1..=8).contains(&self.limits.max_attempts) {
            return Err(PlanError::InvalidManifest(
                "max_attempts must be between 1 and 8".to_owned(),
            ));
        }
        let mut templates = BTreeSet::new();
        for template in &self.command_templates {
            if template.id.trim().is_empty()
                || template.executable.trim().is_empty()
                || !templates.insert(template.id.as_str())
            {
                return Err(PlanError::InvalidManifest(
                    "command templates need unique IDs and fixed executables".to_owned(),
                ));
            }
        }
        for artifact in &self.output.required_artifacts {
            validate_relative_path(&artifact.path)?;
        }
        Ok(())
    }

    /// Validates a model plan without changing any acceptance decisions.
    pub fn validate_plan(&self, plan: &PlanDraft) -> Result<(), PlanError> {
        if plan.steps.is_empty() {
            return Err(PlanError::NoSteps);
        }
        if plan.steps.len() > self.limits.max_steps {
            return Err(PlanError::TooManySteps {
                actual: plan.steps.len(),
                maximum: self.limits.max_steps,
            });
        }

        let mut indexes = BTreeSet::new();
        for step in &plan.steps {
            if step.index == 0 || !indexes.insert(step.index) {
                return Err(PlanError::InvalidStepIndex { index: step.index });
            }
            if step.criterion.trim().chars().count() < 10 {
                return Err(PlanError::VagueCriterion { index: step.index });
            }
            if step.checks.is_empty() {
                return Err(PlanError::NoChecks { index: step.index });
            }
            if let Some(tool) = &step.tool {
                if !self.tools.contains(tool) {
                    return Err(PlanError::UnknownTool {
                        index: step.index,
                        tool: tool.clone(),
                    });
                }
            }
            for check in &step.checks {
                if !self.checks.contains(&check.kind) {
                    return Err(PlanError::UnknownCheck {
                        index: step.index,
                        kind: check.kind.clone(),
                    });
                }
                validate_check_args(check).map_err(|message| PlanError::InvalidCheck {
                    index: step.index,
                    message,
                })?;
                if is_semantic_kind(&check.kind) {
                    self.validate_semantic_args(step.index, check)?;
                }
            }
            // A read-only step can never change a file, so a `file_changed`
            // check there is an impossible plan rather than a strict one.
            let mutating = matches!(step.tool.as_deref(), Some("write_file" | "edit_file"))
                || !self.command_templates.is_empty();
            if !mutating && step.checks.iter().any(|check| check.kind == "file_changed") {
                return Err(PlanError::InvalidCheck {
                    index: step.index,
                    message: "file_changed needs a step that writes or edits a file".to_owned(),
                });
            }
        }
        if !self.output.required_artifacts.is_empty() && self.command_templates.is_empty() {
            let writes = plan
                .steps
                .iter()
                .any(|step| matches!(step.tool.as_deref(), Some("write_file" | "edit_file")));
            if !writes {
                return Err(PlanError::InvalidCheck {
                    index: plan.steps[0].index,
                    message: "the plan never writes the required output files: add a step whose tool is write_file or edit_file".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Why semantic checks cannot pass automatically for this mission.
    ///
    /// `None` means either the mission has no semantic checks or it carries a
    /// validated automatic qualification. A human-review-only policy always
    /// returns its explicit user-visible reason.
    #[must_use]
    pub fn semantic_unavailable_reason(&self) -> Option<String> {
        let uses_semantic = self.checks.iter().any(|check| is_semantic_kind(check));
        if !uses_semantic {
            return None;
        }
        let policy = self.semantic.as_ref()?;
        if policy.automatic() {
            return None;
        }
        Some(
            policy.reason.clone().unwrap_or_else(|| {
                "semantic criteria need human review for this mission".to_owned()
            }),
        )
    }

    /// The frozen semantic capability, when the mission declares one.
    #[must_use]
    pub fn semantic_policy(&self) -> Option<&SemanticPolicy> {
        self.semantic.as_ref()
    }

    /// Rejects semantic checks whose source or image is not a declared input,
    /// so a plan can never point at bytes the acceptance did not freeze.
    fn validate_semantic_args(&self, index: u32, check: &CheckSpec) -> Result<(), PlanError> {
        let reject = |message: String| PlanError::InvalidCheck { index, message };
        match check.kind.as_str() {
            "d1_grounded" => {
                if !check
                    .args
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    return Err(reject("d1_grounded needs the output JSON path".to_owned()));
                }
                let source = check.args.get("source").and_then(Value::as_str);
                match source {
                    Some(source) if self.inputs.contains_key(source) => {}
                    Some(source) => {
                        return Err(reject(format!(
                            "d1_grounded source {source:?} is not a declared mission input"
                        )));
                    }
                    None => {
                        return Err(reject(
                            "d1_grounded needs `source`, a declared mission input".to_owned(),
                        ));
                    }
                }
            }
            "vl_criterion" => {
                let image = check.args.get("image").and_then(Value::as_str);
                match image {
                    Some(image)
                        if self
                            .inputs
                            .get(image)
                            .is_some_and(|input| input.kind == InputKind::Image) => {}
                    Some(image) => {
                        return Err(reject(format!(
                            "vl_criterion image {image:?} is not a declared image input"
                        )));
                    }
                    None => {
                        return Err(reject(
                            "vl_criterion needs `image`, a declared image input".to_owned(),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Validates a plan and freezes it into the run's immutable acceptance contract.
    pub fn compile_acceptance(
        &self,
        goal: impl Into<String>,
        inputs: BTreeMap<String, InputArtifact>,
        plan: PlanDraft,
    ) -> Result<FrozenAcceptance, PlanError> {
        self.validate_plan(&plan)?;
        AcceptanceSpec {
            schema_version: 1,
            mission: self.name.clone(),
            mission_version: self.version,
            goal: goal.into(),
            inputs,
            steps: plan.steps,
            output: self.output.clone(),
            allowed_tools: self.tools.iter().cloned().collect(),
            allowed_checks: self.checks.iter().cloned().collect(),
            max_steps: self.limits.max_steps,
            max_attempts: self.limits.max_attempts,
        }
        .freeze()
    }

    #[must_use]
    pub fn accepts_strategy(
        &self,
        acceptance: &FrozenAcceptance,
        strategy: &ExecutionStrategy,
    ) -> bool {
        acceptance.validate_strategy(strategy).is_ok()
    }
}

/// Loads and validates every mission manifest in a directory.
pub fn load_missions(
    directory: impl AsRef<Path>,
) -> Result<BTreeMap<String, MissionManifest>, PlanError> {
    let directory = directory.as_ref();
    let entries = fs::read_dir(directory)
        .map_err(|error| PlanError::Io(format!("{}: {error}", directory.display())))?;
    let mut missions = BTreeMap::new();
    for entry in entries {
        let path = entry
            .map_err(|error| PlanError::Io(format!("{}: {error}", directory.display())))?
            .path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .map_err(|error| PlanError::Io(format!("{}: {error}", path.display())))?;
        let manifest = MissionManifest::from_json(&raw)?;
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if stem != manifest.name {
            return Err(PlanError::InvalidManifest(format!(
                "{} declares mission {:?}",
                path.display(),
                manifest.name
            )));
        }
        if missions.insert(manifest.name.clone(), manifest).is_some() {
            return Err(PlanError::InvalidManifest(format!(
                "duplicate mission name in {}",
                path.display()
            )));
        }
    }
    if missions.is_empty() {
        return Err(PlanError::InvalidManifest(format!(
            "no mission manifests in {}",
            directory.display()
        )));
    }
    Ok(missions)
}

fn validate_unique(values: &[String], label: &str) -> Result<(), PlanError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if value.trim().is_empty() || !seen.insert(value.as_str()) {
            return Err(PlanError::InvalidManifest(format!(
                "{label} names must be non-empty and unique"
            )));
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == '_')
}

fn validate_check_args(check: &crate::mission::CheckSpec) -> Result<(), String> {
    let non_empty_string = |key: &str| {
        check
            .args
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };
    match check.kind.as_str() {
        "file_exists" | "dir_exists" | "file_changed" => {
            if !non_empty_string("path") {
                return Err(format!("{} needs a non-empty path", check.kind));
            }
        }
        "content_matches" => {
            if !non_empty_string("path") {
                return Err("content_matches needs a non-empty path".to_owned());
            }
            let contains_ok = check.args.get("contains").is_some_and(|value| match value {
                Value::String(text) => !text.is_empty(),
                Value::Array(values) => {
                    !values.is_empty()
                        && values
                            .iter()
                            .all(|value| value.as_str().is_some_and(|text| !text.is_empty()))
                }
                _ => false,
            });
            let has_condition = contains_ok
                || non_empty_string("regex")
                || check
                    .args
                    .get("all_of")
                    .and_then(Value::as_array)
                    .is_some_and(|values| !values.is_empty());
            if !has_condition {
                return Err("content_matches needs contains, all_of, or regex".to_owned());
            }
        }
        "content_absent" => {
            if !non_empty_string("path") {
                return Err("content_absent needs a non-empty path".to_owned());
            }
            if !check
                .args
                .get("forbidden")
                .and_then(Value::as_array)
                .is_some_and(|values| !values.is_empty())
            {
                return Err("content_absent needs a non-empty forbidden list".to_owned());
            }
        }
        "json_schema" => {
            if !non_empty_string("path") {
                return Err("json_schema needs a non-empty path".to_owned());
            }
            if !check.args.get("schema").is_some_and(Value::is_object) {
                return Err("json_schema needs an object schema".to_owned());
            }
        }
        "numeric_consistency" => {
            if !non_empty_string("path") {
                return Err("numeric_consistency needs a non-empty path".to_owned());
            }
            if !non_empty_string("total")
                || !check
                    .args
                    .get("sum")
                    .and_then(Value::as_array)
                    .is_some_and(|values| !values.is_empty())
            {
                return Err("numeric_consistency needs total and a non-empty sum list".to_owned());
            }
        }
        "report_complete" => {
            if !check
                .args
                .get("fields")
                .and_then(Value::as_array)
                .is_some_and(|values| !values.is_empty())
            {
                return Err("report_complete needs a non-empty fields list".to_owned());
            }
        }
        "transcript_nonempty" => {
            if check
                .args
                .get("min_words")
                .and_then(Value::as_u64)
                .is_some_and(|value| value == 0)
            {
                return Err("transcript_nonempty min_words must be positive".to_owned());
            }
        }
        "command_exit" | "tests_pass" => {
            if !non_empty_string("template") {
                return Err(format!("{} needs a command template ID", check.kind));
            }
        }
        "stdout_contains" => {
            if !(non_empty_string("contains") || non_empty_string("regex")) {
                return Err("stdout_contains needs contains or regex".to_owned());
            }
        }
        "vl_criterion" | "d1_noul" => {
            if !non_empty_string("criterion") {
                return Err(format!("{} needs a criterion", check.kind));
            }
        }
        "d1_choice" => {
            if !non_empty_string("criterion")
                || !check.args.get("options").is_some_and(Value::is_object)
                || !non_empty_string("expect")
            {
                return Err("d1_choice needs criterion, options, and expect".to_owned());
            }
        }
        "d1_grounded" => {
            if !check.args.get("fields").is_some_and(Value::is_object) {
                return Err("d1_grounded needs a fields object".to_owned());
            }
        }
        other => return Err(format!("no argument validator for check {other:?}")),
    }
    Ok(())
}
