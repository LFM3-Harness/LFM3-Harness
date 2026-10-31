//! Durable job journal: turn folding, terminal proof, and truthful restart.

use lfm3_harness::job::{JobDeliverable, JobFinish, JobStart, JobStatus, JobStore};
use serde_json::json;

fn store() -> (JobStore, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = JobStore::open(directory.path().join("jobs.db")).expect("job store");
    (store, directory)
}

fn start(store: &JobStore) -> String {
    store
        .create(JobStart {
            goal: "Summarise every note into out/report.md".to_owned(),
            deliverable: Some("out/report.md".to_owned()),
            minutes: 30,
            max_turns: 12,
        })
        .expect("create job")
        .id
}

#[test]
fn turns_and_todos_are_folded_from_the_journal() {
    let (store, _directory) = store();
    let id = start(&store);
    store
        .append(&id, "job.started", &json!({ "goal": "Summarise" }))
        .expect("started");
    store
        .append(
            &id,
            "loop.turn",
            &json!({
                "index": 1,
                "note": "",
                "calls": [{
                    "tool": "todo_write",
                    "arguments": { "items": [{ "text": "read the notes" }] },
                    "ok": true,
                    "output": "[ ] read the notes",
                }],
                "todos": [{ "text": "read the notes", "done": false }],
            }),
        )
        .expect("first turn");
    store
        .append(
            &id,
            "loop.turn",
            &json!({
                "index": 2,
                "note": "",
                "calls": [{
                    "tool": "write_file",
                    "arguments": { "path": "out/report.md" },
                    "ok": true,
                    "output": "wrote 42 bytes",
                }],
                "todos": [
                    { "text": "read the notes", "done": true },
                    { "text": "write the report", "done": true },
                ],
            }),
        )
        .expect("second turn");

    let view = store.view(&id).expect("view");
    assert_eq!(view.turns.len(), 2);
    assert_eq!(view.turns[1].calls[0].tool, "write_file");
    assert_eq!(view.todos.len(), 2);
    assert!(view.todos.iter().all(|todo| todo.done));
    assert_eq!(view.job.status, JobStatus::Running);
}

#[test]
fn finishing_records_status_and_measured_proof_in_one_event() {
    let (store, _directory) = store();
    let id = start(&store);
    let event = store
        .finish(
            &id,
            JobFinish {
                status: JobStatus::Passed,
                stopped: "the required output verified".to_owned(),
                model: "lfm3-1.7b".to_owned(),
                proof: Some(JobDeliverable {
                    path: "out/report.md".to_owned(),
                    bytes: 42,
                    digest: "a".repeat(64),
                }),
            },
        )
        .expect("finish");

    assert_eq!(event.kind, "job.finished");
    assert_eq!(event.seq, 1);
    let view = store.view(&id).expect("view");
    assert_eq!(view.job.status, JobStatus::Passed);
    assert_eq!(view.job.deliverable_proof.expect("proof").bytes, 42);
    assert!(view.job.finished_at.is_some());
    assert!(store.active().expect("active").is_none());
}

#[test]
fn a_restart_marks_running_jobs_interrupted_and_never_resumes_them() {
    let (store, _directory) = store();
    let id = start(&store);
    assert_eq!(
        store.active().expect("active").map(|job| job.id),
        Some(id.clone())
    );

    let interrupted = store.reconcile_interrupted().expect("reconcile");
    assert_eq!(interrupted, vec![id.clone()]);

    let view = store.view(&id).expect("view");
    assert_eq!(view.job.status, JobStatus::Interrupted);
    assert!(view
        .job
        .stopped
        .contains("stopped before this job finished"));
    assert!(store.active().expect("active").is_none());
    let events = store.events_after(&id, 0).expect("events");
    assert!(events.iter().any(|event| event.kind == "job.interrupted"));
    // Re-running reconciliation is a no-op, not a second interruption.
    assert!(store
        .reconcile_interrupted()
        .expect("reconcile again")
        .is_empty());
}

#[test]
fn events_replay_after_a_cursor() {
    let (store, _directory) = store();
    let id = start(&store);
    for index in 1..=3 {
        store
            .append(
                &id,
                "loop.turn",
                &json!({ "index": index, "note": "", "calls": [] }),
            )
            .expect("turn");
    }
    let tail = store.events_after(&id, 2).expect("events");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, 3);
}
