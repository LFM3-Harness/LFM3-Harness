use std::{collections::BTreeMap, path::Path, time::Duration};

use lfm3_harness::{
    store::{sha256_hex, TEXT_PLAIN},
    AcceptanceSpec, AttemptStatus, CheckSpec, FinishAttempt, FrozenAcceptance, Reconciliation,
    RunId, RunStatus, StartAttempt, StepStatus, Store,
};
use serde_json::json;
use tempfile::TempDir;

fn acceptance() -> FrozenAcceptance {
    AcceptanceSpec {
        schema_version: 1,
        mission: "desk_task".to_owned(),
        mission_version: 1,
        goal: "Update the workspace notes".to_owned(),
        inputs: BTreeMap::new(),
        steps: vec![lfm3_harness::PlannedStep {
            index: 1,
            title: "Write the notes".to_owned(),
            criterion: "notes.md exists and contains the requested text".to_owned(),
            checks: vec![CheckSpec::new("file_exists", [("path", json!("notes.md"))])],
            tool: Some("write_file".to_owned()),
        }],
        output: Default::default(),
        allowed_tools: ["write_file".to_owned()].into_iter().collect(),
        allowed_checks: ["file_exists".to_owned()].into_iter().collect(),
        max_steps: 8,
        max_attempts: 3,
    }
    .freeze()
    .expect("fixture acceptance is valid")
}

async fn open_store() -> (TempDir, Store) {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::open(directory.path().join("runs.db"))
        .await
        .expect("store opens");
    (directory, store)
}

fn start(run_id: &RunId) -> StartAttempt {
    StartAttempt {
        run_id: run_id.clone(),
        step_index: 1,
        strategy: 1,
        action: json!({"tool": "write_file", "path": "notes.md"}),
        model: "lfm3-1.7b".to_owned(),
        deadline_at: None,
    }
}

async fn running_attempt(store: &Store) -> (RunId, lfm3_harness::AttemptId) {
    let acceptance = acceptance();
    let run = store.create_run(&acceptance, None).await.expect("run");
    store
        .persist_pending_steps(&run.id, &acceptance.spec().steps)
        .await
        .expect("steps");
    let attempt = store.start_attempt(start(&run.id)).await.expect("attempt");
    (run.id, attempt)
}

async fn finished_attempt_fixture(store: &Store) -> (RunId, lfm3_harness::AttemptId) {
    let (run_id, attempt_id) = running_attempt(store).await;
    store
        .finish_attempt(FinishAttempt {
            run_id: run_id.clone(),
            attempt_id: attempt_id.clone(),
            status: AttemptStatus::Passed,
            error: None,
        })
        .await
        .expect("finish");
    (run_id, attempt_id)
}

#[tokio::test]
async fn attempt_result_and_event_share_one_transaction() {
    let (_directory, store) = open_store().await;
    let (run_id, attempt_id) = running_attempt(&store).await;

    let snapshot = store.snapshot(&run_id).await.unwrap();
    assert_eq!(snapshot.run.status, RunStatus::Running);
    assert_eq!(snapshot.steps[0].status, StepStatus::Running);
    assert_eq!(
        snapshot.steps[0].attempts[0].status,
        AttemptStatus::Executing
    );

    let finished = store
        .finish_attempt(FinishAttempt {
            run_id: run_id.clone(),
            attempt_id,
            status: AttemptStatus::Passed,
            error: None,
        })
        .await
        .unwrap();
    assert_eq!(finished.step_status, StepStatus::Passed);

    let snapshot = store.snapshot(&run_id).await.unwrap();
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Passed);
    assert_eq!(snapshot.steps[0].status, StepStatus::Passed);

    let events = store.events_after(&run_id, 0).await.unwrap();
    assert!(events.iter().any(|event| event.kind == "step.passed"));
}

