//! Test fixtures that pin the `desk_task` acceptance contract.
//!
//! A fixture describes the source note, the fixed output path, the terms a
//! correct summary must contain, a forbidden invented term, and the expected
//! summary used by fake-model golden runs. The fixture never executes
//! anything; the CLI derives plan checks from it before freezing acceptance.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use lfm3_harness::{
    mission::{CheckSpec, PlanDraft, RequiredArtifact},
    store::sha256_hex,
    ContentDigest, MissionManifest,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Scripted behaviour available only to the `--fake-model` test gate.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FakeBehavior {
    /// The first write attempt fails the deterministic checks.
    #[serde(default)]
    pub fail_first_write: bool,
    /// The scripted model writes the forbidden summary instead of the
    /// expected one.
    #[serde(default)]
    pub write_forbidden: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeskFixture {
    pub goal: String,
    /// Relative path of the note the agent must read.
    pub source: String,
    /// Fixed relative output path.
    pub output: String,
    /// Terms every correct summary must contain.
    pub required_terms: Vec<String>,
    /// Terms a correct summary must never contain.
    #[serde(default)]
    pub forbidden_terms: Vec<String>,
    /// Files written into the workspace before the run starts.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// The summary a competent writer produces; used by fake mode and goldens.
    #[serde(default)]
    pub expected_summary: Option<String>,
    /// Deliberately wrong text used by the fake-mode failure cases.
    #[serde(default)]
    pub wrong_text: Option<String>,
    #[serde(default)]
    pub fake: FakeBehavior,
}

impl DeskFixture {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .map_err(|error| format!("cannot read fixture {}: {error}", path.display()))?;
        let fixture: Self = serde_json::from_str(&raw)
            .map_err(|error| format!("fixture {} is invalid JSON: {error}", path.display()))?;
        fixture.validate()?;
        Ok(fixture)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.goal.trim().is_empty() {
            return Err("the fixture goal must be non-empty".to_owned());
        }
        if self.source.trim().is_empty() {
            return Err("the fixture source path must be non-empty".to_owned());
        }
        if self.output.trim().is_empty() {
            return Err("the fixture output path must be non-empty".to_owned());
        }
        if self.required_terms.is_empty() {
            return Err("the fixture needs at least one required term".to_owned());
        }
        if self.required_terms.iter().any(|term| term.is_empty()) {
            return Err("required terms must be non-empty strings".to_owned());
        }
        if !self.files.contains_key(&self.source) {
            return Err(format!(
                "the fixture files do not contain the source note {:?}",
                self.source
            ));
        }
        if (self.fake.fail_first_write || self.fake.write_forbidden) && self.wrong_text.is_none() {
            return Err("fake failure modes need a wrong_text".to_owned());
        }
        Ok(())
    }

    /// The goal the model sees, with the verified source text attached so a
    /// single-action executor can ground its writing without cross-step state.
    pub fn acceptance_goal(&self) -> String {
        let text = self.files.get(&self.source).cloned().unwrap_or_default();
        format!(
            "{}\n\nSource note {}:\n{}",
            self.goal.trim_end(),
            self.source,
            text.trim_end()
        )
    }

    /// The correct summary used by fake mode and golden comparisons.
    pub fn expected_summary(&self) -> Result<&str, String> {
        self.expected_summary
            .as_deref()
            .ok_or_else(|| "the fixture has no expected_summary".to_owned())
    }

    /// The scripted content for one write attempt.
    pub fn scripted_write(&self, attempt: usize) -> Result<String, String> {
        if self.fake.write_forbidden {
            return self
                .wrong_text
                .clone()
                .ok_or_else(|| "write_forbidden needs a wrong_text".to_owned());
        }
        if self.fake.fail_first_write && attempt == 1 {
            return self
                .wrong_text
                .clone()
                .ok_or_else(|| "fail_first_write needs a wrong_text".to_owned());
        }
        Ok(self.expected_summary()?.to_owned())
    }

    /// Deterministic checks appended to the last planned step before freezing.
    pub fn checks(&self) -> Result<Vec<CheckSpec>, String> {
        let mut checks = vec![
            CheckSpec::new("file_exists", [("path", json!(self.output))]),
            CheckSpec::new(
                "content_matches",
                [
                    ("path", json!(self.output)),
                    ("all_of", json!(self.required_terms)),
                ],
            ),
        ];
        if !self.forbidden_terms.is_empty() {
            checks.push(CheckSpec::new(
                "content_absent",
                [
                    ("path", json!(self.output)),
                    ("forbidden", json!(self.forbidden_terms)),
                ],
            ));
        }
        Ok(checks)
    }

    /// Clones the mission with the fixture's fixed output contract.
    ///
    /// The verified source text is embedded in the goal, so the only action a
    /// fixture run needs is the write. Narrowing the granted tools to that
    /// action keeps a small model from spending the one permitted step on a
    /// redundant read.
    pub fn mission(&self, mission: &MissionManifest) -> MissionManifest {
        let mut mission = mission.clone();
        mission.output.required_artifacts = vec![RequiredArtifact {
            path: self.output.clone(),
            non_empty: true,
        }];
        mission.output.schema = None;
        mission.tools = vec!["write_file".to_owned()];
        mission
    }

    /// Writes the fixture files and removes any previous output.
    pub fn prepare_workspace(&self, workspace: impl AsRef<Path>) -> Result<PathBuf, String> {
        let root = workspace.as_ref();
        fs::create_dir_all(root)
            .map_err(|error| format!("create workspace {}: {error}", root.display()))?;
        for (relative, content) in &self.files {
            let path = join_relative(root, relative)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("create {}: {error}", parent.display()))?;
            }
            fs::write(&path, content)
                .map_err(|error| format!("write fixture {}: {error}", path.display()))?;
        }
        let output = join_relative(root, &self.output)?;
        if output.exists() {
            fs::remove_file(&output)
                .map_err(|error| format!("remove previous {}: {error}", output.display()))?;
        }
        fs::canonicalize(root)
            .map_err(|error| format!("canonicalize workspace {}: {error}", root.display()))
    }

    /// Digest of the source note as it exists in the workspace.
    pub fn source_digest(&self, workspace: &Path) -> Result<ContentDigest, String> {
        let path = join_relative(workspace, &self.source)?;
        ContentDigest::of_file(&path)
            .map(|(digest, _)| digest)
            .map_err(|error| format!("digest {}: {error}", path.display()))
    }

    /// Digest of the expected summary, used by the fake-model golden.
    pub fn expected_summary_sha256(&self) -> Result<String, String> {
        Ok(sha256_hex(self.expected_summary()?.as_bytes()))
    }
}

