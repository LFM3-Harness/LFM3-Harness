//! Deterministic checks over observed bytes, processes, and typed tool results.
//!
//! A check may pass only by producing evidence bound to the attempt being
//! verified. Model text is never evidence: a `read_file` observation that
//! happens to say `exit=0` cannot satisfy a command check, and a pass built
//! without same-attempt evidence is downgraded to an inconclusive error.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    mission::{CheckSpec, InputArtifact},
    paths::{self, Mode, PathError},
    store::ArtifactStore,
    tools::{CommandRun, ToolName, ToolResult, ToolStatus},
    AttemptId,
};

use super::{media_type_for, ContentDigest};

/// Outcome of one deterministic check.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The criterion was observed to hold, backed by evidence.
    Pass,
    /// The criterion was observed not to hold.
    Fail,
    /// The criterion could not be decided; never treated as a pass.
    Error,
}

impl CheckStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Error => "error",
        }
    }
}

/// One durable pointer to what a check actually observed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckEvidence {
    /// Where the observation came from, for example `workspace:out/report.json`
    /// or `command_run:tests`.
    pub source: String,
    /// The attempt that produced the observation.
    pub attempt: Option<AttemptId>,
    /// Digest of the observed bytes, when the observation is content.
    pub digest: Option<ContentDigest>,
    pub detail: String,
    /// Present only for a semantic judgement. The judge's exact prompt, raw
    /// response, identity, confidence, and output pointers live here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<super::SemanticEvidence>,
}

/// Verified result of one check, bound to the frozen acceptance hash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub kind: String,
    pub acceptance_hash: String,
    pub step_index: u32,
    pub attempt: AttemptId,
    pub status: CheckStatus,
    pub message: String,
    pub evidence: Vec<CheckEvidence>,
}

/// An unknown check kind is a programming error, not a mission outcome.
#[derive(Debug, Error)]
pub enum CheckError {
    #[error("unknown check kind {0:?}")]
    UnknownKind(String),
}

/// A digest captured for every regular file in the workspace.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileManifest {
    pub entries: BTreeMap<String, ContentDigest>,
}

impl FileManifest {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, raw: impl AsRef<str>, digest: ContentDigest) {
        self.entries.insert(normalized(raw.as_ref()), digest);
    }

    #[must_use]
    pub fn get(&self, raw: &str) -> Option<&ContentDigest> {
        self.entries.get(&normalized(raw))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Captures a manifest without following links or descending into build
    /// output. Used to prove that a step changed specific bytes.
    pub fn capture(root: &Path) -> io::Result<Self> {
        let mut manifest = Self::new();
        walk(root, root, 0, &mut manifest)?;
        Ok(manifest)
    }
}

/// Everything a check may consult. Nothing here is model-authored except the
/// frozen spec and typed results that the harness itself produced.
#[derive(Clone, Debug)]
pub struct CheckContext {
    pub acceptance_hash: String,
    pub step_index: u32,
    pub attempt: AttemptId,
    pub workspace: PathBuf,
    pub tool_results: Vec<ToolResult>,
    pub command_runs: Vec<CommandRun>,
    pub before: FileManifest,
    /// Default report location when a check does not name its own path.
    pub report_path: Option<String>,
    /// Optional content store used to re-verify artifact bytes by digest.
    pub artifacts: Option<ArtifactStore>,
    /// Provenance-bound mission inputs, keyed by declared input name.
    pub inputs: BTreeMap<String, InputArtifact>,
}

impl CheckContext {
    #[must_use]
    pub fn new(
        acceptance_hash: impl Into<String>,
        step_index: u32,
        attempt: AttemptId,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        Self {
            acceptance_hash: acceptance_hash.into(),
            step_index,
            attempt,
            workspace: workspace.into(),
            tool_results: Vec::new(),
            command_runs: Vec::new(),
            before: FileManifest::new(),
            report_path: None,
            artifacts: None,
            inputs: BTreeMap::new(),
        }
    }

    /// A context whose only observation is free-form model text. Used to prove
    /// that text can never satisfy a command or artifact check.
    #[must_use]
    pub fn fixture_with_observation(observation: &str) -> Self {
        let attempt = AttemptId::new();
        let mut context = Self::new(
            "0".repeat(64),
            1,
            attempt.clone(),
            std::env::temp_dir().join("lfm3-check-fixture"),
        );
        context.tool_results.push(ToolResult {
            attempt: attempt.clone(),
            call_id: "call-1".to_owned(),
            tool: ToolName::ReadFile,
            status: ToolStatus::Succeeded,
            exit_code: None,
            output: observation.to_owned(),
            artifacts: Vec::new(),
            duration: Duration::ZERO,
        });
        context.command_runs.push(CommandRun {
            attempt: attempt.clone(),
            template: "workspace_tests".to_owned(),
            status: ToolStatus::Failed,
            exit_code: None,
            output: observation.to_owned(),
            duration: Duration::ZERO,
        });
        context
    }

