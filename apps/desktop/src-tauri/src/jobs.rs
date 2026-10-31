//! The model-driven job loop behind the desktop command surface.
//!
//! A job is a turn stream, not a frozen plan: the acting model decides each
//! next step from what it actually observed. Every turn is journaled before it
//! reaches the webview, and the only completion proof is the measured
//! deliverable. Jobs never enter the verified run journal.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use lfm3_harness::{
    agent::{AgentLoop, DeliverableProof, LoopStatus, LFM3_TEXT},
    job::{JobDeliverable, JobEvent, JobFinish, JobRecord, JobStart, JobStatus, JobStore, JobView},
    CancellationContext, ModelGateway, ToolRegistry,
};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::engine::EngineError;

/// Live job events. Must match `JOB_EVENT_CHANNEL` in the UI bridge.
pub const JOB_EVENT_CHANNEL: &str = "lfm3:job-event";

/// Receives every journaled job event after it commits.
pub type JobEmitter = Arc<dyn Fn(JobEvent) + Send + Sync>;

/// One submitted job, already validated by the engine.
#[derive(Clone, Debug)]
pub struct JobRequest {
    pub goal: String,
    pub deliverable: Option<String>,
    pub minutes: u32,
    pub max_turns: u32,
}

struct ActiveJob {
    id: String,
    cancel: CancellationContext,
}

struct Inner {
    store: JobStore,
    workspace: PathBuf,
    emitter: Mutex<JobEmitter>,
    active: AsyncMutex<Option<ActiveJob>>,
}

/// Cloneable handle to the one job runtime on this engine.
#[derive(Clone)]
pub struct JobRuntime {
    inner: Arc<Inner>,
}

impl JobRuntime {
    /// Opens the durable job journal and reconciles work the last engine left
    /// running. Nothing is resumed automatically.
    pub fn open(data_dir: &Path, workspace: PathBuf) -> Result<Self, EngineError> {
        let store = JobStore::open(data_dir.join("jobs.db"))?;
        if let Ok(interrupted) = store.reconcile_interrupted() {
            if !interrupted.is_empty() {
                tracing::warn!(
                    jobs = interrupted.len(),
                    "jobs from the previous engine were marked interrupted"
                );
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                workspace,
                emitter: Mutex::new(Arc::new(|_| {})),
                active: AsyncMutex::new(None),
            }),
        })
    }

    pub fn set_emitter(&self, emitter: JobEmitter) {
        if let Ok(mut slot) = self.inner.emitter.lock() {
            *slot = emitter;
        }
    }

    #[must_use]
    pub fn store(&self) -> &JobStore {
        &self.inner.store
    }

    pub fn view(&self, job_id: &str) -> Result<JobView, EngineError> {
        Ok(self.inner.store.view(job_id)?)
    }

    pub fn list(&self, limit: u32) -> Result<Vec<JobRecord>, EngineError> {
        Ok(self.inner.store.list(limit)?)
    }

    pub fn events_after(&self, job_id: &str, after: i64) -> Result<Vec<JobEvent>, EngineError> {
        Ok(self.inner.store.events_after(job_id, after)?)
    }

    /// Rejects a second job before any model tokens are spent.
    pub async fn ensure_idle(&self) -> Result<(), EngineError> {
        if let Some(active) = self.inner.active.lock().await.as_ref() {
            return Err(EngineError::Conflict(format!(
                "job {} is already running on this engine",
                active.id
            )));
        }
        if let Some(left) = self.inner.store.active()? {
            return Err(EngineError::Conflict(format!(
                "job {} was left running by a previous engine; restart to reconcile it",
                left.id
            )));
        }
        Ok(())
    }

    /// Journals the job, then runs the loop in the background. The returned
    /// record is already durable, so the UI can hydrate from the journal even
    /// if it misses a live event.
    pub async fn start(
        &self,
        gateway: Arc<dyn ModelGateway>,
        registry: ToolRegistry,
        model: String,
        request: JobRequest,
    ) -> Result<JobRecord, EngineError> {
        let mut slot = self.inner.active.lock().await;
        if let Some(active) = slot.as_ref() {
            return Err(EngineError::Conflict(format!(
                "job {} is already running on this engine",
                active.id
            )));
        }
        let record = self.inner.store.create(JobStart {
            goal: request.goal.clone(),
            deliverable: request.deliverable.clone(),
            minutes: request.minutes,
            max_turns: request.max_turns,
        })?;
        let cancel = CancellationContext::new(Duration::from_secs(u64::from(request.minutes) * 60));
        *slot = Some(ActiveJob {
            id: record.id.clone(),
            cancel: cancel.clone(),
        });
        drop(slot);

        let started = self.inner.store.append(
            &record.id,
            "job.started",
            &serde_json::json!({
                "goal": request.goal,
                "deliverable": request.deliverable,
                "model": model,
                "minutes": request.minutes,
                "max_turns": request.max_turns,
            }),
        )?;
        self.publish(&started);

        let inner = Arc::clone(&self.inner);
        let id = record.id.clone();
        tokio::spawn(async move {
            inner
                .execute(id, gateway, registry, model, request, cancel)
                .await;
        });
        Ok(record)
    }

    /// Stops a running job. The loop observes the cancellation between turns and
    /// inside every model call; the journal records `cancelled`, never `passed`.
    pub async fn cancel(&self, job_id: &str) -> Result<(), EngineError> {
        let slot = self.inner.active.lock().await;
        let Some(active) = slot.as_ref().filter(|active| active.id == job_id) else {
            return Err(EngineError::Conflict(format!(
                "job {job_id} is not running on this engine"
            )));
        };
        active.cancel.cancel();
        Ok(())
    }

    fn publish(&self, event: &JobEvent) {
        let emitter = match self.inner.emitter.lock() {
            Ok(slot) => Arc::clone(&slot),
            Err(_) => return,
        };
        emitter(event.clone());
    }
}