#[tokio::test]
async fn events_are_ordered_committed_and_replayable() {
    let (_directory, store) = open_store().await;
    let mut live = store.subscribe();
    let (run_id, attempt_id) = running_attempt(&store).await;

    store
        .finish_attempt(FinishAttempt {
            run_id: run_id.clone(),
            attempt_id,
            status: AttemptStatus::Passed,
            error: None,
        })
        .await
        .unwrap();

    let live_passed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("broadcast stays open");
            if event.kind == "step.passed" {
                return event;
            }
        }
    })
    .await
    .expect("live event arrives");
    assert_eq!(live_passed.run_id, run_id);

    let events = store.events_after(&run_id, 0).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "run.created",
            "run.status",
            "run.planned",
            "attempt.started",
            "step.passed"
        ]
    );
    let sequences: Vec<i64> = events.iter().map(|event| event.seq).collect();
    assert_eq!(sequences, vec![1, 2, 3, 4, 5]);
    assert_eq!(live_passed.seq, 5);

    assert!(store
        .events_after(&run_id, sequences[4])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn snapshots_keep_every_pending_step_in_order() {
    let (_directory, store) = open_store().await;
    let acceptance = acceptance();
    let run = store.create_run(&acceptance, None).await.unwrap();
    let steps = vec![
        acceptance.spec().steps[0].clone(),
        lfm3_harness::PlannedStep {
            index: 2,
            title: "Check the notes".to_owned(),
            criterion: "the notes file still matches the requested schema".to_owned(),
            checks: acceptance.spec().steps[0].checks.clone(),
            tool: None,
        },
    ];
    store.persist_pending_steps(&run.id, &steps).await.unwrap();

    let snapshot = store.snapshot(&run.id).await.unwrap();
    assert_eq!(snapshot.steps.len(), 2);
    assert_eq!(snapshot.steps[0].index, 1);
    assert_eq!(snapshot.steps[1].index, 2);
    assert!(snapshot
        .steps
        .iter()
        .all(|step| step.status == StepStatus::Pending));
}

#[tokio::test]
async fn restart_reconciles_unfinished_state_without_replay() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("runs.db");
    let (run_id, attempt_id) = {
        let store = Store::open(&database).await.unwrap();
        running_attempt(&store).await
    };

    let reopened = Store::open(&database).await.unwrap();
    assert_eq!(
        reopened.reconcile_interrupted().await.unwrap(),
        Reconciliation {
            runs: 1,
            steps: 1,
            attempts: 1,
        }
    );

    let snapshot = reopened.snapshot(&run_id).await.unwrap();
    assert_eq!(snapshot.run.status, RunStatus::Interrupted);
    assert_eq!(snapshot.steps[0].status, StepStatus::Blocked);
    assert_eq!(
        snapshot.steps[0].attempts[0].status,
        AttemptStatus::Interrupted
    );
    assert_eq!(snapshot.steps[0].attempts[0].id, attempt_id);

    let events = reopened.events_after(&run_id, 0).await.unwrap();
    assert!(events.iter().any(|event| event.kind == "run.interrupted"));
    assert_eq!(
        reopened.reconcile_interrupted().await.unwrap(),
        Reconciliation::default()
    );
    assert!(
        reopened.start_attempt(start(&run_id)).await.is_err(),
        "a terminal run must not accept new attempts"
    );

    let fresh = reopened.create_run(&acceptance(), None).await.unwrap();
    assert_eq!(fresh.status, RunStatus::Created);
}

#[tokio::test]
async fn migrations_are_repeatable_and_preserve_history() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("runs.db");
    let run_id = {
        let store = Store::open(&database).await.unwrap();
        assert_eq!(store.schema_version(), 1);
        let run = store.create_run(&acceptance(), None).await.unwrap();
        store
            .persist_pending_steps(&run.id, &acceptance().spec().steps)
            .await
            .unwrap();
        run.id
    };

    let reopened = Store::open(&database).await.unwrap();
    assert_eq!(reopened.schema_version(), 1);
    let snapshot = reopened.snapshot(&run_id).await.unwrap();
    assert_eq!(snapshot.steps.len(), 1);
    assert_eq!(
        reopened.events_after(&run_id, 0).await.unwrap().len(),
        3,
        "existing journal survives a reopen"
    );
}

#[tokio::test]
async fn finished_attempts_and_passed_steps_cannot_be_reused() {
    let (_directory, store) = open_store().await;
    let (run_id, attempt_id) = finished_attempt_fixture(&store).await;

    assert!(store
        .finish_attempt(FinishAttempt {
            run_id: run_id.clone(),
            attempt_id,
            status: AttemptStatus::Passed,
            error: None,
        })
        .await
        .is_err());
    assert!(store.start_attempt(start(&run_id)).await.is_err());
}

#[tokio::test]
async fn artifacts_are_deduplicated_and_byte_exact() {
    let (_directory, store) = open_store().await;
    let first = store
        .write_artifact(b"report body", TEXT_PLAIN)
        .await
        .unwrap();
    let second = store
        .write_artifact(b"report body", "text/plain")
        .await
        .unwrap();

    assert_eq!(first.id, second.id);
    assert_eq!(first.sha256, sha256_hex(b"report body"));
    assert_eq!(first.size, 11);
    assert_eq!(
        store.read_artifact(&first.id).await.unwrap(),
        b"report body"
    );
    assert_eq!(count_files(store.artifacts_path()), 1);

    let other = store.write_artifact(b"\x00\xffbinary", "application/octet-stream");
    let other = other.await.unwrap();
    assert_ne!(other.id, first.id);
    assert_eq!(
        store.read_artifact(&other.id).await.unwrap(),
        b"\x00\xffbinary"
    );
    assert_eq!(count_files(store.artifacts_path()), 2);
}

#[tokio::test]
async fn workspace_writes_cannot_alter_trusted_state() {
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open(trusted.path().join("runs.db")).await.unwrap();
    let (run_id, _attempt) = running_attempt(&store).await;

    std::fs::write(workspace.path().join("runs.db"), b"decoy").unwrap();
    std::fs::write(workspace.path().join("engine.lock"), b"decoy").unwrap();
    std::fs::create_dir_all(workspace.path().join("artifacts")).unwrap();
    std::fs::write(workspace.path().join("artifacts").join("decoy"), b"decoy").unwrap();

    assert!(store.database_path().starts_with(trusted.path()));
    assert!(store.artifacts_path().starts_with(trusted.path()));
    assert!(!store.database_path().starts_with(workspace.path()));
    assert!(!store.artifacts_path().starts_with(workspace.path()));
    assert_eq!(
        store.events_after(&run_id, 0).await.unwrap().len(),
        4,
        "trusted journal is untouched by workspace writes"
    );
}

fn count_files(root: &Path) -> usize {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += count_files(&path);
        } else {
            total += 1;
        }
    }
    total
}