    #[must_use]
    pub fn with_report_path(mut self, path: impl Into<String>) -> Self {
        self.report_path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_before(mut self, path: &str, digest: ContentDigest) -> Self {
        self.before.insert(path, digest);
        self
    }

    #[must_use]
    pub fn with_tool_result(mut self, result: ToolResult) -> Self {
        self.tool_results.push(result);
        self
    }

    #[must_use]
    pub fn with_command_run(mut self, run: CommandRun) -> Self {
        self.command_runs.push(run);
        self
    }

    #[must_use]
    pub fn with_artifacts(mut self, artifacts: ArtifactStore) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    fn latest_run(&self, template: Option<&str>) -> Option<&CommandRun> {
        self.command_runs.iter().rev().find(|run| {
            run.attempt == self.attempt && template.is_none_or(|expected| run.template == expected)
        })
    }

    fn same_attempt_artifact<'a>(
        &'a self,
        digest: &ContentDigest,
    ) -> Option<(&'a ToolResult, &'a ContentDigest)> {
        self.tool_results
            .iter()
            .filter(|result| result.attempt == self.attempt && result.succeeded())
            .find_map(|result| {
                result
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.sha256 == digest.sha256)
                    .map(|artifact| (result, artifact))
            })
    }

    fn store_confirms(&self, digest: &ContentDigest) -> Result<(), String> {
        let Some(store) = &self.artifacts else {
            return Ok(());
        };
        match store.read(&digest.sha256) {
            Ok(bytes) if digest.matches(&bytes) => Ok(()),
            Ok(_) => Err(format!(
                "stored bytes for {} do not match the observed digest",
                digest.sha256
            )),
            Err(_) => Err(format!("artifact store does not contain {}", digest.sha256)),
        }
    }
}

/// Evaluates one frozen check spec. Invalid specs are inconclusive errors;
/// only a genuinely unknown kind is a caller bug.
pub async fn evaluate(spec: CheckSpec, context: CheckContext) -> Result<CheckResult, CheckError> {
    let result = match spec.kind.as_str() {
        "file_exists" => file_exists(&spec, &context),
        "dir_exists" => dir_exists(&spec, &context),
        "file_changed" => file_changed(&spec, &context),
        "content_matches" => content_matches(&spec, &context),
        "content_absent" => content_absent(&spec, &context),
        "json_schema" => json_schema(&spec, &context),
        "numeric_consistency" => numeric_consistency(&spec, &context),
        "report_complete" => report_complete(&spec, &context),
        "transcript_nonempty" => transcript_nonempty(&spec, &context),
        "command_exit" => command_exit(&spec, &context),
        "tests_pass" => tests_pass(&spec, &context),
        "stdout_contains" => stdout_contains(&spec, &context),
        // Semantic criteria need a calibrated model and are not deterministic.
        "vl_criterion" | "d1_noul" | "d1_choice" | "d1_grounded" => error(
            &spec,
            &context,
            format!(
                "{} is a semantic criterion; the deterministic verifier cannot decide it",
                spec.kind
            ),
        ),
        other => return Err(CheckError::UnknownKind(other.to_owned())),
    };
    Ok(result)
}

/// Named entry point matching the planned check interface.
pub struct Check;

impl Check {
    pub async fn evaluate(
        spec: CheckSpec,
        context: CheckContext,
    ) -> Result<CheckResult, CheckError> {
        evaluate(spec, context).await
    }
}

fn file_exists(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(raw) = string_arg(spec, "path") else {
        return error(spec, context, "file_exists needs a non-empty path");
    };
    let key = normalized(raw);
    match paths::resolve(&context.workspace, raw, Mode::Read) {
        Ok(path) => match ContentDigest::of_file(&path) {
            Ok((digest, _)) => pass(
                spec,
                context,
                format!("{key} exists"),
                vec![digest_evidence(
                    context,
                    format!("workspace:{key}"),
                    digest,
                    "file exists",
                )],
            ),
            Err(problem) => error(spec, context, format!("{key}: {problem}")),
        },
        Err(PathError::NotFound) => fail(
            spec,
            context,
            format!("{key} does not exist"),
            vec![workspace_evidence(context, &key, "file is absent")],
        ),
        Err(PathError::NotAFile) => fail(
            spec,
            context,
            format!("{key} is a directory, not a file"),
            vec![workspace_evidence(context, &key, "path is a directory")],
        ),
        Err(problem) => error(spec, context, describe(problem)),
    }
}

fn dir_exists(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(raw) = string_arg(spec, "path") else {
        return error(spec, context, "dir_exists needs a non-empty path");
    };
    let key = normalized(raw);
    match paths::resolve(&context.workspace, raw, Mode::List) {
        Ok(_) => pass(
            spec,
            context,
            format!("{key} exists"),
            vec![workspace_evidence(context, &key, "directory exists")],
        ),
        Err(PathError::NotFound) => fail(
            spec,
            context,
            format!("{key} does not exist"),
            vec![workspace_evidence(context, &key, "directory is absent")],
        ),
        Err(PathError::NotADirectory) => fail(
            spec,
            context,
            format!("{key} is a file, not a directory"),
            vec![workspace_evidence(context, &key, "path is a file")],
        ),
        Err(problem) => error(spec, context, describe(problem)),
    }
}

