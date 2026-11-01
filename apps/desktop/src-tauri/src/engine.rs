//! One embedded harness instance behind the desktop command layer.
//!
//! The engine owns the durable store, file-backed acceptance contracts, the
//! model scheduler, and the run coordinator. Commands validate state here;
//! Tauri only transports typed values and committed journal events.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use lfm3_harness::{
    agent::{
        plan_request, Budget, Executor, ModelProfile, Planner, Runner, SemanticVerifier, LFM3_TEXT,
    },
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
    downloads::{self, DownloadError, DownloadEvent, DownloadManager},
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
    #[error(transparent)]
    Download(#[from] DownloadError),
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

/// Installed weights discovered inside the models folder.
///
/// A role is only usable when every file it needs is present, so vision and
/// audio require their projector as well as their weight.
#[derive(Clone, Debug, Default)]
struct InstalledWeights {
    text: Option<PathBuf>,
    vision: Option<PathBuf>,
    vision_projector: Option<PathBuf>,
    audio: Option<PathBuf>,
    audio_projector: Option<PathBuf>,
    d1: Option<PathBuf>,
}

impl InstalledWeights {
    fn discover(root: Option<&Path>) -> Self {
        let Some(root) = root else {
            return Self::default();
        };
        Self {
            text: setup::find_weight(root, "LFM3-1.7B"),
            vision: setup::find_weight(root, "LFM3-VL-2B"),
            vision_projector: setup::find_projector_for(root, "LFM3-VL-2B"),
            audio: setup::find_weight(root, "LFM3-Audio-2B"),
            audio_projector: setup::find_projector_for(root, "LFM3-Audio-2B"),
            d1: setup::find_weight(root, "d1-2B"),
        }
    }

    fn weight(&self, key: ModelKey) -> Option<&Path> {
        match key {
            ModelKey::Text => self.text.as_deref(),
            ModelKey::Vision => self.vision.as_deref(),
            ModelKey::Audio => self.audio.as_deref(),
            ModelKey::D1 => self.d1.as_deref(),
        }
    }

    fn projector(&self, key: ModelKey) -> Option<&Path> {
        match key {
            ModelKey::Vision => self.vision_projector.as_deref(),
            ModelKey::Audio => self.audio_projector.as_deref(),
            _ => None,
        }
    }

    /// True only when this role could actually start with the files present.
    fn usable(&self, key: ModelKey) -> bool {
        match key {
            ModelKey::Text | ModelKey::D1 => self.weight(key).is_some(),
            ModelKey::Vision | ModelKey::Audio => {
                self.weight(key).is_some() && self.projector(key).is_some()
            }
        }
    }
}

/// The single engine instance the desktop app embeds.
pub struct Engine {
    config: Config,
    missions: BTreeMap<String, MissionManifest>,
    workspace: RwLock<PathBuf>,
    store: Store,
    coordinator: lfm3_harness::RunCoordinator,
    models: RwLock<Option<ModelManager>>,
    text_memory: RwLock<u64>,
    llama: LlamaRuntime,
    setup: RwLock<SetupSettings>,
    models_dir: PathBuf,
    weights: RwLock<InstalledWeights>,
    speech: RwLock<SpeechConfig>,
    downloads: DownloadManager,
    /// A finished download whose files could not be swapped in because a turn
    /// held the models. The next idle catalog/reload applies it.
    pending_reload: AtomicBool,
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
            .or_else(|| settings.workspace.clone())
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
        let models_dir = effective_models_dir(&config, &setup);
        let weights = InstalledWeights::discover(Some(&models_dir));
        let llama = probe_runtime(&config);
        let models = build_model_manager(&config, &llama, &weights, &job);
        let text_memory = text_memory_of(&weights);
        let coordinator = lfm3_harness::RunCoordinator::new(store.clone());
        let speech = build_speech(&config, &setup, &weights);
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
            workspace: RwLock::new(workspace),
            store,
            coordinator,
            models: RwLock::new(models),
            text_memory: RwLock::new(text_memory),
            llama,
            setup: RwLock::new(setup),
            models_dir,
            weights: RwLock::new(weights),
            speech: RwLock::new(speech),
            downloads: DownloadManager::default(),
            pending_reload: AtomicBool::new(false),
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

    /// The shared model manager, for benchmark probes that exercise the real
    /// router without starting a chat turn. The clone shares the same servers;
    /// it is safe to hold for the duration of one probe.
    #[must_use]
    pub fn model_manager(&self) -> Option<ModelManager> {
        read(&self.models).clone()
    }

    /// The folder chat and jobs work in, copied so a rename mid-turn cannot
    /// change what that turn acts on.
    #[must_use]
    pub fn workspace(&self) -> PathBuf {
        read(&self.workspace).clone()
    }

    #[must_use]
    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    #[must_use]
    pub fn setup_settings(&self) -> SetupSettings {
        read(&self.setup).clone()
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
        let speech_ready = read(&self.speech).ready();
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "local_only": true,
            "data_dir": self.config.data_dir.display().to_string(),
            "workspace": self.workspace().display().to_string(),
            "models_dir": self.models_dir.display().to_string(),
            "llama": {
                "executable": self.llama.executable.as_ref().map(|path| path.display().to_string()),
                "build": self.llama.build,
                "release": self.llama.release,
                "pinned": self.llama.pinned,
                "error": self.llama.error,
            },
            "setup": serde_json::to_value(setup::inspect(&self.setup_settings())).unwrap_or(Value::Null),
            "models": self.models(),
            "model_profile": LFM3_TEXT.report(),
            "model_profiles": ModelProfile::all_reports(),
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
        let manager = self.model_manager();
        let active: Vec<ModelKey> = manager
            .as_ref()
            .map(ModelManager::active_models)
            .unwrap_or_default();
        let weights = read(&self.weights).clone();
        let text_memory = *read(&self.text_memory);
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
                let loaded = active.contains(&key);
                let installed = weights.usable(key);
                let memory = if loaded && key == ModelKey::Text {
                    text_memory
                } else {
                    0
                };
                json!({
                    "key": key,
                    "model": model,
                    "url": url,
                    "state": if loaded {
                        "ready"
                    } else if installed {
                        "installed"
                    } else {
                        "absent"
                    },
                    "installed": installed,
                    "loaded": loaded,
                    "path": weights.weight(key).map(|path| path.display().to_string()),
                    "memory_bytes": memory,
                    "error": Value::Null,
                })
            })
            .collect()
    }

    /// What can be downloaded and what is already on disk. Read-only: this
    /// never creates the models folder or contacts the network.
    #[must_use]
    pub fn model_catalog(&self) -> Value {
        let manager = self.model_manager();
        let loaded = manager
            .as_ref()
            .map(ModelManager::active_models)
            .unwrap_or_default();
        let weights = read(&self.weights).clone();
        let entries: Vec<Value> = downloads::catalog()
            .into_iter()
            .map(|model| {
                let on_disk = downloads::installed(&self.models_dir, &model);
                let usable = weights.usable(model.key);
                let is_loaded = loaded.contains(&model.key);
                json!({
                    "key": model.key,
                    "title": model.title,
                    "repo": model.repo,
                    "source": model.source(),
                    "license": downloads::LICENSE,
                    "total_bytes": model.total_bytes(),
                    "files": model.files,
                    "installed": on_disk || usable,
                    "loaded": is_loaded,
                    "state": if is_loaded {
                        "loaded"
                    } else if on_disk || usable {
                        "installed"
                    } else {
                        "absent"
                    },
                })
            })
            .collect();
        json!({
            "models_dir": self.models_dir.display().to_string(),
            "download": self.downloads.active(),
            "pending_reload": self.pending_reload.load(Ordering::SeqCst),
            "runtime_ready": self.llama.pinned && self.llama.executable.is_some(),
            "runtime_note": self.llama.error,
            "models": entries,
        })
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
        let request = plan_request(manifest.clone(), goal.clone(), details, self.workspace());
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

        let workspace = self.workspace();
        let route = lfm3_harness::TaskRouter::route(&goal, &workspace);
        let cancel = CancellationContext::new(Duration::from_secs(u64::from(minutes) * 60));
        let lease = self.acquire_text(&cancel).await?;
        let gateway: Arc<dyn ModelGateway> = Arc::new(LeaseGateway { lease });
        let mut registry = ToolRegistry::new(&workspace)?;
        // The deterministic route decides which optional observation tools the
        // job may use. Search and related-file lookup are always available;
        // `look` and `transcribe` appear only when the route selected their
        // model, so a job cannot invent a capability the workspace lacks.
        for tool in route.tools() {
            let tool = ToolName::parse(&tool).ok_or_else(|| {
                EngineError::Setup(format!("the route named an unknown tool {tool:?}"))
            })?;
            registry = registry.grant(tool);
        }
        if let Some(manager) = self.model_manager() {
            registry = registry.with_models(manager);
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
        let workspace = self.workspace();
        let mut registry = ToolRegistry::new(&workspace)?;
        for tool in &acceptance.spec().allowed_tools {
            let name = ToolName::parse(tool).ok_or_else(|| {
                EngineError::Setup(format!("mission grants unimplemented tool {tool:?}"))
            })?;
            registry = registry.grant(name);
        }
        if let Some(manager) = self.model_manager() {
            registry = registry.with_models(manager);
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
            workspace,
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
            .model_manager()
            .ok_or_else(|| EngineError::Setup("no local model runtime is configured".to_owned()))?;
        Ok(manager.evict(key)?)
    }

    /// Switches the folder chat and jobs act in. Refused while any turn, job,
    /// or run is live so no operation can change its ground mid-flight.
    pub async fn set_workspace(&self, path: PathBuf) -> Result<PathBuf, EngineError> {
        let _guard = self.starting.lock().await;
        self.ensure_switchable().await?;
        if !path.is_absolute() {
            return Err(EngineError::InvalidInput(
                "choose an absolute folder path".to_owned(),
            ));
        }
        if !path.is_dir() {
            return Err(EngineError::InvalidInput(format!(
                "{} is not a folder",
                path.display()
            )));
        }
        let path = path.canonicalize().unwrap_or(path);
        let previous = self.workspace();
        // Take the chat gate first: if a turn slipped in, nothing has changed
        // yet and the person can simply try again.
        self.chat.set_workspace(path.clone()).await.map_err(|_| {
            EngineError::Conflict(
                "a chat turn started while the folder was changing; try again".to_owned(),
            )
        })?;
        let mut settings = self.setup_settings();
        settings.workspace = Some(path.clone());
        if let Err(error) = settings.save(&self.config.data_dir) {
            let _ = self.chat.set_workspace(previous).await;
            return Err(error.into());
        }
        *write(&self.workspace) = path.clone();
        *write(&self.setup) = settings;
        self.jobs.set_workspace(path.clone());
        Ok(path)
    }

    /// Applies downloaded weights. Refused while a model is serving work; the
    /// caller can retry once the turn ends.
    pub async fn reload_models(&self) -> Result<Vec<Value>, EngineError> {
        let _guard = self.starting.lock().await;
        self.ensure_switchable().await?;
        self.swap_installed_models().await?;
        Ok(self.models())
    }

    /// Starts one explicit, user-initiated download. Only one may run; the
    /// exact pinned files are verified before they are used.
    pub fn start_download(
        self: &Arc<Self>,
        key: ModelKey,
        emit: Arc<dyn Fn(DownloadEvent) + Send + Sync>,
    ) -> Result<(), EngineError> {
        let model = downloads::catalog_model(key)
            .ok_or_else(|| EngineError::NotFound(format!("no catalog entry for {key}")))?;
        fs::create_dir_all(&self.models_dir).map_err(|error| {
            EngineError::Setup(format!(
                "create models folder {}: {error}",
                self.models_dir.display()
            ))
        })?;
        let cancel = self.downloads.begin(key).map_err(EngineError::Conflict)?;
        let engine = Arc::clone(self);
        let root = self.models_dir.clone();
        tokio::spawn(async move {
            let on_event = |event: DownloadEvent| emit(event);
            let outcome = downloads::download_model(&root, &model, &cancel, &on_event).await;
            engine.downloads.finish(key);
            match outcome {
                Ok(_) => {
                    *write(&engine.weights) = InstalledWeights::discover(Some(&root));
                    emit(DownloadEvent::Done { key });
                    if engine.reload_models().await.is_err() {
                        engine.pending_reload.store(true, Ordering::SeqCst);
                    } else {
                        engine.pending_reload.store(false, Ordering::SeqCst);
                    }
                }
                Err(error) if error.is_cancelled() => {
                    emit(DownloadEvent::Cancelled { key });
                }
                Err(error) => {
                    emit(DownloadEvent::Failed {
                        key,
                        message: error.to_string(),
                    });
                }
            }
        });
        Ok(())
    }

    /// Stops the active download of this role, if any. Cancelling a finished
    /// or already-cancelled download is a no-op so the button can race the
    /// final event without showing a spurious error.
    pub fn cancel_download(&self, key: ModelKey) -> Result<(), EngineError> {
        self.downloads.cancel(key);
        Ok(())
    }

    /// Retries a finished download that could not be swapped in while busy.
    pub async fn apply_pending_reload(&self) {
        if !self.pending_reload.load(Ordering::SeqCst) {
            return;
        }
        if self.reload_models().await.is_ok() {
            self.pending_reload.store(false, Ordering::SeqCst);
        }
    }

    async fn ensure_switchable(&self) -> Result<(), EngineError> {
        self.ensure_idle().await?;
        self.jobs.ensure_idle().await?;
        if self.chat.is_busy().await {
            return Err(EngineError::Conflict(
                "a chat turn is still answering; stop it or wait".to_owned(),
            ));
        }
        Ok(())
    }

    async fn swap_installed_models(&self) -> Result<(), EngineError> {
        let weights = InstalledWeights::discover(Some(&self.models_dir));
        let setup = self.setup_settings();
        let new_models = build_model_manager(&self.config, &self.llama, &weights, &self.job);
        let new_speech = build_speech(&self.config, &setup, &weights);
        let new_memory = text_memory_of(&weights);
        self.chat
            .replace_runtime(new_models.clone(), new_speech.clone())
            .await
            .map_err(|_| {
                EngineError::Conflict(
                    "a chat turn started while the models were changing; try again".to_owned(),
                )
            })?;
        let old = {
            *write(&self.weights) = weights;
            *write(&self.speech) = new_speech;
            *write(&self.text_memory) = new_memory;
            std::mem::replace(&mut *write(&self.models), new_models)
        };
        if let Some(old) = old {
            if let Err(error) = old.shutdown() {
                tracing::warn!(%error, "could not stop the replaced model servers");
            }
        }
        Ok(())
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
        let manager = self.model_manager().ok_or_else(|| {
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

/// The models folder the harness searches. An explicit setup path wins, then a
/// discovered `models` folder, then trusted application data. The fallback is
/// only created when a person actually downloads a model.
fn effective_models_dir(config: &Config, setup: &SetupSettings) -> PathBuf {
    setup
        .models_dir
        .clone()
        .or_else(setup::find_models_dir)
        .unwrap_or_else(|| config.data_dir.join("models"))
}

fn build_speech(
    config: &Config,
    setup: &SetupSettings,
    weights: &InstalledWeights,
) -> SpeechConfig {
    SpeechConfig {
        executable: config
            .llama
            .tts
            .clone()
            .filter(|path| path.is_file())
            .or_else(|| setup.llama_tts.clone().filter(|path| path.is_file()))
            .or_else(setup::find_tts_executable),
        weight: weights.audio.clone(),
        projector: weights.audio_projector.clone(),
    }
}

fn text_memory_of(weights: &InstalledWeights) -> u64 {
    weights
        .text
        .as_ref()
        .and_then(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len().saturating_add(MODEL_MEMORY_HEADROOM))
        .unwrap_or(0)
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn build_model_manager(
    config: &Config,
    runtime: &LlamaRuntime,
    weights: &InstalledWeights,
    job: &ApplicationJob,
) -> Option<ModelManager> {
    let executable = runtime.executable.clone()?;
    if !runtime.pinned {
        return None;
    }
    let weight = weights.text.as_deref()?;
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
    if let (Some(audio), Some(projector)) =
        (weights.audio.as_deref(), weights.audio_projector.as_deref())
    {
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
    if let (Some(vision), Some(projector)) = (
        weights.vision.as_deref(),
        weights.vision_projector.as_deref(),
    ) {
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
    if let Some(d1) = weights.d1.as_deref() {
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
        let models_dir = directory.path().join("models");
        let engine = Engine {
            config,
            missions: BTreeMap::new(),
            workspace: RwLock::new(workspace),
            store,
            coordinator,
            models: RwLock::new(None),
            text_memory: RwLock::new(0),
            llama: LlamaRuntime {
                executable: None,
                build: 0,
                release: String::new(),
                pinned: false,
                error: None,
            },
            setup: RwLock::new(SetupSettings::default()),
            models_dir,
            weights: RwLock::new(InstalledWeights::default()),
            speech: RwLock::new(SpeechConfig::default()),
            downloads: DownloadManager::default(),
            pending_reload: AtomicBool::new(false),
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
        let fixture = fixture().await;
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
        *write(&fixture.engine.models) = Some(manager);

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

    #[tokio::test]
    async fn catalog_lists_every_pinned_role_without_touching_the_models_folder() {
        let fixture = fixture().await;
        let catalog = fixture.engine.model_catalog();

        assert_eq!(catalog["download"], Value::Null);
        assert_eq!(catalog["pending_reload"], json!(false));
        assert_eq!(
            catalog["models_dir"],
            json!(fixture.engine.models_dir().display().to_string())
        );

        let models = catalog["models"].as_array().expect("catalog models");
        assert_eq!(models.len(), 4);
        let keys: BTreeSet<&str> = models
            .iter()
            .map(|model| model["key"].as_str().expect("model key"))
            .collect();
        let mut expected = BTreeSet::new();
        expected.extend(["audio", "d1", "text", "vision"]);
        assert_eq!(keys, expected);

        for model in models {
            assert_eq!(model["state"], json!("absent"));
            assert_eq!(model["installed"], json!(false));
            assert_eq!(model["loaded"], json!(false));
            let repo = model["repo"].as_str().expect("repo");
            assert_eq!(model["source"], json!(format!("LiquidAI/{repo}")));
            assert!(model["files"]
                .as_array()
                .is_some_and(|files| !files.is_empty()));
            assert!(model["total_bytes"].as_u64().is_some_and(|bytes| bytes > 0));
        }

        assert!(
            !fixture.engine.models_dir().exists(),
            "reading the catalog must not create the models folder"
        );
    }

    #[tokio::test]
    async fn switching_workspace_moves_chat_and_jobs_and_persists_the_choice() {
        let fixture = fixture().await;
        let folder = fixture._directory.path().join("second workspace");
        fs::create_dir_all(&folder).expect("second workspace");

        let chosen = fixture
            .engine
            .set_workspace(folder.clone())
            .await
            .expect("switch workspace");
        assert_eq!(chosen, folder.canonicalize().expect("canonical"));
        assert_eq!(fixture.engine.workspace(), chosen);
        assert_eq!(fixture.engine.chat.workspace(), chosen);
        assert_eq!(fixture.engine.jobs.workspace(), chosen);

        let saved = SetupSettings::load(fixture._directory.path()).expect("saved settings");
        assert_eq!(saved.workspace.as_deref(), Some(chosen.as_path()));
    }

    #[tokio::test]
    async fn workspace_switch_rejects_non_folders_without_changing_anything() {
        let fixture = fixture().await;
        let before = fixture.engine.workspace();
        let file = fixture._directory.path().join("not-a-folder.txt");
        fs::write(&file, b"x").expect("regular file");

        assert!(fixture.engine.set_workspace(file).await.is_err());
        assert!(fixture
            .engine
            .set_workspace(PathBuf::from("relative"))
            .await
            .is_err());

        assert_eq!(fixture.engine.workspace(), before);
        assert_eq!(fixture.engine.chat.workspace(), before);
        assert_eq!(fixture.engine.jobs.workspace(), before);
        let saved = SetupSettings::load(fixture._directory.path()).expect("saved settings");
        assert!(
            saved.workspace.is_none(),
            "a refused switch persists nothing"
        );
    }
}
