mod support;

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use lfm3_harness::{
    sha256_hex, AttemptId, CancellationContext, CommandArg, CommandTemplate, ContentDigest,
    ModelKey, ModelManager, ModelManagerConfig, ModelSpec, ProcessSupervisor, ToolError, ToolName,
    ToolRegistry, ToolRequest, ToolStatus,
};
use serde_json::json;
use support::{text_reply, FakeConfig, FakeLlamaServer};
use url::Url;

fn registry(root: &Path, granted: &[ToolName]) -> ToolRegistry {
    let mut registry = ToolRegistry::new(root).expect("workspace");
    for tool in granted {
        registry = registry.grant(*tool);
    }
    registry
}

fn temp_workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).expect("workspace");
    (directory, root)
}

fn request(tool: ToolName, arguments: serde_json::Value) -> ToolRequest {
    ToolRequest::new(AttemptId::new(), "call_1", tool, arguments)
}

#[tokio::test]
async fn parent_components_are_rejected_before_any_side_effect() {
    let (directory, root) = temp_workspace();
    let outside = directory.path().join("outside.txt");
    let registry = registry(&root, &[ToolName::WriteFile]);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let error = registry
        .execute(
            request(
                ToolName::WriteFile,
                json!({"path": "../outside.txt", "content": "blocked"}),
            ),
            &cancel.child(),
        )
        .await
        .expect_err("parent traversal must fail");
    assert!(
        matches!(error, ToolError::OutsideWorkspace { .. }),
        "{error:?}"
    );
    assert!(!outside.exists());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
}

#[tokio::test]
async fn absolute_paths_are_rejected() {
    let (directory, root) = temp_workspace();
    let target = directory.path().join("absolute.txt");
    let registry = registry(&root, &[ToolName::WriteFile]);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let error = registry
        .execute(
            request(
                ToolName::WriteFile,
                json!({"path": target.to_str().unwrap(), "content": "blocked"}),
            ),
            &cancel.child(),
        )
        .await
        .expect_err("absolute paths must fail");
    assert!(
        matches!(error, ToolError::OutsideWorkspace { .. }),
        "{error:?}"
    );
    assert!(!target.exists());
}

#[cfg(windows)]
#[tokio::test]
async fn reparse_points_cannot_escape_the_workspace() {
    let (directory, root) = temp_workspace();
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).expect("outside dir");
    let link = root.join("linked");
    let created = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&outside)
        .status();
    if !created.map(|status| status.success()).unwrap_or(false) {
        eprintln!("skipping: this environment cannot create a junction");
        return;
    }

    let registry = registry(&root, &[ToolName::WriteFile]);
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let error = registry
        .execute(
            request(
                ToolName::WriteFile,
                json!({"path": "linked/escaped.txt", "content": "blocked"}),
            ),
            &cancel.child(),
        )
        .await
        .expect_err("a junction must not be traversable");
    assert!(
        matches!(
            error,
            ToolError::ReparsePoint { .. } | ToolError::OutsideWorkspace { .. }
        ),
        "{error:?}"
    );
    assert!(!outside.join("escaped.txt").exists());
}

#[tokio::test]
async fn writes_and_reads_agree_on_content_hashes() {
    let (_directory, root) = temp_workspace();
    let registry = registry(
        &root,
        &[ToolName::WriteFile, ToolName::ReadFile, ToolName::ListFiles],
    );
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let attempt = AttemptId::new();
    let content = "exit=0\nnotes";

    let written = registry
        .execute(
            ToolRequest::new(
                attempt.clone(),
                "call_1",
                ToolName::WriteFile,
                json!({"path": "notes.md", "content": content}),
            ),
            &cancel.child(),
        )
        .await
        .expect("write");
    assert!(written.succeeded());
    assert_eq!(written.exit_code, None);
    assert_eq!(
        written.artifacts,
        vec![ContentDigest::of(content.as_bytes(), "text/plain")]
    );
    assert_eq!(written.artifacts[0].sha256, sha256_hex(content.as_bytes()));

    let read = registry
        .execute(
            ToolRequest::new(
                attempt,
                "call_2",
                ToolName::ReadFile,
                json!({"path": "notes.md"}),
            ),
            &cancel.child(),
        )
        .await
        .expect("read");
    assert_eq!(read.output, content);
    assert_eq!(read.artifacts, written.artifacts);

    let listed = registry
        .execute(request(ToolName::ListFiles, json!({})), &cancel.child())
        .await
        .expect("list");
    assert!(
        listed.output.lines().any(|line| line == "notes.md"),
        "{}",
        listed.output
    );
}