fn file_changed(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(raw) = string_arg(spec, "path") else {
        return error(spec, context, "file_changed needs a non-empty path");
    };
    let key = normalized(raw);
    let Some(before) = context.before.get(&key) else {
        return error(
            spec,
            context,
            format!("file_changed has no before digest for {key}"),
        );
    };
    let before = before.clone();
    let path = match paths::resolve(&context.workspace, raw, Mode::Read) {
        Ok(path) => path,
        Err(PathError::NotFound) => {
            return fail(
                spec,
                context,
                format!("{key} is missing after the attempt"),
                vec![workspace_evidence(context, &key, "file is absent")],
            );
        }
        Err(problem) => return error(spec, context, describe(problem)),
    };
    let (digest, _) = match ContentDigest::of_file(&path) {
        Ok(observed) => observed,
        Err(problem) => return error(spec, context, format!("{key}: {problem}")),
    };
    if digest.sha256 == before.sha256 {
        return fail(
            spec,
            context,
            format!("{key} content is unchanged"),
            vec![digest_evidence(
                context,
                format!("workspace:{key}"),
                digest,
                format!("unchanged from {}", before.sha256),
            )],
        );
    }
    let Some((result, artifact)) = context.same_attempt_artifact(&digest) else {
        return error(
            spec,
            context,
            format!("changed bytes of {key} were not produced by a tool in this attempt"),
        );
    };
    if let Err(problem) = context.store_confirms(artifact) {
        return error(spec, context, problem);
    }
    pass(
        spec,
        context,
        format!("{key} changed to a same-attempt artifact"),
        vec![CheckEvidence {
            source: format!("tool_result:{}", result.call_id),
            attempt: Some(result.attempt.clone()),
            digest: Some(artifact.clone()),
            detail: format!("changed from {}", before.sha256),
            semantic: None,
        }],
    )
}