impl Inner {
    async fn execute(
        &self,
        id: String,
        gateway: Arc<dyn ModelGateway>,
        registry: ToolRegistry,
        model: String,
        request: JobRequest,
        cancel: CancellationContext,
    ) {
        let runner = AgentLoop::new(gateway, Arc::new(registry), model.clone())
            .with_options(LFM3_TEXT.loop_options(request.max_turns))
            .with_max_tokens(LFM3_TEXT.max_output_tokens);

        // The callback runs inside the loop, so it journals each turn before the
        // UI hears about it. A storage failure is logged, not hidden.
        let mut sink = |value: Value| {
            let mut payload = value;
            let kind = payload
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("loop.turn")
                .to_owned();
            if let Some(object) = payload.as_object_mut() {
                object.remove("kind");
            }
            match self.store.append(&id, &kind, &payload) {
                Ok(event) => {
                    let emitter = match self.emitter.lock() {
                        Ok(slot) => Arc::clone(&slot),
                        Err(_) => return,
                    };
                    emitter(event);
                }
                Err(error) => {
                    tracing::warn!(job = %id, %error, "could not journal a job turn");
                }
            }
        };

        let outcome = runner
            .run(
                &request.goal,
                request.deliverable.as_deref(),
                &self.workspace,
                &cancel,
                &mut sink,
            )
            .await;
        let cancelled = cancel.is_cancelled();
        let finish = match outcome {
            Ok(report) => JobFinish {
                status: if report.status == LoopStatus::Passed {
                    JobStatus::Passed
                } else if cancelled {
                    JobStatus::Cancelled
                } else {
                    JobStatus::Failed
                },
                stopped: report.stopped,
                model: report.model,
                proof: report.deliverable.map(proof_of),
            },
            Err(error) => JobFinish {
                status: if cancelled {
                    JobStatus::Cancelled
                } else {
                    JobStatus::Failed
                },
                stopped: format!("the job stopped: {error}"),
                model,
                proof: None,
            },
        };

        match self.store.finish(&id, finish) {
            Ok(event) => {
                let emitter = match self.emitter.lock() {
                    Ok(slot) => Arc::clone(&slot),
                    Err(_) => return,
                };
                emitter(event);
            }
            Err(error) => tracing::error!(job = %id, %error, "could not record the job outcome"),
        }

        let mut slot = self.active.lock().await;
        if slot.as_ref().is_some_and(|active| active.id == id) {
            *slot = None;
        }
    }
}

fn proof_of(proof: DeliverableProof) -> JobDeliverable {
    JobDeliverable {
        path: proof.path,
        bytes: proof.bytes,
        digest: proof.digest,
    }
}
