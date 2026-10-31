//! One embedded harness instance behind the desktop command layer.
//!
//! The engine owns the durable store, file-backed acceptance contracts, the
//! model scheduler, and the run coordinator. Commands validate state here;
//! Tauri only transports typed values and committed journal events.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use lfm3_harness::{
    agent::{plan_request, Budget, Executor, Planner, Runner, SemanticVerifier},
    mission::{load_missions, InputArtifact, PlanError},
    model::{
        ChatRequest, ChatResponse, Decision, DecisionRequest, LaunchSpec, LlamaClient, ModelFuture,
        ModelKey, ModelLease, ModelManager, ModelManagerConfig, ModelSpec, TranscribeRequest,
        Transcript,
    },
    verify::ModelJudge,
    AcceptanceSpec, ArtifactId, CancellationContext, Config, MissionManifest, ModelError,
    ModelGateway, RunId, RunRecord, RunStatus, Snapshot, Store, ToolName, ToolRegistry,
};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    chat::{ChatRuntime, SpeechConfig},
    job_windows::ApplicationJob,
    jobs::{JobRequest, JobRuntime},
    setup::{self, SetupSettings},
};

const MODEL_MEMORY_HEADROOM: u64 = 512 * 1024 * 1024;
const MANAGER_MEMORY_HEADROOM: u64 = 2 * 1024 * 1024 * 1024;
const APPROVAL_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Harness(#[from] lfm3_harness::HarnessError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Tool(#[from] lfm3_harness::ToolError),
    #[error("{0}")]
    Setup(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Process(String),
}

/// One owned text-model lease held for the lifetime of an approved run.
struct ActiveRun {
    run_id: RunId,
    cancel: CancellationContext,
    gateway: Arc<dyn ModelGateway>,
}

struct PendingApproval {
    run_id: RunId,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
pub struct LlamaRuntime {
    pub executable: Option<PathBuf>,
    pub build: u32,
    pub release: String,
    pub pinned: bool,
    pub error: Option<String>,
}

/// The single engine instance the desktop app embeds.
pub struct Engine {
    config: Config,
    missions: BTreeMap<String, MissionManifest>,
    workspace: PathBuf,
    store: Store,
    coordinator: lfm3_harness::RunCoordinator,
    models: Option<ModelManager>,
    text_memory: u64,
    llama: LlamaRuntime,
    setup: SetupSettings,
    models_dir: Option<PathBuf>,
    speech: SpeechConfig,
    job: ApplicationJob,
    chat: ChatRuntime,
    jobs: JobRuntime,
    active: Arc<Mutex<Option<ActiveRun>>>,
    starting: Mutex<()>,
    approvals: Mutex<HashMap<String, PendingApproval>>,
}

impl Engine {
    /// Opens durable state, reconciles interrupted work, and loads missions.
    pub async fn boot(missions_dir: &Path) -> Result<Self, EngineError> {
        let mut config = Config::load()?;
        let settings = SetupSettings::load(&config.data_dir)?;
        apply_settings(&mut config, &settings);
        config.validate_local(config.profile)?;
        let workspace = config
            .workspace
            .clone()
            .unwrap_or_else(|| config.data_dir.join("workspace"));
        fs::create_dir_all(&workspace).map_err(|error| {
            EngineError::Setup(format!("create workspace {}: {error}", workspace.display()))
        })?;
        let store = Store::open(config.data_dir.join("runs.db")).await?;
        store.reconcile_interrupted().await?;
        let missions = load_missions(missions_dir)?;
        let job = ApplicationJob::new()?;
        Self::assemble(config, missions, workspace, store, job, settings)
    }

    /// Builds an engine around already-open dependencies. Used by boot and by
    /// command-state tests that never start a model.
    pub fn assemble(
        config: Config,
        missions: BTreeMap<String, MissionManifest>,
        workspace: PathBuf,
        store: Store,
        job: ApplicationJob,
        setup: SetupSettings,
    ) -> Result<Self, EngineError> {
        let models_dir = setup.models_dir.clone().or_else(setup::find_models_dir);
        let weight = models_dir
            .as_ref()
            .and_then(|root| setup::find_weight(root, "LFM3-1.7B"));
        let audio_weight = models_dir
            .as_ref()
            .and_then(|root| setup::find_weight(root, "LFM3-Audio-2B"));
        let audio_projector = models_dir
            .as_ref()
            .and_then(|root| setup::find_projector_for(root, "LFM3-Audio-2B"));
        let vision_weight = models_dir
            .as_ref()
            .and_then(|root| setup::find_weight(root, "LFM3-VL-2B"));
        let vision_projector = models_dir
            .as_ref()
            .and_then(|root| setup::find_projector_for(root, "LFM3-VL-2B"));
        let d1_weight = models_dir
            .as_ref()
            .and_then(|root| setup::find_weight(root, "d1-2B"));
        let llama = probe_runtime(&config);
        let models = build_model_manager(
            &config,
            &llama,
            weight.as_deref(),
            audio_weight.as_deref(),
            audio_projector.as_deref(),
            vision_weight.as_deref(),
            vision_projector.as_deref(),
            d1_weight.as_deref(),
            &job,
        );
        let text_memory = weight
            .as_ref()
            .and_then(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.len().saturating_add(MODEL_MEMORY_HEADROOM))
            .unwrap_or(0);
        let coordinator = lfm3_harness::RunCoordinator::new(store.clone());
        let speech = SpeechConfig {
            executable: config
                .llama
                .tts
                .clone()
                .filter(|path| path.is_file())
                .or_else(|| setup.llama_tts.clone().filter(|path| path.is_file()))
                .or_else(setup::find_tts_executable),
            weight: audio_weight.clone(),
            projector: audio_projector.clone(),
        };
        let chat = ChatRuntime::open(
            &config.data_dir,
            workspace.clone(),
            models.clone(),
            job.supervisor(),
            speech.clone(),
        )?;
        let jobs = JobRuntime::open(&config.data_dir, workspace.clone())?;
        Ok(Self {
            config,
            missions,
            workspace,
            store,
            coordinator,
            models,
            text_memory,
            llama,
            setup,
            models_dir,
            speech,
            job,
            chat,
            jobs,
            active: Arc::new(Mutex::new(None)),
            starting: Mutex::new(()),
            approvals: Mutex::new(HashMap::new()),
        })
    }

    #[must_use]
    pub fn chat(&self) -> &ChatRuntime {
        &self.chat
    }

    #[must_use]
    pub fn jobs(&self) -> &JobRuntime {
        &self.jobs
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    #[must_use]
    pub fn missions(&self) -> Vec<MissionManifest> {
        self.missions.values().cloned().collect()
    }

    #[must_use]
    pub fn health(&self) -> Value {
        let speech_ready = self.speech.ready();
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "local_only": true,
            "data_dir": self.config.data_dir.display().to_string(),
            "models_dir": self.models_dir.as_ref().map(|path| path.display().to_string()),
            "llama": {
                "executable": self.llama.executable.as_ref().map(|path| path.display().to_string()),
                "build": self.llama.build,
                "release": self.llama.release,
                "pinned": self.llama.pinned,
                "error": self.llama.error,
            },
            "setup": serde_json::to_value(setup::inspect(&self.setup)).unwrap_or(Value::Null),
            "models": self.models(),
            "voice_output": {
                "available": speech_ready || cfg!(windows),
                "backend": if speech_ready {
                    Value::from("lfm3-tts")
                } else if cfg!(windows) {
                    Value::from("system-sapi")
                } else {
                    Value::Null
                },
                "model": if speech_ready { Value::from("LFM3-Audio-2B") } else { Value::Null },
                "lfm3_tts_weights": speech_ready,
                "note": if speech_ready {
                    Value::from(
                        "Local LFM3-Audio-2B speech output through llama-tts. Audio is \
                         generated and played on this machine."
                    )
                } else if cfg!(windows) {
                    Value::from(
                        "LFM3 speech output is not configured, so spoken replies use the \
                         local operating-system voice. Nothing is uploaded."
                    )
                } else {
                    Value::from("No local speech output is configured.")
                }
            },
        })
    }

    #[must_use]
    pub fn models(&self) -> Vec<Value> {
        let active: Vec<ModelKey> = self
            .models
            .as_ref()
            .map(|manager| manager.active_models())
            .unwrap_or_default();
        let roles = [
            (
                ModelKey::Text,
                self.config.text_url.as_str(),
                self.config.text_model.as_str(),
            ),
            (
                ModelKey::Vision,
                self.config.vision_url.as_str(),
                self.config.vision_model.as_str(),
            ),
            (
                ModelKey::Audio,
                self.config.audio_url.as_str(),
                self.config.audio_model.as_str(),
            ),
            (
                ModelKey::D1,
                self.config.d1_url.as_str(),
                self.config.d1_model.as_str(),
            ),
        ];
        roles
            .into_iter()
            .map(|(key, url, model)| {
                let ready = active.contains(&key);
                let memory = if ready && key == ModelKey::Text {
                    self.text_memory
                } else {
                    0
                };
                json!({
                    "key": key,
                    "model": model,
                    "url": url,
                    "state": if ready { "ready" } else { "absent" },
                    "memory_bytes": memory,
                    "error": Value::Null,
                })
            })
            .collect()
    }

    pub async fn list_runs(&self, limit: u32) -> Result<Vec<RunRecord>, EngineError> {
        Ok(self.store.list_runs(limit).await?)
    }

    pub async fn snapshot(&self, run_id: &RunId) -> Result<Snapshot, EngineError> {
        Ok(self.store.snapshot(run_id).await?)
    }

    pub async fn acceptance(&self, run_id: &RunId) -> Result<AcceptanceSpec, EngineError> {
        Ok(self
            .store
            .run_acceptance(run_id.clone())
            .await?
            .spec()
            .clone())
    }

    pub async fn events_after(
        &self,
        run_id: &RunId,
        after: i64,
    ) -> Result<Vec<lfm3_harness::RunEvent>, EngineError> {
        Ok(self.store.events_after(run_id, after).await?)
    }

    /// Rejects a second active run before any planning tokens are spent.
    pub async fn ensure_idle(&self) -> Result<(), EngineError> {
        if let Some(active) = self.coordinator.active().await {
            return Err(EngineError::Conflict(format!(
                "run {active} is already active on this engine"
            )));
        }
        if let Some(active) = self.store.active_run().await? {
            return Err(EngineError::Conflict(format!(
                "run {active} is already active on this engine"
            )));
        }
        Ok(())
    }

    /// Plans one mission from typed inputs, freezes acceptance, and stops at
    /// plan approval. No step may run until `approve_plan` validates the hash.
    pub async fn start_run(
        &self,
        mission: &str,
        inputs: BTreeMap<String, Value>,
    ) -> Result<RunId, EngineError> {
        let _guard = self.starting.lock().await;
        self.ensure_idle().await?;
        let manifest = self
            .missions
            .get(mission)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(format!("mission {mission:?}")))?;
        if let Some(reason) = manifest.semantic_unavailable_reason() {
            return Err(EngineError::InvalidInput(format!(
                "mission {mission:?} cannot run: {reason}"
            )));
        }
        let (goal, details, artifacts) = self.collect_inputs(&manifest, &inputs).await?;

        let cancel =
            CancellationContext::new(Duration::from_secs(self.config.limits.run_timeout_seconds));
        let lease = self.acquire_text(&cancel).await?;
        let gateway: Arc<dyn ModelGateway> = Arc::new(LeaseGateway { lease });
        let planner = Planner::new(Arc::clone(&gateway)).with_max_tokens(1536);
        let request = plan_request(
            manifest.clone(),
            goal.clone(),
            details,
            self.workspace.clone(),
        );
        let plan = planner.draft(request, &cancel).await.map_err(|error| {
            EngineError::InvalidInput(format!(
                "the planner could not produce a valid plan: {error}"
            ))
        })?;
        let acceptance = manifest.compile_acceptance(goal, artifacts, plan)?;
        let record = self.store.create_run(&acceptance, None).await?;
        self.store
            .transition_run(
                record.id.clone(),
                RunStatus::Created,
                RunStatus::AwaitingPlanApproval,
                None,
            )
            .await?;
        *self.active.lock().await = Some(ActiveRun {
            run_id: record.id.clone(),
            cancel,
            gateway,
        });
        Ok(record.id)
    }

    /// Starts one stated job as a model-driven turn loop.
    ///
    /// Nothing is frozen and nothing is approved up front: the acting model
    /// decides each next action from what it has actually observed, and the
    /// journal records every turn. The only completion proof is the measured
    /// deliverable, so a job is never reported as verified mission work.
    pub async fn start_job(
        &self,
        goal: &str,
        deliverable: Option<String>,
        minutes: Option<u64>,
        max_turns: Option<u32>,
    ) -> Result<(lfm3_harness::JobRecord, Value), EngineError> {
        let _guard = self.starting.lock().await;
        let goal = goal.trim().to_owned();
        if goal.is_empty() {
            return Err(EngineError::InvalidInput(
                "a job needs a stated goal".to_owned(),
            ));
        }
        if goal.chars().count() > 32_000 {
            return Err(EngineError::InvalidInput(
                "the job goal is longer than 32,000 characters".to_owned(),
            ));
        }
        let deliverable = deliverable
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty());
        if let Some(path) = deliverable.as_deref() {
            Self::validate_deliverable(path)?;
        }
        let minutes = u32::try_from(minutes.unwrap_or(30).clamp(1, 24 * 60)).unwrap_or(30);
        let max_turns = max_turns.unwrap_or(24).clamp(1, 64);
        self.ensure_idle().await?;
        self.jobs.ensure_idle().await?;

        let route = lfm3_harness::TaskRouter::route(&goal, &self.workspace);
        let cancel = CancellationContext::new(Duration::from_secs(u64::from(minutes) * 60));
        let lease = self.acquire_text(&cancel).await?;
        let gateway: Arc<dyn ModelGateway> = Arc::new(LeaseGateway { lease });
        let mut registry = ToolRegistry::new(&self.workspace)?;
        for tool in [
            ToolName::ListFiles,
            ToolName::ReadFile,
            ToolName::WriteFile,
            ToolName::EditFile,
        ] {
            registry = registry.grant(tool);
        }
        if let Some(manager) = &self.models {
            registry = registry.with_models(manager.clone());
        }
        registry = registry.with_supervisor(self.job.supervisor());
        let record = self
            .jobs
            .start(
                gateway,
                registry,
                self.config.text_model.clone(),
                JobRequest {
                    goal,
                    deliverable,
                    minutes,
                    max_turns,
                },
            )
            .await?;
        Ok((record, route.report()))
    }

    /// Approves the displayed acceptance hash and starts execution.
    pub async fn approve_plan(
        &self,
        run_id: &RunId,
        acceptance_hash: &str,
    ) -> Result<RunId, EngineError> {
        let _guard = self.starting.lock().await;
        let snapshot = self.store.snapshot(run_id).await?;
        if snapshot.run.status != RunStatus::AwaitingPlanApproval {
            return Err(EngineError::Conflict(format!(
                "run {run_id} is {}, not awaiting plan approval",
                snapshot.run.status.as_str()
            )));
        }
        if snapshot.run.acceptance_hash != acceptance_hash {
            return Err(EngineError::Conflict(
                "the submitted acceptance hash does not match this run".to_owned(),
            ));
        }
        let acceptance = self.store.run_acceptance(run_id.clone()).await?;
        if acceptance.hash() != acceptance_hash {
            return Err(EngineError::Conflict(
                "the stored acceptance contract does not match the displayed hash".to_owned(),
            ));
        }
        let task_run = acceptance.spec().mission == lfm3_harness::GENERAL_TASK;

        if let Some(other) = self.coordinator.active().await {
            if other != *run_id {
                return Err(EngineError::Conflict(format!(
                    "run {other} is already active on this engine"
                )));
            }
        }
        if let Some(other) = self.store.active_run().await? {
            if other != *run_id {
                return Err(EngineError::Conflict(format!(
                    "run {other} is already active on this engine"
                )));
            }
        }

        let mut slot = self.active.lock().await;
        if slot.as_ref().is_some_and(|active| active.run_id != *run_id) {
            return Err(EngineError::Conflict(format!(
                "run {run_id} does not own the engine's model lease"
            )));
        }
        let taken = slot.take();
        drop(slot);

        let (gateway, cancel) = match taken {
            Some(active) => (active.gateway, active.cancel),
            None => {
                let seconds = if task_run {
                    30 * 60
                } else {
                    self.config.limits.run_timeout_seconds
                };
                let cancel = CancellationContext::new(Duration::from_secs(seconds));
                let lease = self.acquire_text(&cancel).await?;
                (
                    Arc::new(LeaseGateway { lease }) as Arc<dyn ModelGateway>,
                    cancel,
                )
            }
        };
        let mut registry = ToolRegistry::new(&self.workspace)?;
        for tool in &acceptance.spec().allowed_tools {
            let name = ToolName::parse(tool).ok_or_else(|| {
                EngineError::Setup(format!("mission grants unimplemented tool {tool:?}"))
            })?;
            registry = registry.grant(name);
        }
        if let Some(manager) = &self.models {
            registry = registry.with_models(manager.clone());
        }
        registry = registry.with_supervisor(self.job.supervisor());
        let executor = Executor::new(
            Arc::clone(&gateway),
            Arc::new(registry),
            self.config.text_model.clone(),
        )
        .with_max_tokens(if task_run { 2048 } else { 512 })
        .with_max_calls(if task_run { 3 } else { 1 });
        let active_gateway = Arc::clone(&gateway);
        let active_cancel = cancel.clone();
        let mut runner = Runner::new(
            self.store.clone(),
            acceptance.clone(),
            self.workspace.clone(),
            Arc::new(executor),
            cancel,
        )?
        .with_budget(if task_run {
            self.task_budget()
        } else {
            self.budget()
        });

        // Semantic missions are only approved when their frozen policy is
        // automatic; `start_run` already refuses human-review-only missions.
        if let Some(policy) = self
            .missions
            .get(&acceptance.spec().mission)
            .and_then(MissionManifest::semantic_policy)
            .filter(|policy| policy.automatic())
        {
            let d1 = LlamaClient::new(self.config.d1_url.clone(), self.config.d1_model.clone())?;
            let vision = LlamaClient::new(
                self.config.vision_url.clone(),
                self.config.vision_model.clone(),
            )
            .ok()
            .map(|client| Arc::new(client) as Arc<dyn ModelGateway>);
            let judge = Arc::new(ModelJudge::new(Arc::new(d1), vision));
            runner = runner.with_verifier(Arc::new(SemanticVerifier::new(policy.clone(), judge)));
        }

        self.store
            .transition_run(
                run_id.clone(),
                RunStatus::AwaitingPlanApproval,
                RunStatus::Running,
                None,
            )
            .await?;

        // Keep the cancellation handle engine-owned for the whole execution so
        // a UI cancel reaches the live runner instead of being refused.
        *self.active.lock().await = Some(ActiveRun {
            run_id: run_id.clone(),
            cancel: active_cancel,
            gateway: active_gateway,
        });

        let coordinator = self.coordinator.clone();
        let active = Arc::clone(&self.active);
        let run = run_id.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            match coordinator.start(run.clone(), runner).await {
                Ok(outcome) => {
                    tracing::info!(run = %run, status = outcome.status.as_str(), "run finished");
                }
                Err(error) => {
                    tracing::error!(run = %run, error = %error, "run coordinator refused execution");
                    // Nothing owns the run now. Finish it as incomplete rather
                    // than leaving a `running` row that never moves.
                    let current = store.snapshot(&run).await;
                    if let Ok(snapshot) = current {
                        let _ = store
                            .transition_run(
                                run.clone(),
                                snapshot.run.status,
                                RunStatus::Incomplete,
                                Some("execution_refused".to_owned()),
                            )
                            .await;
                    }
                }
            }
            let mut slot = active.lock().await;
            if slot.as_ref().is_some_and(|held| held.run_id == run) {
                *slot = None;
            }
        });
        Ok(run_id.clone())
    }

    /// Cancels an awaiting, running, or cancelling run. Unknown runs fail
    /// closed; terminal runs are a conflict.
    pub async fn cancel_run(&self, run_id: &RunId) -> Result<(), EngineError> {
        let snapshot = self.store.snapshot(run_id).await?;
        match snapshot.run.status {
            RunStatus::AwaitingPlanApproval | RunStatus::Created | RunStatus::Planning => {
                self.store
                    .transition_run(
                        run_id.clone(),
                        snapshot.run.status,
                        RunStatus::Cancelled,
                        Some("cancelled_by_user".to_owned()),
                    )
                    .await?;
                let mut slot = self.active.lock().await;
                if slot.as_ref().is_some_and(|active| active.run_id == *run_id) {
                    *slot = None;
                }
            }
            RunStatus::Cancelling | RunStatus::Cancelled => {}
            RunStatus::Running => {
                let slot = self.active.lock().await;
                let Some(active) = slot.as_ref().filter(|active| active.run_id == *run_id) else {
                    return Err(EngineError::Conflict(format!(
                        "run {run_id} has no engine-owned cancellation context"
                    )));
                };
                active.cancel.cancel();
            }
            status if status.is_terminal() => {
                return Err(EngineError::Conflict(format!(
                    "run {run_id} is already {}",
                    status.as_str()
                )));
            }
            status => {
                return Err(EngineError::Conflict(format!(
                    "run {run_id} cannot be cancelled from {}",
                    status.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Creates a linked child with the same frozen contract, stopping before
    /// plan approval so the user reviews the unchanged hash again.
    pub async fn retry_run(&self, parent: &RunId) -> Result<RunId, EngineError> {
        let _guard = self.starting.lock().await;
        self.ensure_idle().await?;
        let snapshot = self.store.snapshot(parent).await?;
        if !snapshot.run.status.is_terminal() {
            return Err(EngineError::Conflict(format!(
                "run {parent} is still {}",
                snapshot.run.status.as_str()
            )));
        }
        if snapshot.run.status == RunStatus::Passed {
            return Err(EngineError::Conflict(
                "a passed run is already proven; start a new run instead".to_owned(),
            ));
        }
        let acceptance = self.store.run_acceptance(parent.clone()).await?;
        let record = self
            .store
            .create_run(&acceptance, Some(parent.clone()))
            .await?;
        self.store
            .transition_run(
                record.id.clone(),
                RunStatus::Created,
                RunStatus::AwaitingPlanApproval,
                None,
            )
            .await?;
        Ok(record.id)
    }

    pub async fn release_model(&self, key: ModelKey) -> Result<bool, EngineError> {
        let manager = self
            .models
            .as_ref()
            .ok_or_else(|| EngineError::Setup("no local model runtime is configured".to_owned()))?;
        Ok(manager.evict(key)?)
    }

    /// Registers a pending tool approval. The future tool gate will wait on
    /// this entry; commands validate identity, expiry, and run state now.
    pub async fn register_approval(&self, run_id: RunId) -> String {
        self.register_approval_ttl(run_id, APPROVAL_TTL).await
    }

    async fn register_approval_ttl(&self, run_id: RunId, ttl: Duration) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.approvals.lock().await.insert(
            id.clone(),
            PendingApproval {
                run_id,
                expires_at: Instant::now() + ttl,
            },
        );
        id
    }

    pub async fn decide_approval(
        &self,
        approval_id: &str,
        _allow: bool,
    ) -> Result<(), EngineError> {
        let pending = self
            .approvals
            .lock()
            .await
            .remove(approval_id)
            .ok_or_else(|| {
                EngineError::NotFound(format!("approval {approval_id:?} is not pending"))
            })?;
        if pending.expires_at <= Instant::now() {
            return Err(EngineError::Conflict(format!(
                "approval {approval_id:?} expired"
            )));
        }
        let snapshot = self.store.snapshot(&pending.run_id).await?;
        if snapshot.run.status.is_terminal() {
            return Err(EngineError::Conflict(format!(
                "run {} is already {}; the approval is stale",
                pending.run_id,
                snapshot.run.status.as_str()
            )));
        }
        Ok(())
    }

    /// Resolves an artifact inside trusted engine storage. The caller never
    /// supplies a path, so a foreign ID cannot reach the filesystem.
    pub async fn artifact_path(&self, raw: &str) -> Result<PathBuf, EngineError> {
        let id: ArtifactId = raw.parse()?;
        Ok(self.store.artifact_path(&id).await?)
    }

    pub async fn open_artifact(&self, raw: &str) -> Result<(), EngineError> {
        let path = self.artifact_path(raw).await?;
        open::that_detached(&path).map_err(|error| {
            EngineError::Process(format!("open artifact {}: {error}", path.display()))
        })?;
        Ok(())
    }

    pub fn validate_choosable(&self, mission: &str, input_key: &str) -> Result<(), EngineError> {
        let manifest = self
            .missions
            .get(mission)
            .ok_or_else(|| EngineError::NotFound(format!("mission {mission:?}")))?;
        let input = manifest
            .inputs
            .get(input_key)
            .ok_or_else(|| EngineError::NotFound(format!("input {input_key:?}")))?;
        if !matches!(
            input.kind,
            lfm3_harness::mission::InputKind::Image
                | lfm3_harness::mission::InputKind::Audio
                | lfm3_harness::mission::InputKind::File
        ) {
            return Err(EngineError::InvalidInput(format!(
                "input {input_key:?} is not a file input"
            )));
        }
        Ok(())
    }

    /// Refuses an output path that could leave the workspace or name a device.
    fn validate_deliverable(path: &str) -> Result<(), EngineError> {
        let normalized = path.replace('\\', "/");
        if normalized.starts_with('/') || normalized.contains(':') {
            return Err(EngineError::InvalidInput(
                "the required output must be a workspace-relative path".to_owned(),
            ));
        }
        if normalized
            .split('/')
            .any(|part| part == ".." || part.is_empty() && normalized.len() > 1)
        {
            return Err(EngineError::InvalidInput(
                "the required output cannot leave the workspace".to_owned(),
            ));
        }
        Ok(())
    }

    /// A job may retry far more than a single-action mission step.
    fn task_budget(&self) -> Budget {
        Budget {
            attempts_per_strategy: 4,
            strategies: 3,
            total_attempts: 16,
        }
    }

    fn budget(&self) -> Budget {
        let attempts = u32::try_from(self.config.limits.max_attempts_per_strategy)
            .unwrap_or(1)
            .max(1);
        let strategies = u32::try_from(self.config.limits.max_strategies)
            .unwrap_or(1)
            .max(1);
        let total = u32::try_from(self.config.limits.max_total_attempts)
            .unwrap_or(attempts)
            .max(attempts);
        Budget {
            attempts_per_strategy: attempts,
            strategies,
            total_attempts: total,
        }
    }

    async fn acquire_text(&self, cancel: &CancellationContext) -> Result<ModelLease, EngineError> {
        let manager = self.models.as_ref().ok_or_else(|| {
            EngineError::Setup(
                self.llama
                    .error
                    .clone()
                    .unwrap_or_else(|| "no local model runtime is configured".to_owned()),
            )
        })?;
        manager
            .acquire(ModelKey::Text, &cancel.child())
            .await
            .map_err(EngineError::from)
    }

    async fn collect_inputs(
        &self,
        manifest: &MissionManifest,
        inputs: &BTreeMap<String, Value>,
    ) -> Result<
        (
            String,
            BTreeMap<String, String>,
            BTreeMap<String, InputArtifact>,
        ),
        EngineError,
    > {
        for (name, spec) in &manifest.inputs {
            let value = inputs.get(name);
            let missing = match value {
                None => true,
                Some(Value::String(text)) => text.trim().is_empty(),
                Some(Value::Null) => true,
                Some(_) => false,
            };
            if spec.required && missing {
                return Err(EngineError::InvalidInput(format!(
                    "required input {name:?} is missing"
                )));
            }
        }
        let mut goal = String::new();
        let mut details = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        for (name, spec) in &manifest.inputs {
            let Some(value) = inputs.get(name) else {
                continue;
            };
            match spec.kind {
                lfm3_harness::mission::InputKind::String => {
                    let text = value.as_str().ok_or_else(|| {
                        EngineError::InvalidInput(format!("input {name:?} must be text"))
                    })?;
                    if name == "goal" {
                        goal = text.to_owned();
                    } else {
                        details.insert(name.clone(), text.to_owned());
                    }
                }
                lfm3_harness::mission::InputKind::Json => {
                    details.insert(
                        name.clone(),
                        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
                    );
                }
                lfm3_harness::mission::InputKind::Image
                | lfm3_harness::mission::InputKind::Audio
                | lfm3_harness::mission::InputKind::File => {
                    let raw = value.as_str().ok_or_else(|| {
                        EngineError::InvalidInput(format!(
                            "input {name:?} must be a chosen file path"
                        ))
                    })?;
                    let path = PathBuf::from(raw);
                    let bytes = fs::read(&path).map_err(|error| {
                        EngineError::InvalidInput(format!("read {}: {error}", path.display()))
                    })?;
                    let stored = self
                        .store
                        .write_artifact(&bytes, lfm3_harness::store::TEXT_PLAIN)
                        .await?;
                    let id: ArtifactId = stored.sha256[..32].parse().map_err(|error| {
                        EngineError::Setup(format!("artifact id from digest: {error}"))
                    })?;
                    details.insert(
                        name.clone(),
                        format!("{} (sha256 {})", path.display(), stored.sha256),
                    );
                    artifacts.insert(
                        name.clone(),
                        InputArtifact {
                            id,
                            sha256: stored.sha256,
                        },
                    );
                }
            }
        }
        if goal.trim().is_empty() {
            goal = manifest.description.clone();
        }
        Ok((goal, details, artifacts))
    }
}

/// Serialized inference through one held module lease.
struct LeaseGateway {
    lease: ModelLease,
}

impl ModelGateway for LeaseGateway {
    fn chat(
        &self,
        request: ChatRequest,
        cancel: lfm3_harness::CancellationChild,
    ) -> ModelFuture<'_, ChatResponse> {
        Box::pin(async move { self.lease.chat(request, &cancel).await })
    }

    fn transcribe(
        &self,
        request: TranscribeRequest,
        cancel: lfm3_harness::CancellationChild,
    ) -> ModelFuture<'_, Transcript> {
        Box::pin(async move { self.lease.transcribe(request, &cancel).await })
    }

    fn decide(
        &self,
        request: DecisionRequest,
        cancel: lfm3_harness::CancellationChild,
    ) -> ModelFuture<'_, Decision> {
        Box::pin(async move { self.lease.decide(request, &cancel).await })
    }
}

#[allow(clippy::too_many_arguments)]
fn build_model_manager(
    config: &Config,
    runtime: &LlamaRuntime,
    weight: Option<&Path>,
    audio_weight: Option<&Path>,
    audio_projector: Option<&Path>,
    vision_weight: Option<&Path>,
    vision_projector: Option<&Path>,
    d1_weight: Option<&Path>,
    job: &ApplicationJob,
) -> Option<ModelManager> {
    let executable = runtime.executable.clone()?;
    if !runtime.pinned {
        return None;
    }
    let weight = weight?;
    let file_bytes = fs::metadata(weight).ok()?.len();
    let text_memory = file_bytes.saturating_add(MODEL_MEMORY_HEADROOM);
    let mut specs = vec![ModelSpec::owned(
        ModelKey::Text,
        config.text_url.clone(),
        config.text_model.clone(),
        text_memory,
        LaunchSpec::new(executable.clone(), text_launch_args(config, weight)),
    )];
    let mut total = text_memory;

    // Voice input uses the local audio model through the same manager. It is
    // registered only when both the weight and its projector exist, so an
    // incomplete install never advertises a capability it cannot run.
    if let (Some(audio), Some(projector)) = (audio_weight, audio_projector) {
        if let Ok(metadata) = fs::metadata(audio) {
            let audio_memory = metadata.len().saturating_add(MODEL_MEMORY_HEADROOM);
            specs.push(ModelSpec::owned(
                ModelKey::Audio,
                config.audio_url.clone(),
                config.audio_model.clone(),
                audio_memory,
                LaunchSpec::new(
                    executable.clone(),
                    audio_launch_args(config, audio, projector),
                ),
            ));
            total = total.saturating_add(audio_memory);
        }
    }

    // Visual observation is registered only with both its weight and its
    // projector, so `look` never advertises a capability it cannot run.
    if let (Some(vision), Some(projector)) = (vision_weight, vision_projector) {
        if let Ok(metadata) = fs::metadata(vision) {
            let vision_memory = metadata.len().saturating_add(MODEL_MEMORY_HEADROOM);
            specs.push(ModelSpec::owned(
                ModelKey::Vision,
                config.vision_url.clone(),
                config.vision_model.clone(),
                vision_memory,
                LaunchSpec::new(
                    executable.clone(),
                    projector_launch_args(
                        &config.vision_url,
                        vision,
                        projector,
                        &config.vision_model,
                    ),
                ),
            ));
            total = total.saturating_add(vision_memory);
        }
    }

    // The calibrated classifier serves routing judgements and grounded checks.
    if let Some(d1) = d1_weight {
        if let Ok(metadata) = fs::metadata(d1) {
            let d1_memory = metadata.len().saturating_add(MODEL_MEMORY_HEADROOM);
            specs.push(ModelSpec::owned(
                ModelKey::D1,
                config.d1_url.clone(),
                config.d1_model.clone(),
                d1_memory,
                LaunchSpec::new(executable, d1_launch_args(config, d1)),
            ));
            total = total.saturating_add(d1_memory);
        }
    }

    ModelManager::new(
        ModelManagerConfig {
            total_memory_bytes: total.saturating_add(MANAGER_MEMORY_HEADROOM),
            startup_timeout: Duration::from_secs(120),
            health_interval: Duration::from_millis(250),
        },
        specs,
        job.supervisor(),
    )
    .ok()
}

fn text_launch_args(config: &Config, weight: &Path) -> Vec<String> {
    let mut args = base_launch_args(&config.text_url, weight, 8192);
    args.push("--alias".to_owned());
    args.push(config.text_model.clone());
    args
}

fn audio_launch_args(config: &Config, weight: &Path, projector: &Path) -> Vec<String> {
    projector_launch_args(&config.audio_url, weight, projector, &config.audio_model)
}

fn projector_launch_args(
    url: &url::Url,
    weight: &Path,
    projector: &Path,
    alias: &str,
) -> Vec<String> {
    let mut args = base_launch_args(url, weight, 8192);
    args.push("--mmproj".to_owned());
    args.push(projector.display().to_string());
    args.push("--alias".to_owned());
    args.push(alias.to_owned());
    args
}

fn d1_launch_args(config: &Config, weight: &Path) -> Vec<String> {
    let mut args = base_launch_args(&config.d1_url, weight, 8192);
    args.push("-b".to_owned());
    args.push("8192".to_owned());
    args.push("-ub".to_owned());
    args.push("8192".to_owned());
    args.push("--alias".to_owned());
    args.push(config.d1_model.clone());
    args
}

fn base_launch_args(url: &url::Url, weight: &Path, context: u32) -> Vec<String> {
    let host = url.host_str().unwrap_or("127.0.0.1");
    let port = url.port_or_known_default().unwrap_or(8080);
    vec![
        "-m".to_owned(),
        weight.display().to_string(),
        "--host".to_owned(),
        host.to_owned(),
        "--port".to_owned(),
        port.to_string(),
        "--jinja".to_owned(),
        "-c".to_owned(),
        context.to_string(),
        "-ngl".to_owned(),
        "0".to_owned(),
    ]
}

/// Applies only explicit, existing settings. Missing files stay missing so the
/// health report can say so; nothing is created or downloaded.
fn apply_settings(config: &mut Config, settings: &SetupSettings) {
    if config.llama.server.is_none() {
        if let Some(path) = settings.llama_server.as_ref().filter(|path| path.is_file()) {
            config.llama.server = Some(path.clone());
        }
    }
    if config.llama.tts.is_none() {
        if let Some(path) = settings.llama_tts.as_ref().filter(|path| path.is_file()) {
            config.llama.tts = Some(path.clone());
        }
    }
}

/// Resolves the pinned llama.cpp runtime without touching the network.
#[must_use]
pub fn probe_runtime(config: &Config) -> LlamaRuntime {
    let Some(executable) = config
        .llama
        .server
        .clone()
        .or_else(setup::find_server_executable)
    else {
        return LlamaRuntime {
            executable: None,
            build: 0,
            release: String::new(),
            pinned: false,
            error: Some(format!(
                "no llama-server binary found; set LFM3_LLAMA_SERVER to the pinned b{} build",
                setup::PINNED_BUILD
            )),
        };
    };
    match setup::probe_server(&executable) {
        Ok(probe) => LlamaRuntime {
            executable: Some(executable),
            build: probe.build,
            release: probe.release,
            pinned: probe.pinned,
            error: if probe.pinned {
                None
            } else {
                Some(format!(
                    "runtime is not the pinned b{} / v{} build",
                    setup::PINNED_BUILD,
                    setup::PINNED_RELEASE
                ))
            },
        },
        Err(error) => LlamaRuntime {
            executable: Some(executable),
            build: 0,
            release: String::new(),
            pinned: false,
            error: Some(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        io::{Read, Write},
        net::TcpListener,
    };

    use lfm3_harness::{
        mission::{OutputContract, PlannedStep},
        AcceptanceSpec, CheckSpec, ProcessSupervisor, RunCoordinator, RunStatus,
    };
    use serde_json::json;

    use super::*;

    struct Fixture {
        engine: Engine,
        _directory: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let directory = tempfile::tempdir().expect("temp dir");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let store = Store::open(directory.path().join("runs.db"))
            .await
            .expect("store");
        let config = Config {
            data_dir: directory.path().to_path_buf(),
            workspace: Some(workspace.clone()),
            ..Config::default()
        };
        let coordinator = RunCoordinator::new(store.clone());
        let job = ApplicationJob::new().expect("application job");
        let chat = ChatRuntime::open(
            directory.path(),
            workspace.clone(),
            None,
            job.supervisor(),
            SpeechConfig::default(),
        )
        .expect("chat runtime");
        let jobs = JobRuntime::open(directory.path(), workspace.clone()).expect("job runtime");
        let engine = Engine {
            config,
            missions: BTreeMap::new(),
            workspace,
            store,
            coordinator,
            models: None,
            text_memory: 0,
            llama: LlamaRuntime {
                executable: None,
                build: 0,
                release: String::new(),
                pinned: false,
                error: None,
            },
            setup: SetupSettings::default(),
            models_dir: None,
            speech: SpeechConfig::default(),
            job,
            chat,
            jobs,
            active: Arc::new(Mutex::new(None)),
            starting: Mutex::new(()),
            approvals: Mutex::new(HashMap::new()),
        };
        Fixture {
            engine,
            _directory: directory,
        }
    }

    fn acceptance() -> lfm3_harness::FrozenAcceptance {
        AcceptanceSpec {
            schema_version: 1,
            mission: "desk_task".to_owned(),
            mission_version: 1,
            goal: "Write the requested summary".to_owned(),
            inputs: BTreeMap::new(),
            steps: vec![PlannedStep {
                index: 1,
                title: "Write the summary".to_owned(),
                criterion: "the summary file exists and is non-empty".to_owned(),
                checks: vec![CheckSpec::new(
                    "file_exists",
                    [("path", json!("out/summary.md"))],
                )],
                tool: None,
            }],
            output: OutputContract::default(),
            allowed_tools: BTreeSet::new(),
            allowed_checks: BTreeSet::new(),
            max_steps: 4,
            max_attempts: 3,
        }
        .freeze()
        .expect("freeze acceptance")
    }

    async fn awaiting_run(engine: &Engine) -> (RunId, String) {
        let frozen = acceptance();
        let record = engine
            .store
            .create_run(&frozen, None)
            .await
            .expect("create run");
        engine
            .store
            .transition_run(
                record.id.clone(),
                RunStatus::Created,
                RunStatus::AwaitingPlanApproval,
                None,
            )
            .await
            .expect("await approval");
        (record.id, frozen.hash().to_owned())
    }

    /// A gateway that is never called by these state tests.
    struct UnreachableGateway;

    impl ModelGateway for UnreachableGateway {
        fn chat(
            &self,
            _request: ChatRequest,
            _cancel: lfm3_harness::CancellationChild,
        ) -> ModelFuture<'_, ChatResponse> {
            Box::pin(async { Err(ModelError::Protocol("unreachable".to_owned())) })
        }

        fn transcribe(
            &self,
            _request: TranscribeRequest,
            _cancel: lfm3_harness::CancellationChild,
        ) -> ModelFuture<'_, Transcript> {
            Box::pin(async { Err(ModelError::Protocol("unreachable".to_owned())) })
        }

        fn decide(
            &self,
            _request: DecisionRequest,
            _cancel: lfm3_harness::CancellationChild,
        ) -> ModelFuture<'_, Decision> {
            Box::pin(async { Err(ModelError::Protocol("unreachable".to_owned())) })
        }
    }

    fn stub_server(identity: &'static str) -> url::Url {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub model server");
        let address = listener.local_addr().expect("stub address");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request);
                let body = format!(
                    r#"{{"object":"list","data":[{{"id":"{identity}","object":"model"}}]}}"#
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}/v1").parse().expect("stub url")
    }

    #[tokio::test]
    async fn human_review_semantic_missions_cannot_start() {
        let mut fixture = fixture().await;
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../missions");
        let raw = fs::read_to_string(directory.join("screen_qa.json")).expect("read mission");
        let mission = MissionManifest::from_json(&raw).expect("mission");
        fixture
            .engine
            .missions
            .insert(mission.name.clone(), mission);

        let error = fixture
            .engine
            .start_run("screen_qa", BTreeMap::new())
            .await
            .expect_err("human-review-only missions cannot start");
        assert!(matches!(error, EngineError::InvalidInput(_)), "{error}");
        assert!(error.to_string().contains("cannot run"));
        assert!(fixture
            .engine
            .store
            .active_run()
            .await
            .expect("idle")
            .is_none());
    }

    #[tokio::test]
    async fn approving_a_foreign_acceptance_hash_is_rejected() {
        let fixture = fixture().await;
        let (run, _) = awaiting_run(&fixture.engine).await;

        let error = fixture
            .engine
            .approve_plan(&run, &"0".repeat(64))
            .await
            .expect_err("a foreign hash cannot approve a run");
        assert!(matches!(error, EngineError::Conflict(_)), "{error}");
        let snapshot = fixture.engine.snapshot(&run).await.expect("snapshot");
        assert_eq!(snapshot.run.status, RunStatus::AwaitingPlanApproval);
    }

    #[tokio::test]
    async fn approvals_expire_and_foreign_ids_are_rejected() {
        let fixture = fixture().await;
        let (run, _) = awaiting_run(&fixture.engine).await;

        let expired = fixture
            .engine
            .register_approval_ttl(run.clone(), Duration::ZERO)
            .await;
        let error = fixture
            .engine
            .decide_approval(&expired, true)
            .await
            .expect_err("an expired approval cannot be allowed");
        assert!(matches!(error, EngineError::Conflict(_)), "{error}");

        let error = fixture
            .engine
            .decide_approval("00000000000000000000000000000000", true)
            .await
            .expect_err("an unknown approval id cannot be decided");
        assert!(matches!(error, EngineError::NotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn cancelling_an_unknown_run_fails_closed() {
        let fixture = fixture().await;
        let error = fixture
            .engine
            .cancel_run(&RunId::new())
            .await
            .expect_err("an unknown run cannot be cancelled");
        assert!(matches!(error, EngineError::Harness(_)), "{error}");
    }

    #[tokio::test]
    async fn cancelling_a_running_run_reaches_the_engine_owned_token() {
        let fixture = fixture().await;
        let (run, _) = awaiting_run(&fixture.engine).await;
        fixture
            .engine
            .store
            .transition_run(
                run.clone(),
                RunStatus::AwaitingPlanApproval,
                RunStatus::Running,
                None,
            )
            .await
            .expect("start running");
        let cancel = CancellationContext::new(Duration::from_secs(30));
        let witness = cancel.clone();
        let gateway: Arc<dyn ModelGateway> = Arc::new(UnreachableGateway);
        *fixture.engine.active.lock().await = Some(ActiveRun {
            run_id: run.clone(),
            cancel,
            gateway,
        });

        fixture
            .engine
            .cancel_run(&run)
            .await
            .expect("a running run is cancellable");
        assert!(witness.is_cancelled());
    }

    #[tokio::test]
    async fn a_second_active_run_is_refused() {
        let fixture = fixture().await;
        let (_run, _) = awaiting_run(&fixture.engine).await;

        let error = fixture
            .engine
            .ensure_idle()
            .await
            .expect_err("one engine owns at most one active run");
        assert!(matches!(error, EngineError::Conflict(_)), "{error}");
    }

    #[tokio::test]
    async fn leased_models_cannot_be_released() {
        let mut fixture = fixture().await;
        let endpoint = stub_server("stub-text");
        let manager = ModelManager::new(
            ModelManagerConfig {
                total_memory_bytes: 8 * 1024 * 1024,
                startup_timeout: Duration::from_secs(5),
                health_interval: Duration::from_millis(20),
            },
            vec![ModelSpec::external(
                ModelKey::Text,
                endpoint,
                "stub-text",
                1024,
            )],
            ProcessSupervisor::new().expect("supervisor"),
        )
        .expect("manager");
        let lease = manager
            .acquire(
                ModelKey::Text,
                &CancellationContext::new(Duration::from_secs(5)).child(),
            )
            .await
            .expect("acquire stub model");
        fixture.engine.models = Some(manager);

        let error = fixture
            .engine
            .release_model(ModelKey::Text)
            .await
            .expect_err("a leased model is never released");
        assert!(matches!(error, EngineError::Model(ModelError::Leased(_))));

        drop(lease);
        assert!(fixture
            .engine
            .release_model(ModelKey::Text)
            .await
            .expect("idle model releases"));
    }

    #[tokio::test]
    async fn artifact_ids_cannot_escape_trusted_storage() {
        let fixture = fixture().await;
        for raw in ["../../secret.txt", "C:\\Windows\\win.ini", "not-an-id"] {
            assert!(
                fixture.engine.artifact_path(raw).await.is_err(),
                "{raw} must not resolve"
            );
        }
        assert!(fixture.engine.artifact_path(&"a".repeat(32)).await.is_err());

        let stored = fixture
            .engine
            .store
            .write_artifact(b"evidence", lfm3_harness::store::TEXT_PLAIN)
            .await
            .expect("store artifact");
        let path = fixture
            .engine
            .artifact_path(stored.id.as_str())
            .await
            .expect("trusted artifact path");
        assert!(path.is_file());
    }

    #[tokio::test]
    async fn trusted_state_survives_a_path_containing_spaces() {
        let directory = tempfile::tempdir().expect("temp dir");
        let spaced = directory
            .path()
            .join("LFM3 Test Space")
            .join("nested folder");
        fs::create_dir_all(&spaced).expect("spaced directory");
        let store = Store::open(spaced.join("runs.db")).await.expect("store");
        let stored = store
            .write_artifact(b"spaced evidence", lfm3_harness::store::TEXT_PLAIN)
            .await
            .expect("write artifact");
        let bytes = store
            .read_artifact(&stored.id)
            .await
            .expect("read artifact");
        assert_eq!(bytes, b"spaced evidence");
        let path = store
            .artifact_path(&stored.id)
            .await
            .expect("artifact path");
        assert!(path.starts_with(spaced.canonicalize().expect("canonical")));
    }

    #[tokio::test]
    async fn events_replay_after_a_cursor() {
        let fixture = fixture().await;
        let (run, _) = awaiting_run(&fixture.engine).await;

        let all = fixture
            .engine
            .events_after(&run, 0)
            .await
            .expect("initial journal");
        assert!(all.len() >= 2, "create and transition are journaled");
        assert!(all.windows(2).all(|pair| pair[0].seq < pair[1].seq));

        let cursor = all[0].seq;
        let rest = fixture
            .engine
            .events_after(&run, cursor)
            .await
            .expect("replayed journal");
        assert!(rest.iter().all(|event| event.seq > cursor));
        assert_eq!(rest.len(), all.len() - 1);
    }
}