fn content_matches(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(raw) = string_arg(spec, "path") else {
        return error(spec, context, "content_matches needs a non-empty path");
    };
    let pattern = raw_string(spec, "regex").filter(|value| !value.trim().is_empty());
    // `contains` accepts one string or a list; a list means every entry must
    // appear. The planner sometimes uses a list where the schema asks for a
    // string, and both readings demand the same bytes.
    let mut required: Vec<String> = match spec.args.get("contains") {
        Some(Value::String(text)) if !text.is_empty() => vec![text.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    required.extend(
        spec.args
            .get("all_of")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    );
    if required.is_empty() && pattern.is_none() {
        return error(
            spec,
            context,
            "content_matches needs contains, all_of, or regex",
        );
    }
    let key = normalized(raw);
    let (digest, bytes) = match observe_file(spec, context, raw) {
        Ok(observed) => observed,
        Err(result) => return *result,
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => return error(spec, context, format!("{key} is not UTF-8 text")),
    };
    let evidence = vec![digest_evidence(
        context,
        format!("workspace:{key}"),
        digest,
        "content observed",
    )];
    for needle in &required {
        if !text.contains(needle.as_str()) {
            return CheckResult {
                kind: spec.kind.clone(),
                acceptance_hash: context.acceptance_hash.clone(),
                step_index: context.step_index,
                attempt: context.attempt.clone(),
                status: CheckStatus::Fail,
                message: format!("{key} does not contain {needle:?}"),
                evidence,
            };
        }
    }
    if let Some(pattern) = pattern {
        let regex = match Regex::new(pattern) {
            Ok(regex) => regex,
            Err(problem) => return error(spec, context, format!("invalid regex: {problem}")),
        };
        if !regex.is_match(&text) {
            return CheckResult {
                kind: spec.kind.clone(),
                acceptance_hash: context.acceptance_hash.clone(),
                step_index: context.step_index,
                attempt: context.attempt.clone(),
                status: CheckStatus::Fail,
                message: format!("{key} does not match the required regex"),
                evidence,
            };
        }
    }
    pass(
        spec,
        context,
        format!("{key} matches every required condition"),
        evidence,
    )
}

/// Fails when the observed file contains any forbidden term. Used to reject
/// invented content that a required-terms check alone would allow through.
fn content_absent(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(raw) = string_arg(spec, "path") else {
        return error(spec, context, "content_absent needs a non-empty path");
    };
    let forbidden: Vec<String> = spec
        .args
        .get("forbidden")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if forbidden.is_empty() {
        return error(
            spec,
            context,
            "content_absent needs a non-empty forbidden list",
        );
    }
    let key = normalized(raw);
    let (digest, bytes) = match observe_file(spec, context, raw) {
        Ok(observed) => observed,
        Err(result) => return *result,
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => return error(spec, context, format!("{key} is not UTF-8 text")),
    };
    let evidence = vec![digest_evidence(
        context,
        format!("workspace:{key}"),
        digest,
        "content observed",
    )];
    let found: Vec<&str> = forbidden
        .iter()
        .filter(|term| text.contains(term.as_str()))
        .map(String::as_str)
        .collect();
    if found.is_empty() {
        pass(
            spec,
            context,
            format!("{key} contains none of the forbidden terms"),
            evidence,
        )
    } else {
        CheckResult {
            kind: spec.kind.clone(),
            acceptance_hash: context.acceptance_hash.clone(),
            step_index: context.step_index,
            attempt: context.attempt.clone(),
            status: CheckStatus::Fail,
            message: format!("{key} contains forbidden text: {}", found.join(", ")),
            evidence,
        }
    }
}

fn json_schema(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(raw) = string_arg(spec, "path") else {
        return error(spec, context, "json_schema needs a non-empty path");
    };
    let Some(schema) = spec.args.get("schema").filter(|value| value.is_object()) else {
        return error(spec, context, "json_schema needs an object schema");
    };
    let key = normalized(raw);
    let (digest, bytes) = match observe_file(spec, context, raw) {
        Ok(observed) => observed,
        Err(result) => return *result,
    };
    let instance: Value = match serde_json::from_slice(&bytes) {
        Ok(instance) => instance,
        Err(problem) => {
            return fail(
                spec,
                context,
                format!("{key} is not valid JSON: {problem}"),
                vec![digest_evidence(
                    context,
                    format!("workspace:{key}"),
                    digest,
                    "invalid JSON",
                )],
            )
        }
    };
    let evidence = vec![digest_evidence(
        context,
        format!("workspace:{key}"),
        digest,
        "JSON observed",
    )];
    match validate(schema, &instance) {
        Ok(()) => pass(
            spec,
            context,
            format!("{key} satisfies the schema"),
            evidence,
        ),
        Err(SchemaProblem::Invalid(detail)) => CheckResult {
            kind: spec.kind.clone(),
            acceptance_hash: context.acceptance_hash.clone(),
            step_index: context.step_index,
            attempt: context.attempt.clone(),
            status: CheckStatus::Fail,
            message: format!("{key} does not satisfy the schema: {detail}"),
            evidence,
        },
        Err(SchemaProblem::Unsupported(keyword)) => error(
            spec,
            context,
            format!("schema keyword {keyword:?} is not supported by the deterministic verifier"),
        ),
    }
}

fn numeric_consistency(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(raw) = string_arg(spec, "path") else {
        return error(spec, context, "numeric_consistency needs a non-empty path");
    };
    let Some(total_field) = string_arg(spec, "total") else {
        return error(spec, context, "numeric_consistency needs a total field");
    };
    let sum_fields: Vec<String> = spec
        .args
        .get("sum")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if sum_fields.is_empty() {
        return error(
            spec,
            context,
            "numeric_consistency needs a non-empty sum list",
        );
    }
    let key = normalized(raw);
    let (digest, bytes) = match observe_file(spec, context, raw) {
        Ok(observed) => observed,
        Err(result) => return *result,
    };
    let object: BTreeMap<String, Value> = match serde_json::from_slice(&bytes) {
        Ok(Value::Object(map)) => map.into_iter().collect(),
        Ok(_) => return error(spec, context, format!("{key} is not a JSON object")),
        Err(problem) => return error(spec, context, format!("{key}: {problem}")),
    };
    let Some(total) = object.get(total_field).and_then(Value::as_f64) else {
        return error(
            spec,
            context,
            format!("{key} has no numeric {total_field:?} field"),
        );
    };
    let mut sum = 0.0_f64;
    for field in &sum_fields {
        let Some(value) = object.get(field).and_then(Value::as_f64) else {
            return error(
                spec,
                context,
                format!("{key} has no numeric {field:?} field"),
            );
        };
        sum += value;
    }
    let tolerance = 1e-6_f64.max(total.abs() * 1e-9);
    let evidence = vec![digest_evidence(
        context,
        format!("workspace:{key}"),
        digest,
        format!("total {total} vs sum {sum}"),
    )];
    if (total - sum).abs() <= tolerance {
        pass(
            spec,
            context,
            format!("{total_field} equals the sum of {}", sum_fields.join(", ")),
            evidence,
        )
    } else {
        CheckResult {
            kind: spec.kind.clone(),
            acceptance_hash: context.acceptance_hash.clone(),
            step_index: context.step_index,
            attempt: context.attempt.clone(),
            status: CheckStatus::Fail,
            message: format!("{total_field} is {total} but the parts sum to {sum}"),
            evidence,
        }
    }
}

fn report_complete(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let raw = string_arg(spec, "path").or(context.report_path.as_deref());
    let Some(raw) = raw else {
        return error(
            spec,
            context,
            "report_complete needs a path or a configured report location",
        );
    };
    let fields: Vec<String> = spec
        .args
        .get("fields")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if fields.is_empty() {
        return error(
            spec,
            context,
            "report_complete needs a non-empty fields list",
        );
    }
    let key = normalized(raw);
    let (digest, bytes) = match observe_file(spec, context, raw) {
        Ok(observed) => observed,
        Err(result) => return *result,
    };
    let object: BTreeMap<String, Value> = match serde_json::from_slice(&bytes) {
        Ok(Value::Object(map)) => map.into_iter().collect(),
        Ok(_) => return error(spec, context, format!("{key} is not a JSON object")),
        Err(problem) => return error(spec, context, format!("{key}: {problem}")),
    };
    let missing: Vec<&str> = fields
        .iter()
        .filter(|field| !is_filled(object.get(field.as_str())))
        .map(String::as_str)
        .collect();
    let evidence = vec![digest_evidence(
        context,
        format!("workspace:{key}"),
        digest,
        format!("report fields present: {}", fields.len() - missing.len()),
    )];
    if missing.is_empty() {
        pass(
            spec,
            context,
            format!("{key} reports every required field"),
            evidence,
        )
    } else {
        CheckResult {
            kind: spec.kind.clone(),
            acceptance_hash: context.acceptance_hash.clone(),
            step_index: context.step_index,
            attempt: context.attempt.clone(),
            status: CheckStatus::Fail,
            message: format!("{key} is missing filled fields: {}", missing.join(", ")),
            evidence,
        }
    }
}

fn transcript_nonempty(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let minimum = spec
        .args
        .get("min_words")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let Some(result) = context.tool_results.iter().rev().find(|result| {
        result.attempt == context.attempt
            && result.tool == ToolName::Transcribe
            && result.succeeded()
    }) else {
        return error(
            spec,
            context,
            "transcript_nonempty needs a successful transcription in this attempt",
        );
    };
    let Some(digest) = result.artifacts.first().cloned() else {
        return error(
            spec,
            context,
            "the transcription carries no audio digest evidence",
        );
    };
    let words = result
        .output
        .split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count();
    let evidence = vec![CheckEvidence {
        source: format!("tool_result:{}", result.call_id),
        attempt: Some(result.attempt.clone()),
        digest: Some(digest),
        detail: format!("{words} transcript words"),
        semantic: None,
    }];
    if words >= usize::try_from(minimum).unwrap_or(usize::MAX) {
        pass(
            spec,
            context,
            format!("transcript has at least {minimum} words"),
            evidence,
        )
    } else {
        CheckResult {
            kind: spec.kind.clone(),
            acceptance_hash: context.acceptance_hash.clone(),
            step_index: context.step_index,
            attempt: context.attempt.clone(),
            status: CheckStatus::Fail,
            message: format!("transcript has {words} words, fewer than {minimum}"),
            evidence,
        }
    }
}

fn command_exit(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(template) = string_arg(spec, "template") else {
        return error(spec, context, "command_exit needs a command template ID");
    };
    let expected = spec
        .args
        .get("exit_code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let Some(run) = context.latest_run(Some(template)) else {
        return error(
            spec,
            context,
            format!("no same-attempt command result for template {template:?}"),
        );
    };
    let Some(code) = run.exit_code else {
        return error(
            spec,
            context,
            format!("template {template:?} has no real process exit code"),
        );
    };
    let evidence = vec![run_evidence(context, run)];
    if i64::from(code) == expected {
        pass(
            spec,
            context,
            format!("{template} exited with {expected}"),
            evidence,
        )
    } else {
        CheckResult {
            kind: spec.kind.clone(),
            acceptance_hash: context.acceptance_hash.clone(),
            step_index: context.step_index,
            attempt: context.attempt.clone(),
            status: CheckStatus::Fail,
            message: format!("{template} exited with {code}, expected {expected}"),
            evidence,
        }
    }
}

fn tests_pass(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let Some(template) = string_arg(spec, "template") else {
        return error(spec, context, "tests_pass needs a command template ID");
    };
    let Some(run) = context.latest_run(Some(template)) else {
        return error(
            spec,
            context,
            format!("no same-attempt command result for template {template:?}"),
        );
    };
    let Some(code) = run.exit_code else {
        return error(
            spec,
            context,
            format!("template {template:?} has no real process exit code"),
        );
    };
    let evidence = vec![run_evidence(context, run)];
    let outcome = |status: CheckStatus, message: String| CheckResult {
        kind: spec.kind.clone(),
        acceptance_hash: context.acceptance_hash.clone(),
        step_index: context.step_index,
        attempt: context.attempt.clone(),
        status,
        message,
        evidence: evidence.clone(),
    };
    if code != 0 {
        return outcome(CheckStatus::Fail, format!("{template} exited with {code}"));
    }
    let Some(summary) = machine_summary(&run.output) else {
        return outcome(
            CheckStatus::Fail,
            format!(
                "{template} exited with 0 but printed no machine-readable \
                 {{\"passed\":N,\"failed\":M}} summary"
            ),
        );
    };
    if summary.failed != 0 {
        return outcome(
            CheckStatus::Fail,
            format!("{template} reported {} failed tests", summary.failed),
        );
    }
    if summary.passed == 0 {
        return outcome(
            CheckStatus::Fail,
            format!("{template} reported zero passed tests"),
        );
    }
    outcome(
        CheckStatus::Pass,
        format!("{template} passed {} tests", summary.passed),
    )
}

fn stdout_contains(spec: &CheckSpec, context: &CheckContext) -> CheckResult {
    let template = string_arg(spec, "template");
    let contains = raw_string(spec, "contains").filter(|value| !value.is_empty());
    let pattern = raw_string(spec, "regex").filter(|value| !value.trim().is_empty());
    if contains.is_none() && pattern.is_none() {
        return error(spec, context, "stdout_contains needs contains or regex");
    }
    let Some(run) = context.latest_run(template) else {
        return error(
            spec,
            context,
            "stdout_contains needs a same-attempt command result",
        );
    };
    let evidence = vec![run_evidence(context, run)];
    let matched = if let Some(needle) = contains {
        run.output.contains(needle)
    } else {
        let pattern = pattern.unwrap_or_default();
        match Regex::new(pattern) {
            Ok(regex) => regex.is_match(&run.output),
            Err(problem) => return error(spec, context, format!("invalid regex: {problem}")),
        }
    };
    if matched {
        pass(
            spec,
            context,
            format!("{} output contains the required text", run.template),
            evidence,
        )
    } else {
        CheckResult {
            kind: spec.kind.clone(),
            acceptance_hash: context.acceptance_hash.clone(),
            step_index: context.step_index,
            attempt: context.attempt.clone(),
            status: CheckStatus::Fail,
            message: format!("{} output does not contain the required text", run.template),
            evidence,
        }
    }
}

#[derive(Deserialize)]
struct TestSummary {
    passed: u64,
    failed: u64,
}

/// Finds the last JSON object on any line that carries both counts. Extra
/// fields are ignored; prose alone is never a test summary.
fn machine_summary(output: &str) -> Option<TestSummary> {
    output.lines().rev().find_map(|line| {
        let start = line.find('{')?;
        let end = line.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str::<TestSummary>(&line[start..=end]).ok()
    })
}

enum SchemaProblem {
    Invalid(String),
    Unsupported(String),
}

/// A strict, explicit subset of JSON Schema. Anything outside the subset is
/// inconclusive rather than silently ignored.
fn validate(schema: &Value, instance: &Value) -> Result<(), SchemaProblem> {
    let object = schema
        .as_object()
        .ok_or_else(|| SchemaProblem::Invalid("schema is not an object".to_owned()))?;
    for (keyword, rule) in object {
        match keyword.as_str() {
            "type" => {
                let required = type_names(rule)?;
                if !required
                    .iter()
                    .any(|name| instance_has_type(instance, name))
                {
                    return Err(SchemaProblem::Invalid(format!(
                        "expected type {}, found {}",
                        required.join(" or "),
                        instance_type(instance)
                    )));
                }
            }
            "properties" => {
                let properties = rule.as_object().ok_or_else(|| {
                    SchemaProblem::Invalid("properties must be an object".to_owned())
                })?;
                if let Some(map) = instance.as_object() {
                    for (name, property_schema) in properties {
                        if let Some(value) = map.get(name) {
                            validate(property_schema, value)?;
                        }
                    }
                }
            }
            "required" => {
                let names = rule.as_array().ok_or_else(|| {
                    SchemaProblem::Invalid("required must be an array".to_owned())
                })?;
                let map = instance.as_object().ok_or_else(|| {
                    SchemaProblem::Invalid("object schema requires an object".to_owned())
                })?;
                for name in names {
                    let name = name.as_str().ok_or_else(|| {
                        SchemaProblem::Invalid("required entries must be strings".to_owned())
                    })?;
                    if !map.contains_key(name) {
                        return Err(SchemaProblem::Invalid(format!(
                            "missing required property {name:?}"
                        )));
                    }
                }
            }
            "additionalProperties" => {
                let allowed = rule.as_bool().ok_or_else(|| {
                    SchemaProblem::Unsupported(
                        "additionalProperties schemas are not supported".to_owned(),
                    )
                })?;
                if !allowed {
                    let properties = object
                        .get("properties")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    if let Some(map) = instance.as_object() {
                        for name in map.keys() {
                            if !properties.contains_key(name) {
                                return Err(SchemaProblem::Invalid(format!(
                                    "unexpected property {name:?}"
                                )));
                            }
                        }
                    }
                }
            }
            "items" => {
                if !rule.is_object() {
                    return Err(SchemaProblem::Unsupported(
                        "tuple items are not supported".to_owned(),
                    ));
                }
                let values = instance
                    .as_array()
                    .ok_or_else(|| SchemaProblem::Invalid("items requires an array".to_owned()))?;
                for value in values {
                    validate(rule, value)?;
                }
            }
            "enum" => {
                let options = rule
                    .as_array()
                    .ok_or_else(|| SchemaProblem::Invalid("enum must be an array".to_owned()))?;
                if !options.iter().any(|option| option == instance) {
                    return Err(SchemaProblem::Invalid(
                        "value is not one of the allowed options".to_owned(),
                    ));
                }
            }
            "const" => {
                if rule != instance {
                    return Err(SchemaProblem::Invalid(
                        "value does not equal the constant".to_owned(),
                    ));
                }
            }
            "minimum" => {
                let minimum = rule
                    .as_f64()
                    .ok_or_else(|| SchemaProblem::Invalid("minimum must be a number".to_owned()))?;
                if let Some(value) = instance.as_f64() {
                    if value < minimum {
                        return Err(SchemaProblem::Invalid(format!(
                            "{value} is below minimum {minimum}"
                        )));
                    }
                }
            }
            "maximum" => {
                let maximum = rule
                    .as_f64()
                    .ok_or_else(|| SchemaProblem::Invalid("maximum must be a number".to_owned()))?;
                if let Some(value) = instance.as_f64() {
                    if value > maximum {
                        return Err(SchemaProblem::Invalid(format!(
                            "{value} is above maximum {maximum}"
                        )));
                    }
                }
            }
            "minLength" => {
                let minimum = rule.as_u64().ok_or_else(|| {
                    SchemaProblem::Invalid("minLength must be a number".to_owned())
                })?;
                if let Some(value) = instance.as_str() {
                    if value.chars().count() < usize::try_from(minimum).unwrap_or(usize::MAX) {
                        return Err(SchemaProblem::Invalid(format!(
                            "string is shorter than {minimum} characters"
                        )));
                    }
                }
            }
            "maxLength" => {
                let maximum = rule.as_u64().ok_or_else(|| {
                    SchemaProblem::Invalid("maxLength must be a number".to_owned())
                })?;
                if let Some(value) = instance.as_str() {
                    if value.chars().count() > usize::try_from(maximum).unwrap_or(usize::MAX) {
                        return Err(SchemaProblem::Invalid(format!(
                            "string is longer than {maximum} characters"
                        )));
                    }
                }
            }
            "pattern" => {
                let pattern = rule
                    .as_str()
                    .ok_or_else(|| SchemaProblem::Invalid("pattern must be a string".to_owned()))?;
                let regex = Regex::new(pattern).map_err(|problem| {
                    SchemaProblem::Unsupported(format!("invalid pattern: {problem}"))
                })?;
                if let Some(value) = instance.as_str() {
                    if !regex.is_match(value) {
                        return Err(SchemaProblem::Invalid(
                            "string does not match the pattern".to_owned(),
                        ));
                    }
                }
            }
            "minItems" => {
                let minimum = rule.as_u64().ok_or_else(|| {
                    SchemaProblem::Invalid("minItems must be a number".to_owned())
                })?;
                if let Some(values) = instance.as_array() {
                    if values.len() < usize::try_from(minimum).unwrap_or(usize::MAX) {
                        return Err(SchemaProblem::Invalid(format!(
                            "array has fewer than {minimum} items"
                        )));
                    }
                }
            }
            "maxItems" => {
                let maximum = rule.as_u64().ok_or_else(|| {
                    SchemaProblem::Invalid("maxItems must be a number".to_owned())
                })?;
                if let Some(values) = instance.as_array() {
                    if values.len() > usize::try_from(maximum).unwrap_or(usize::MAX) {
                        return Err(SchemaProblem::Invalid(format!(
                            "array has more than {maximum} items"
                        )));
                    }
                }
            }
            other => return Err(SchemaProblem::Unsupported(other.to_owned())),
        }
    }
    Ok(())
}

fn type_names(rule: &Value) -> Result<Vec<String>, SchemaProblem> {
    if let Some(name) = rule.as_str() {
        return Ok(vec![name.to_owned()]);
    }
    let names = rule
        .as_array()
        .ok_or_else(|| SchemaProblem::Invalid("type must be a string or array".to_owned()))?;
    names
        .iter()
        .map(|name| {
            name.as_str()
                .map(str::to_owned)
                .ok_or_else(|| SchemaProblem::Invalid("type names must be strings".to_owned()))
        })
        .collect()
}

fn instance_type(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn instance_has_type(instance: &Value, name: &str) -> bool {
    match name {
        "null" => instance.is_null(),
        "boolean" => instance.is_boolean(),
        "number" => instance.is_number(),
        "integer" => instance.is_i64() || instance.is_u64(),
        "string" => instance.is_string(),
        "array" => instance.is_array(),
        "object" => instance.is_object(),
        _ => false,
    }
}

fn is_filled(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        Some(Value::Bool(_) | Value::Number(_)) => true,
    }
}

fn observe_file(
    spec: &CheckSpec,
    context: &CheckContext,
    raw: &str,
) -> Result<(ContentDigest, Vec<u8>), Box<CheckResult>> {
    let key = normalized(raw);
    let path = match paths::resolve(&context.workspace, raw, Mode::Read) {
        Ok(path) => path,
        Err(problem) => {
            return Err(Box::new(error(
                spec,
                context,
                format!("{key}: {}", describe(problem)),
            )))
        }
    };
    let (digest, bytes) = match ContentDigest::of_file(&path) {
        Ok(observed) => observed,
        Err(problem) => return Err(Box::new(error(spec, context, format!("{key}: {problem}")))),
    };
    if let Some(pinned) = string_arg(spec, "sha256") {
        if digest.sha256 != pinned.to_ascii_lowercase() {
            return Err(Box::new(CheckResult {
                kind: spec.kind.clone(),
                acceptance_hash: context.acceptance_hash.clone(),
                step_index: context.step_index,
                attempt: context.attempt.clone(),
                status: CheckStatus::Fail,
                message: format!("{key} does not match the pinned sha256"),
                evidence: vec![digest_evidence(
                    context,
                    format!("workspace:{key}"),
                    digest,
                    format!("pinned digest was {pinned}"),
                )],
            }));
        }
    }
    Ok((digest, bytes))
}

fn pass(
    spec: &CheckSpec,
    context: &CheckContext,
    message: impl Into<String>,
    evidence: Vec<CheckEvidence>,
) -> CheckResult {
    let bound = !evidence.is_empty()
        && evidence
            .iter()
            .all(|item| item.attempt.as_ref() == Some(&context.attempt));
    if !bound {
        return error(
            spec,
            context,
            "a pass requires non-empty evidence bound to this attempt",
        );
    }
    CheckResult {
        kind: spec.kind.clone(),
        acceptance_hash: context.acceptance_hash.clone(),
        step_index: context.step_index,
        attempt: context.attempt.clone(),
        status: CheckStatus::Pass,
        message: message.into(),
        evidence,
    }
}

fn fail(
    spec: &CheckSpec,
    context: &CheckContext,
    message: impl Into<String>,
    evidence: Vec<CheckEvidence>,
) -> CheckResult {
    CheckResult {
        kind: spec.kind.clone(),
        acceptance_hash: context.acceptance_hash.clone(),
        step_index: context.step_index,
        attempt: context.attempt.clone(),
        status: CheckStatus::Fail,
        message: message.into(),
        evidence,
    }
}

fn error(spec: &CheckSpec, context: &CheckContext, message: impl Into<String>) -> CheckResult {
    CheckResult {
        kind: spec.kind.clone(),
        acceptance_hash: context.acceptance_hash.clone(),
        step_index: context.step_index,
        attempt: context.attempt.clone(),
        status: CheckStatus::Error,
        message: message.into(),
        evidence: Vec::new(),
    }
}

fn digest_evidence(
    context: &CheckContext,
    source: String,
    digest: ContentDigest,
    detail: impl Into<String>,
) -> CheckEvidence {
    CheckEvidence {
        source,
        attempt: Some(context.attempt.clone()),
        digest: Some(digest),
        detail: detail.into(),
        semantic: None,
    }
}

fn workspace_evidence(
    context: &CheckContext,
    key: &str,
    detail: impl Into<String>,
) -> CheckEvidence {
    CheckEvidence {
        source: format!("workspace:{key}"),
        attempt: Some(context.attempt.clone()),
        digest: None,
        detail: detail.into(),
        semantic: None,
    }
}

fn run_evidence(context: &CheckContext, run: &CommandRun) -> CheckEvidence {
    CheckEvidence {
        source: format!("command_run:{}", run.template),
        attempt: Some(context.attempt.clone()),
        digest: None,
        detail: match run.exit_code {
            Some(code) => format!("process exit code {code}"),
            None => "process result has no exit code".to_owned(),
        },
        semantic: None,
    }
}

fn describe(error: PathError) -> String {
    match error {
        PathError::Outside => "path leaves the workspace".to_owned(),
        PathError::Invalid(message) => message,
        PathError::Reparse(path) => {
            format!("path passes through a link or reparse point at {path:?}")
        }
        PathError::NotFound => "path does not exist".to_owned(),
        PathError::NotAFile => "path is not a file".to_owned(),
        PathError::NotADirectory => "path is not a directory".to_owned(),
        PathError::Io(message) => message,
    }
}

fn raw_string<'a>(spec: &'a CheckSpec, key: &str) -> Option<&'a str> {
    spec.args.get(key).and_then(Value::as_str)
}

fn string_arg<'a>(spec: &'a CheckSpec, key: &str) -> Option<&'a str> {
    raw_string(spec, key).filter(|value| !value.trim().is_empty())
}

fn normalized(raw: &str) -> String {
    raw.replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_owned()
}

fn walk(
    root: &Path,
    directory: &Path,
    depth: usize,
    manifest: &mut FileManifest,
) -> io::Result<()> {
    if depth > 16 {
        return Ok(());
    }
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if paths::is_link(&metadata) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if metadata.is_dir() {
            if matches!(name, ".git" | ".svn" | "target" | "node_modules") {
                continue;
            }
            walk(root, &path, depth + 1, manifest)?;
            continue;
        }
        if name.starts_with(".lfm3-") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        if let Ok((digest, _)) = ContentDigest::of_file(&path) {
            manifest.insert(relative, digest);
        }
    }
    Ok(())
}

/// Media type of a manifest entry; kept next to digests so reports can name it.
#[must_use]
pub fn manifest_media_type(path: &str) -> &'static str {
    media_type_for(Path::new(path))
}