#[tokio::test]
async fn read_output_cannot_masquerade_as_an_exit_code() {
    let (_directory, root) = temp_workspace();
    std::fs::write(root.join("summary.txt"), "exit=0\nall checks passed").unwrap();
    let registry = registry(&root, &[ToolName::ReadFile]);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let result = registry
        .execute(
            request(ToolName::ReadFile, json!({"path": "summary.txt"})),
            &cancel.child(),
        )
        .await
        .expect("read");
    assert_eq!(result.status, ToolStatus::Succeeded);
    assert_eq!(
        result.exit_code, None,
        "only a real process may set an exit code"
    );
    assert_eq!(result.output, "exit=0\nall checks passed");
}

#[tokio::test]
async fn edits_require_a_unique_anchor() {
    let (_directory, root) = temp_workspace();
    std::fs::write(root.join("report.md"), "alpha beta beta").unwrap();
    let registry = registry(&root, &[ToolName::EditFile]);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let error = registry
        .execute(
            request(
                ToolName::EditFile,
                json!({"path": "report.md", "old_text": "beta", "new_text": "gamma"}),
            ),
            &cancel.child(),
        )
        .await
        .expect_err("ambiguous anchor must fail");
    assert!(
        matches!(error, ToolError::AmbiguousEdit { count: 2, .. }),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("report.md")).unwrap(),
        "alpha beta beta"
    );

    let updated = registry
        .execute(
            request(
                ToolName::EditFile,
                json!({"path": "report.md", "old_text": "alpha", "new_text": "gamma"}),
            ),
            &cancel.child(),
        )
        .await
        .expect("unique anchor");
    assert_eq!(updated.artifacts[0].sha256, sha256_hex(b"gamma beta beta"));
    assert_eq!(
        std::fs::read_to_string(root.join("report.md")).unwrap(),
        "gamma beta beta"
    );
}

#[tokio::test]
async fn a_failed_write_leaves_the_target_and_no_temporary_behind() {
    let (_directory, root) = temp_workspace();
    let target = root.join("report.txt");
    std::fs::create_dir(&target).unwrap();
    let registry = registry(&root, &[ToolName::WriteFile]);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let error = registry
        .execute(
            request(
                ToolName::WriteFile,
                json!({"path": "report.txt", "content": "cannot replace a directory"}),
            ),
            &cancel.child(),
        )
        .await
        .expect_err("a directory is not a file");
    assert!(matches!(error, ToolError::NotAFile { .. }), "{error:?}");
    assert!(target.is_dir());
    let stray: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".lfm3-"))
        .collect();
    assert!(stray.is_empty(), "temporary files must be removed");
}

#[tokio::test]
async fn only_granted_tools_execute() {
    let (_directory, root) = temp_workspace();
    let read_only = registry(&root, &[ToolName::ReadFile]);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let error = read_only
        .execute(
            request(
                ToolName::WriteFile,
                json!({"path": "x.txt", "content": "blocked"}),
            ),
            &cancel.child(),
        )
        .await
        .expect_err("write is not granted");
    assert!(
        matches!(
            error,
            ToolError::NotGranted {
                tool: ToolName::WriteFile
            }
        ),
        "{error:?}"
    );
    assert!("delete_everything".parse::<ToolName>().is_err());

    let speak = registry(&root, &[ToolName::Speak]);
    let error = speak
        .execute(
            request(ToolName::Speak, json!({"path": "reply.wav"})),
            &cancel.child(),
        )
        .await
        .expect_err("TTS is not qualified");
    assert!(matches!(error, ToolError::Unsupported { .. }), "{error:?}");
}

