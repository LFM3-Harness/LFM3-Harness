//! Deterministic checks must never pass on words alone.
//!
//! Every passing result carries same-attempt evidence whose digests are
//! recomputed by the verifier. Spoofed text, vacuous specs, missing files,
//! stale hashes, and failed test summaries stay Fail or Error.

use std::{fs, path::PathBuf, time::Duration};

use lfm3_harness::{
    mission::CheckSpec,
    store::ArtifactStore,
    tools::{CommandRun, ToolName, ToolResult, ToolStatus},
    verify::{
        evaluate, media_type_for, CheckContext, CheckError, CheckStatus, ContentDigest,
        FileManifest,
    },
    AttemptId,
};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    root: PathBuf,
    attempt: AttemptId,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp workspace");
        let root = fs::canonicalize(directory.path()).expect("canonical workspace");
        Self {
            directory,
            root,
            attempt: AttemptId::new(),
        }
    }

    fn write(&self, relative: &str, content: &str) -> ContentDigest {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create directory");
        }
        fs::write(&path, content).expect("write fixture");
        ContentDigest::of(content.as_bytes(), media_type_for(&path))
    }

    fn context(&self) -> CheckContext {
        CheckContext::new("a".repeat(64), 1, self.attempt.clone(), self.root.clone())
    }

    fn write_result(&self, relative: &str, digest: ContentDigest) -> ToolResult {
        ToolResult {
            attempt: self.attempt.clone(),
            call_id: format!("write-{relative}"),
            tool: ToolName::WriteFile,
            status: ToolStatus::Succeeded,
            exit_code: None,
            output: format!("wrote {relative}"),
            artifacts: vec![digest],
            duration: Duration::ZERO,
        }
    }

    fn run(&self, template: &str, exit_code: Option<i32>, output: &str) -> CommandRun {
        CommandRun {
            attempt: self.attempt.clone(),
            template: template.to_owned(),
            status: if exit_code == Some(0) {
                ToolStatus::Succeeded
            } else {
                ToolStatus::Failed
            },
            exit_code,
            output: output.to_owned(),
            duration: Duration::ZERO,
        }
    }

    fn journal_path(&self) -> PathBuf {
        self.directory.path().to_path_buf()
    }
}

fn check(kind: &str, args: Vec<(&str, Value)>) -> CheckSpec {
    CheckSpec::new(kind, args)
}

fn command_exit_spec(code: i64) -> CheckSpec {
    check(
        "command_exit",
        vec![
            ("template", json!("workspace_tests")),
            ("exit_code", json!(code)),
        ],
    )
}

#[tokio::test]
async fn command_exit_requires_same_attempt_command_result() {
    let context = CheckContext::fixture_with_observation("exit=0");
    let result = evaluate(command_exit_spec(0), context).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.evidence.len(), 0);
}

#[tokio::test]
async fn command_exit_rejects_another_attempts_run() {
    let fixture = Fixture::new();
    let mut run = fixture.run("workspace_tests", Some(0), "");
    run.attempt = AttemptId::new();
    let context = fixture.context().with_command_run(run);
    let result = evaluate(command_exit_spec(0), context).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.evidence.is_empty());
}

#[tokio::test]
async fn command_exit_distinguishes_real_exit_codes() {
    let fixture = Fixture::new();
    let passing =
        fixture
            .context()
            .with_command_run(fixture.run("workspace_tests", Some(0), "all good"));
    let result = evaluate(command_exit_spec(0), passing).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert_eq!(result.evidence.len(), 1);
    assert_eq!(result.evidence[0].source, "command_run:workspace_tests");
    assert_eq!(result.acceptance_hash, "a".repeat(64));

    let failing =
        fixture
            .context()
            .with_command_run(fixture.run("workspace_tests", Some(1), "boom"));
    let result = evaluate(command_exit_spec(0), failing).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(!result.evidence.is_empty());
}

#[tokio::test]
async fn content_matches_without_a_condition_is_inconclusive() {
    let fixture = Fixture::new();
    fixture.write("report.txt", "anything");
    let spec = check("content_matches", vec![("path", json!("report.txt"))]);
    let result = evaluate(spec, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.evidence.is_empty());
}