/// Appends the fixture checks to the last planned step, keeping the plan
/// contract valid before it is frozen.
pub fn append_fixture_checks(plan: &mut PlanDraft, fixture: &DeskFixture) -> Result<(), String> {
    let Some(last) = plan.steps.iter_mut().max_by_key(|step| step.index) else {
        return Err("the plan has no steps to verify".to_owned());
    };
    last.checks.extend(fixture.checks()?);
    Ok(())
}

fn join_relative(root: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.trim().is_empty() {
        return Err("relative paths must be non-empty".to_owned());
    }
    let candidate = Path::new(relative);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "fixture path {relative:?} must stay inside the workspace"
        ));
    }
    Ok(root.join(candidate))
}

#[cfg(test)]
mod tests {
    use super::DeskFixture;

    fn fixture(json: &str) -> DeskFixture {
        serde_json::from_str(json).expect("fixture JSON")
    }

    const BASIC: &str = r#"{
        "goal": "Summarize the note.",
        "source": "demo/notes.md",
        "output": "out/summary.md",
        "required_terms": ["lamp", "Mina"],
        "forbidden_terms": ["dragon"],
        "files": {"demo/notes.md": "Mina repaired the lamp.\n"},
        "expected_summary": "Summary: Mina repaired the lamp.\n"
    }"#;

    #[test]
    fn fixture_checks_are_derived_not_scripted() {
        let fixture = fixture(BASIC);
        fixture.validate().unwrap();
        let checks = fixture.checks().unwrap();
        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0].kind, "file_exists");
        assert_eq!(checks[1].kind, "content_matches");
        assert_eq!(checks[2].kind, "content_absent");
    }

    #[test]
    fn goal_carries_the_verified_source_text() {
        let fixture = fixture(BASIC);
        let goal = fixture.acceptance_goal();
        assert!(goal.contains("Mina repaired the lamp."));
    }

    #[test]
    fn escape_paths_are_rejected() {
        let fixture = fixture(
            r#"{
                "goal": "x",
                "source": "../secret.txt",
                "output": "out/summary.md",
                "required_terms": ["x"],
                "files": {"../secret.txt": "x"}
            }"#,
        );
        let directory = tempfile::tempdir().unwrap();
        assert!(fixture.prepare_workspace(directory.path()).is_err());
    }
}
