use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{mission::PlanError, ArtifactId};

/// A check specification kept opaque to the planner and frozen at approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub kind: String,
    #[serde(flatten)]
    pub args: BTreeMap<String, Value>,
}

impl CheckSpec {
    #[must_use]
    pub fn new<'a>(
        kind: impl Into<String>,
        args: impl IntoIterator<Item = (&'a str, Value)>,
    ) -> Self {
        Self {
            kind: kind.into(),
            args: args
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedStep {
    pub index: u32,
    pub title: String,
    pub criterion: String,
    pub checks: Vec<CheckSpec>,
    #[serde(default)]
    pub tool: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDraft {
    pub steps: Vec<PlannedStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InputArtifact {
    pub id: ArtifactId,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredArtifact {
    pub path: String,
    #[serde(default = "default_true")]
    pub non_empty: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputContract {
    #[serde(default)]
    pub schema: Option<Value>,
    #[serde(default)]
    pub required_artifacts: Vec<RequiredArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceSpec {
    pub schema_version: u32,
    pub mission: String,
    pub mission_version: u32,
    pub goal: String,
    pub inputs: BTreeMap<String, InputArtifact>,
    pub steps: Vec<PlannedStep>,
    pub output: OutputContract,
    pub allowed_tools: BTreeSet<String>,
    pub allowed_checks: BTreeSet<String>,
    pub max_steps: usize,
    pub max_attempts: usize,
}

impl AcceptanceSpec {
    /// Canonicalizes and freezes the contract into a hash-addressed object.
    pub fn freeze(mut self) -> Result<FrozenAcceptance, PlanError> {
        if self.goal.trim().is_empty() {
            return Err(PlanError::EmptyGoal);
        }
        if self.steps.is_empty() {
            return Err(PlanError::NoSteps);
        }
        if self.steps.len() > self.max_steps || self.steps.len() > 32 {
            return Err(PlanError::TooManySteps {
                actual: self.steps.len(),
                maximum: self.max_steps.min(32),
            });
        }

        self.steps.sort_by_key(|step| step.index);
        for (position, step) in self.steps.iter().enumerate() {
            let expected = u32::try_from(position + 1).map_err(|_| PlanError::NoSteps)?;
            if step.index != expected {
                return Err(PlanError::InvalidStepIndex { index: step.index });
            }
            if step.criterion.trim().chars().count() < 10 {
                return Err(PlanError::VagueCriterion { index: step.index });
            }
            if step.checks.is_empty() {
                return Err(PlanError::NoChecks { index: step.index });
            }
            for check in &step.checks {
                if check.kind.trim().is_empty() {
                    return Err(PlanError::InvalidCheck {
                        index: step.index,
                        message: "check kind is empty".to_owned(),
                    });
                }
            }
            if let Some(tool) = &step.tool {
                if !self.allowed_tools.contains(tool) {
                    return Err(PlanError::UnknownTool {
                        index: step.index,
                        tool: tool.clone(),
                    });
                }
            }
        }

        for input in self.inputs.values() {
            if !is_sha256(&input.sha256) {
                return Err(PlanError::InvalidArtifactHash(input.sha256.clone()));
            }
        }
        for artifact in &self.output.required_artifacts {
            validate_relative_path(&artifact.path)?;
        }
        let mut seen = BTreeSet::new();
        for artifact in &self.output.required_artifacts {
            if !seen.insert(artifact.path.as_str()) {
                return Err(PlanError::InvalidPath(format!(
                    "duplicate required artifact {}",
                    artifact.path
                )));
            }
        }

        let bytes = serde_json::to_vec(&self)
            .map_err(|error| PlanError::Json(format!("cannot canonicalize acceptance: {error}")))?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        Ok(FrozenAcceptance { spec: self, hash })
    }
}

/// The approved contract. Recovery can replace strategies, never this object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenAcceptance {
    spec: AcceptanceSpec,
    hash: String,
}

impl FrozenAcceptance {
    #[must_use]
    pub fn spec(&self) -> &AcceptanceSpec {
        &self.spec
    }

    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn verify_hash(&self) -> Result<(), PlanError> {
        let bytes = serde_json::to_vec(&self.spec)
            .map_err(|error| PlanError::Json(format!("cannot canonicalize acceptance: {error}")))?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        if hash == self.hash {
            Ok(())
        } else {
            Err(PlanError::AcceptanceHashMismatch)
        }
    }

    pub fn validate_strategy(&self, strategy: &ExecutionStrategy) -> Result<(), PlanError> {
        if strategy.acceptance_hash != self.hash {
            return Err(PlanError::AcceptanceHashMismatch);
        }
        for action in &strategy.actions {
            if self
                .spec
                .steps
                .binary_search_by_key(&action.step_index, |step| step.index)
                .is_err()
            {
                return Err(PlanError::UnknownStrategyStep(action.step_index));
            }
            if !self.spec.allowed_tools.contains(&action.tool) {
                return Err(PlanError::UnknownTool {
                    index: action.step_index,
                    tool: action.tool.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyAction {
    pub step_index: u32,
    pub tool: String,
    #[serde(default)]
    pub args: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStrategy {
    pub acceptance_hash: String,
    #[serde(default)]
    pub actions: Vec<StrategyAction>,
}

pub(crate) fn validate_relative_path(raw: &str) -> Result<(), PlanError> {
    if raw.trim().is_empty() {
        return Err(PlanError::InvalidPath("path is empty".to_owned()));
    }
    let path = Path::new(raw);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(PlanError::InvalidPath(format!(
            "{raw:?} must stay relative to the approved workspace"
        )));
    }
    Ok(())
}

pub(crate) fn is_sha256(raw: &str) -> bool {
    raw.len() == 64 && raw.bytes().all(|byte| byte.is_ascii_hexdigit())
}

const fn default_true() -> bool {
    true
}