#[tokio::test]
async fn missing_file_fails_existence_and_errors_on_content() {
    let fixture = Fixture::new();
    let exists = check("file_exists", vec![("path", json!("missing.txt"))]);
    let result = evaluate(exists, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(!result.evidence.is_empty());

    let content = check(
        "content_matches",
        vec![("path", json!("missing.txt")), ("contains", json!("x"))],
    );
    let result = evaluate(content, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
}

#[tokio::test]
async fn content_matches_checks_every_declared_condition() {
    let fixture = Fixture::new();
    let digest = fixture.write("notes.txt", "alpha beta gamma");
    let passing = check(
        "content_matches",
        vec![
            ("path", json!("notes.txt")),
            ("contains", json!("beta")),
            ("all_of", json!(["alpha", "gamma"])),
            ("regex", json!("^alpha .* gamma$")),
        ],
    );
    let result = evaluate(passing, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert_eq!(result.evidence[0].digest.as_ref(), Some(&digest));

    let failing = check(
        "content_matches",
        vec![
            ("path", json!("notes.txt")),
            ("all_of", json!(["alpha", "delta"])),
        ],
    );
    let result = evaluate(failing, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(!result.evidence.is_empty());
}

#[tokio::test]
async fn content_absent_rejects_invented_terms_and_requires_a_list() {
    let fixture = Fixture::new();
    let digest = fixture.write("summary.md", "The lamp was repaired.\n");
    let passing = check(
        "content_absent",
        vec![
            ("path", json!("summary.md")),
            ("forbidden", json!(["dragon", "teleported"])),
        ],
    );
    let result = evaluate(passing, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert_eq!(result.evidence[0].digest.as_ref(), Some(&digest));

    let failing = check(
        "content_absent",
        vec![
            ("path", json!("summary.md")),
            ("forbidden", json!(["repaired", "dragon"])),
        ],
    );
    let result = evaluate(failing, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("repaired"));

    let empty = check(
        "content_absent",
        vec![("path", json!("summary.md")), ("forbidden", json!([]))],
    );
    let result = evaluate(empty, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.evidence.is_empty());
}

#[tokio::test]
async fn paths_outside_the_workspace_are_inconclusive() {
    let fixture = Fixture::new();
    let spec = check(
        "content_matches",
        vec![("path", json!("../escape.txt")), ("contains", json!("x"))],
    );
    let result = evaluate(spec, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.evidence.is_empty());
}

#[tokio::test]
async fn json_schema_accepts_valid_reports_and_rejects_the_wrong_artifact() {
    let fixture = Fixture::new();
    let good = fixture.write("out/report.json", r#"{"total":5,"name":"ok"}"#);
    let other = fixture.write("out/other.json", r#"{"total":6,"name":"no"}"#);
    let schema = json!({
        "type": "object",
        "required": ["total", "name"],
        "additionalProperties": false,
        "properties": {
            "total": {"type": "integer", "minimum": 0},
            "name": {"type": "string", "minLength": 2}
        }
    });

    let passing = check(
        "json_schema",
        vec![
            ("path", json!("out/report.json")),
            ("schema", schema.clone()),
            ("sha256", json!(good.sha256)),
        ],
    );
    let result = evaluate(passing, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert_eq!(result.evidence[0].digest.as_ref(), Some(&good));

    let wrong = check(
        "json_schema",
        vec![
            ("path", json!("out/report.json")),
            ("schema", schema.clone()),
            ("sha256", json!(other.sha256)),
        ],
    );
    let result = evaluate(wrong, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(!result.evidence.is_empty());

    let invalid = fixture.write("out/invalid.json", r#"{"total":-1,"name":"x"}"#);
    let spec = check(
        "json_schema",
        vec![("path", json!("out/invalid.json")), ("schema", schema)],
    );
    let result = evaluate(spec, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result
        .evidence
        .iter()
        .any(|item| item.digest.as_ref() == Some(&invalid)));
}

#[tokio::test]
async fn json_schema_requires_an_explicit_subset() {
    let fixture = Fixture::new();
    fixture.write("out/report.json", r#"{"total":5}"#);
    let spec = check(
        "json_schema",
        vec![
            ("path", json!("out/report.json")),
            ("schema", json!({"oneOf": [{"type": "object"}]})),
        ],
    );
    let result = evaluate(spec, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
}

#[tokio::test]
async fn file_changed_requires_a_before_digest_and_same_attempt_artifact() {
    let fixture = Fixture::new();
    let before = fixture.write("notes.txt", "before");
    let spec = |path: &str| check("file_changed", vec![("path", json!(path))]);

    let unchanged = fixture.context().with_before("notes.txt", before.clone());
    let result = evaluate(spec("notes.txt"), unchanged).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);

    let after = fixture.write("notes.txt", "after");
    let no_artifact = fixture.context().with_before("notes.txt", before.clone());
    let result = evaluate(spec("notes.txt"), no_artifact).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);

    let with_artifact = fixture
        .context()
        .with_before("notes.txt", before)
        .with_tool_result(fixture.write_result("notes.txt", after.clone()));
    let result = evaluate(spec("notes.txt"), with_artifact).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert_eq!(result.evidence[0].digest.as_ref(), Some(&after));

    let unknown_before = fixture.context();
    let result = evaluate(spec("notes.txt"), unknown_before).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
}

#[tokio::test]
async fn file_changed_verifies_artifact_bytes_in_the_store() {
    let fixture = Fixture::new();
    let before = fixture.write("notes.txt", "before");
    let after = fixture.write("notes.txt", "after");
    let store = ArtifactStore::new(fixture.journal_path().join("artifacts"));
    store
        .write(&after.sha256, b"after")
        .expect("store artifact");
    let context = fixture
        .context()
        .with_before("notes.txt", before.clone())
        .with_tool_result(fixture.write_result("notes.txt", after.clone()))
        .with_artifacts(store);
    let spec = check("file_changed", vec![("path", json!("notes.txt"))]);
    let result = evaluate(spec.clone(), context).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);

    let empty_store = ArtifactStore::new(fixture.journal_path().join("empty-store"));
    let context = fixture
        .context()
        .with_before("notes.txt", before.clone())
        .with_tool_result(fixture.write_result("notes.txt", after.clone()))
        .with_artifacts(empty_store);
    let result = evaluate(spec, context).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
}

#[tokio::test]
async fn tests_pass_requires_a_machine_readable_summary() {
    let fixture = Fixture::new();
    let spec = check("tests_pass", vec![("template", json!("workspace_tests"))]);

    let prose = fixture.context().with_command_run(fixture.run(
        "workspace_tests",
        Some(0),
        "everything works",
    ));
    let result = evaluate(spec.clone(), prose).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);

    let failed = fixture.context().with_command_run(fixture.run(
        "workspace_tests",
        Some(0),
        r#"{"passed":1,"failed":2}"#,
    ));
    let result = evaluate(spec.clone(), failed).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);

    let nonzero_exit = fixture.context().with_command_run(fixture.run(
        "workspace_tests",
        Some(1),
        r#"{"passed":3,"failed":0}"#,
    ));
    let result = evaluate(spec.clone(), nonzero_exit).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);

    let zero_tests = fixture.context().with_command_run(fixture.run(
        "workspace_tests",
        Some(0),
        r#"{"passed":0,"failed":0}"#,
    ));
    let result = evaluate(spec.clone(), zero_tests).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);

    let passing = fixture.context().with_command_run(fixture.run(
        "workspace_tests",
        Some(0),
        "running 3 tests\n{\"passed\":3,\"failed\":0}",
    ));
    let result = evaluate(spec, passing).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(!result.evidence.is_empty());
}

#[tokio::test]
async fn numeric_consistency_detects_incorrect_totals() {
    let fixture = Fixture::new();
    let good = fixture.write("out/summary.json", r#"{"total":9,"home":4,"away":5}"#);
    let spec = check(
        "numeric_consistency",
        vec![
            ("path", json!("out/summary.json")),
            ("total", json!("total")),
            ("sum", json!(["home", "away"])),
        ],
    );
    let result = evaluate(spec.clone(), fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert_eq!(result.evidence[0].digest.as_ref(), Some(&good));

    let bad = fixture.write("out/summary.json", r#"{"total":10,"home":4,"away":5}"#);
    let result = evaluate(spec, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
    assert_eq!(result.evidence[0].digest.as_ref(), Some(&bad));

    fixture.write("out/summary.json", r#"{"total":10,"home":4}"#);
    let spec = check(
        "numeric_consistency",
        vec![
            ("path", json!("out/summary.json")),
            ("total", json!("total")),
            ("sum", json!(["home", "away"])),
        ],
    );
    let result = evaluate(spec, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.evidence.is_empty());
}

#[tokio::test]
async fn report_complete_requires_filled_fields() {
    let fixture = Fixture::new();
    fixture.write(
        "out/report.json",
        r#"{"summary":"done","count":3,"notes":[]}"#,
    );
    let spec = check(
        "report_complete",
        vec![("fields", json!(["summary", "count"]))],
    );
    let result = evaluate(spec, fixture.context().with_report_path("out/report.json"))
        .await
        .unwrap();
    assert_eq!(result.status, CheckStatus::Pass);

    let spec = check(
        "report_complete",
        vec![("fields", json!(["summary", "notes"]))],
    );
    let result = evaluate(spec, fixture.context().with_report_path("out/report.json"))
        .await
        .unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(!result.evidence.is_empty());
}

#[tokio::test]
async fn stdout_contains_uses_real_process_output() {
    let fixture = Fixture::new();
    let spec = check(
        "stdout_contains",
        vec![("template", json!("inspect")), ("contains", json!("ready"))],
    );
    let result = evaluate(spec.clone(), fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);

    let passing =
        fixture
            .context()
            .with_command_run(fixture.run("inspect", Some(0), "all systems ready"));
    let result = evaluate(spec.clone(), passing).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);

    let failing =
        fixture
            .context()
            .with_command_run(fixture.run("inspect", Some(0), "still warming"));
    let result = evaluate(spec, failing).await.unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
}

#[tokio::test]
async fn transcript_nonempty_uses_the_typed_transcription() {
    let fixture = Fixture::new();
    let audio = ContentDigest::of(b"RIFF....", "audio/wav");
    let spec = check("transcript_nonempty", vec![("min_words", json!(2))]);

    let result = evaluate(spec.clone(), fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);

    let transcription = ToolResult {
        attempt: fixture.attempt.clone(),
        call_id: "call-audio".to_owned(),
        tool: ToolName::Transcribe,
        status: ToolStatus::Succeeded,
        exit_code: None,
        output: "two words".to_owned(),
        artifacts: vec![audio.clone()],
        duration: Duration::ZERO,
    };
    let result = evaluate(
        spec.clone(),
        fixture.context().with_tool_result(transcription),
    )
    .await
    .unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert_eq!(result.evidence[0].digest.as_ref(), Some(&audio));

    let short = ToolResult {
        attempt: fixture.attempt.clone(),
        call_id: "call-audio".to_owned(),
        tool: ToolName::Transcribe,
        status: ToolStatus::Succeeded,
        exit_code: None,
        output: "word".to_owned(),
        artifacts: vec![audio],
        duration: Duration::ZERO,
    };
    let result = evaluate(spec, fixture.context().with_tool_result(short))
        .await
        .unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
}

#[tokio::test]
async fn existence_and_directory_checks_observe_the_workspace() {
    let fixture = Fixture::new();
    let digest = fixture.write("out/report.json", "{}");
    fs::create_dir_all(fixture.root.join("out/empty")).expect("create directory");

    let file = check("file_exists", vec![("path", json!("out/report.json"))]);
    let result = evaluate(file, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);
    assert_eq!(result.evidence[0].digest.as_ref(), Some(&digest));

    let directory = check("dir_exists", vec![("path", json!("out/empty"))]);
    let result = evaluate(directory, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Pass);

    let file_as_directory = check("dir_exists", vec![("path", json!("out/report.json"))]);
    let result = evaluate(file_as_directory, fixture.context())
        .await
        .unwrap();
    assert_eq!(result.status, CheckStatus::Fail);

    let directory_as_file = check("file_exists", vec![("path", json!("out/empty"))]);
    let result = evaluate(directory_as_file, fixture.context())
        .await
        .unwrap();
    assert_eq!(result.status, CheckStatus::Fail);
}

#[tokio::test]
async fn semantic_and_unknown_checks_never_pass() {
    let fixture = Fixture::new();
    let semantic = check("vl_criterion", vec![("criterion", json!("looks correct"))]);
    let result = evaluate(semantic, fixture.context()).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.evidence.is_empty());

    let unknown = check("made_up", vec![]);
    let problem = evaluate(unknown, fixture.context()).await.unwrap_err();
    assert!(matches!(problem, CheckError::UnknownKind(_)));
}

#[tokio::test]
async fn manifests_capture_workspace_bytes() {
    let fixture = Fixture::new();
    let digest = fixture.write("out/report.json", "{}");
    fs::create_dir_all(fixture.root.join("target")).expect("create target");
    fs::write(fixture.root.join("target/ignored.txt"), "ignored").expect("write ignored");

    let manifest = FileManifest::capture(&fixture.root).expect("capture manifest");
    assert_eq!(manifest.get("out/report.json"), Some(&digest));
    assert!(manifest.get("target/ignored.txt").is_none());
    assert!(!manifest.is_empty());
}