#[cfg(windows)]
#[tokio::test]
async fn command_templates_accept_only_declared_slots() {
    let (_directory, root) = temp_workspace();
    std::fs::write(root.join("notes.txt"), "needle in a haystack\n").unwrap();
    let template = CommandTemplate::new(
        "search_notes",
        system32().join("findstr.exe"),
        vec![
            CommandArg::Fixed {
                value: "/C:needle".to_owned(),
            },
            CommandArg::Slot {
                name: "file".to_owned(),
            },
        ],
    )
    .expect("template");
    let mut registry = registry(&root, &[]);
    registry.add_command(template).expect("add template");
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let run = registry
        .run_command(
            "search_notes",
            AttemptId::new(),
            &json!({"file": "notes.txt"}),
            &cancel.child(),
        )
        .await
        .expect("run template");
    assert_eq!(run.status, ToolStatus::Succeeded);
    assert_eq!(run.exit_code, Some(0));
    assert!(run.output.contains("needle"), "{}", run.output);

    for arguments in [
        json!({}),
        json!({"file": "notes.txt", "executable": "C:\\evil.exe"}),
        json!({"file": "notes.txt\n--flag"}),
    ] {
        let error = registry
            .run_command(
                "search_notes",
                AttemptId::new(),
                &arguments,
                &cancel.child(),
            )
            .await
            .expect_err("undeclared or unsafe slots must fail");
        assert!(
            matches!(error, ToolError::InvalidCommandArguments { .. }),
            "{error:?}"
        );
    }

    assert!(
        CommandTemplate::new("bad", "findstr.exe", Vec::new()).is_err(),
        "the executable must be a fixed absolute path"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn cancelling_a_command_terminates_the_owned_child() {
    let (_directory, root) = temp_workspace();
    let pid_file =
        std::env::temp_dir().join(format!("lfm3-tool-cancel-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);
    let script = format!(
        "$PID | Out-File -FilePath '{}' -Encoding ascii; Start-Sleep -Seconds 60",
        pid_file.display()
    );
    let template = CommandTemplate::new(
        "sleepy",
        powershell(),
        vec![
            CommandArg::Fixed {
                value: "-NoProfile".to_owned(),
            },
            CommandArg::Fixed {
                value: "-Command".to_owned(),
            },
            CommandArg::Fixed { value: script },
        ],
    )
    .expect("template");
    let mut registry = registry(&root, &[]);
    registry.add_command(template).expect("add template");

    let context = CancellationContext::new(Duration::from_secs(30));
    let child_cancel = context.child();
    let handle = tokio::spawn(async move {
        registry
            .run_command("sleepy", AttemptId::new(), &json!({}), &child_cancel)
            .await
    });

    let pid = wait_for_pid(&pid_file, Duration::from_secs(10)).await;
    context.cancel();
    let outcome = handle.await.expect("command task");
    assert!(
        matches!(
            outcome,
            Err(ToolError::Cancelled(lfm3_harness::CancelReason::Requested))
        ),
        "{outcome:?}"
    );
    assert!(
        !lfm3_harness::is_process_alive(pid),
        "owned child {pid} must be terminated"
    );
}

#[tokio::test]
async fn transcribe_uses_the_shared_model_manager() {
    let mut config = FakeConfig::chat(&["lfm3-audio-2b"], text_reply("unused"));
    config.transcription = json!({"text": "hello from audio"}).to_string();
    let server = FakeLlamaServer::start(config).await;
    let manager = ModelManager::new(
        ModelManagerConfig {
            total_memory_bytes: 1_000,
            startup_timeout: Duration::from_secs(2),
            health_interval: Duration::from_millis(50),
        },
        vec![ModelSpec::external(
            ModelKey::Audio,
            Url::parse(server.url()).unwrap(),
            "lfm3-audio-2b",
            100,
        )],
        ProcessSupervisor::new().expect("supervisor"),
    )
    .expect("manager");

    let (_directory, root) = temp_workspace();
    let audio = b"RIFF....WAVEfmt ".to_vec();
    std::fs::write(root.join("note.wav"), &audio).unwrap();
    let registry = registry(&root, &[ToolName::Transcribe]).with_models(manager);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let result = registry
        .execute(
            request(
                ToolName::Transcribe,
                json!({"path": "note.wav", "language": "en"}),
            ),
            &cancel.child(),
        )
        .await
        .expect("transcription");
    assert!(
        result
            .output
            .starts_with("harness facts (measured): file=note.wav format=audio/wav"),
        "{}",
        result.output
    );
    assert!(
        result
            .output
            .ends_with("transcript (LFM3-Audio-2B, verbatim; no speaker labels or timestamps):\nhello from audio"),
        "{}",
        result.output
    );
    assert_eq!(result.exit_code, None);
    assert_eq!(
        result.artifacts,
        vec![ContentDigest::of(&audio, "audio/wav")]
    );
}

#[tokio::test]
async fn look_attaches_the_image_to_the_vision_model() {
    let server = FakeLlamaServer::start(FakeConfig::chat(
        &["lfm3-vl-2b"],
        text_reply("A blue Save button is visible."),
    ))
    .await;
    let manager = ModelManager::new(
        ModelManagerConfig {
            total_memory_bytes: 1_000,
            startup_timeout: Duration::from_secs(2),
            health_interval: Duration::from_millis(50),
        },
        vec![ModelSpec::external(
            ModelKey::Vision,
            Url::parse(server.url()).unwrap(),
            "lfm3-vl-2b",
            100,
        )],
        ProcessSupervisor::new().expect("supervisor"),
    )
    .expect("manager");

    let (_directory, root) = temp_workspace();
    let image = b"\x89PNG\r\n\x1a\nfake-pixels".to_vec();
    std::fs::write(root.join("shot.png"), &image).unwrap();
    let registry = registry(&root, &[ToolName::Look]).with_models(manager);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let result = registry
        .execute(
            request(
                ToolName::Look,
                json!({"path": "shot.png", "instruction": "Is there a Save button?"}),
            ),
            &cancel.child(),
        )
        .await
        .expect("look");
    assert!(result.output.contains("Save"), "{}", result.output);
    assert!(
        result
            .output
            .starts_with("harness facts (measured): file=shot.png format=image/png"),
        "{}",
        result.output
    );
    assert_eq!(
        result.artifacts,
        vec![ContentDigest::of(&image, "image/png")]
    );

    let body = server.chat_requests().pop().expect("vision request").body;
    assert!(body.contains("data:image/png;base64,"), "{body}");
}

#[tokio::test]
async fn search_files_matches_names_and_text_inside_the_workspace() {
    let (_directory, root) = temp_workspace();
    std::fs::create_dir_all(root.join("notes")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(
        root.join("notes/budget.md"),
        "# Budget\n\nThe migration plan costs 12 units.\n",
    )
    .unwrap();
    std::fs::write(root.join("notes/roadmap.md"), "Ship the search tool.\n").unwrap();
    std::fs::write(root.join("notes/old-plan.txt"), "migration plan\n").unwrap();
    std::fs::write(root.join("target/ignored.md"), "migration plan\n").unwrap();
    let registry = registry(&root, &[ToolName::SearchFiles, ToolName::RelatedFiles]);
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let result = registry
        .execute(
            request(ToolName::SearchFiles, json!({"query": "migration plan"})),
            &cancel.child(),
        )
        .await
        .expect("search");
    assert!(
        result.output.contains("notes/budget.md"),
        "{}",
        result.output
    );
    assert!(
        !result.output.contains("target/ignored.md"),
        "{}",
        result.output
    );
    assert!(
        !result.output.contains("notes/roadmap.md"),
        "{}",
        result.output
    );

    let by_name = registry
        .execute(
            request(
                ToolName::SearchFiles,
                json!({"query": "roadmap", "mode": "name"}),
            ),
            &cancel.child(),
        )
        .await
        .expect("name search");
    assert!(
        by_name.output.contains("notes/roadmap.md"),
        "{}",
        by_name.output
    );

    let related = registry
        .execute(
            request(ToolName::RelatedFiles, json!({"path": "notes/budget.md"})),
            &cancel.child(),
        )
        .await
        .expect("related");
    assert!(
        related.output.contains("notes/old-plan.txt"),
        "{}",
        related.output
    );
    assert!(
        !related.output.contains("notes/roadmap.md"),
        "{}",
        related.output
    );
}

#[tokio::test]
async fn search_files_rejects_an_escape_before_reading() {
    let (directory, root) = temp_workspace();
    std::fs::write(directory.path().join("outside.md"), "migration plan").unwrap();
    let registry = registry(&root, &[ToolName::SearchFiles]);
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let error = registry
        .execute(
            request(
                ToolName::SearchFiles,
                json!({"query": "migration", "path": ".."}),
            ),
            &cancel.child(),
        )
        .await
        .expect_err("escape");
    assert!(
        matches!(error, ToolError::OutsideWorkspace { .. }),
        "{error:?}"
    );
}

#[cfg(windows)]
fn system32() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"))
        .join("System32")
}

#[cfg(windows)]
fn powershell() -> PathBuf {
    system32().join("WindowsPowerShell\\v1.0\\powershell.exe")
}

#[cfg(windows)]
async fn wait_for_pid(path: &Path, budget: Duration) -> u32 {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("owned command never reported its pid");
}
