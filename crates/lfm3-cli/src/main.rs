use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use clap::{Parser, Subcommand};
use lfm3_cli::{doctor, events, fixture::DeskFixture, live};
use lfm3_harness::{
    agent::{planner::plan_request, recovery::Budget, AgentLoop, LoopOptions, LoopStatus},
    mission::{load_missions, InputArtifact},
    ArtifactId, Config, Executor, FrozenAcceptance, MissionManifest, ModelGateway, PlanDraft,
    Planner, RunCoordinator, RunId, RunStatus, Runner, Store, ToolName, ToolRegistry,
};
use serde_json::{json, Value};

#[derive(Debug, Parser)]
#[command(name = "lfm3", version, about = "Local, purpose-bound LFM3 harness")]
struct Cli {
    /// Directory of mission manifests.
    #[arg(long, global = true, default_value = "missions")]
    missions: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report resolved configuration without loading a model.
    Doctor {
        /// Probe the pinned llama.cpp runtime and each local model endpoint.
        #[arg(long)]
        probe_models: bool,
    },
    /// List validated mission manifests.
    Missions,
    /// Report configured model endpoints and local weights.
    Models {
        /// Probe each local endpoint for its served model identity.
        #[arg(long)]
        probe: bool,
    },
    /// Draft and validate a plan without creating a run.
    Plan(PlanArgs),
    /// Plan, execute, verify, retry, and report one run.
    Run(RunArgs),
    /// Show the durable journal state of a run.
    Status(StatusArgs),
    /// Create a linked run with the same frozen acceptance contract.
    Retry(RetryArgs),
    /// Plan and verify a general task stated in plain language.
    Task(TaskArgs),
    /// Run a stated job as an agent loop until its required output verifies.
    Job(JobArgs),
}

#[derive(Debug, Parser)]
struct PlanArgs {
    /// Mission name from the missions directory.
    #[arg(long)]
    mission: String,
    /// Acceptance fixture JSON (alias: --input).
    #[arg(long, alias = "input")]
    fixture: Option<PathBuf>,
    /// Goal text when no fixture is used.
    #[arg(long)]
    goal: Option<String>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[cfg(feature = "fake-model")]
    /// Test-only scripted model; excluded from packaged builds.
    #[arg(long)]
    fake_model: bool,
    /// Reuse an already-running loopback llama-server instead of owning one.
    #[arg(long)]
    external: bool,
    /// Seconds allowed for model-server startup.
    #[arg(long, default_value_t = 120)]
    startup_timeout: u64,
}

#[derive(Debug, Parser)]
struct RunArgs {
    /// Mission name from the missions directory.
    #[arg(long)]
    mission: String,
    /// Acceptance fixture JSON (alias: --input).
    #[arg(long, alias = "input")]
    fixture: Option<PathBuf>,
    /// Goal text when no fixture is used.
    #[arg(long)]
    goal: Option<String>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    db: Option<PathBuf>,
    /// Durable JSON-lines events: `-` for stdout or a file path.
    #[arg(long)]
    events: Option<String>,
    /// Whole-run deadline in seconds.
    #[arg(long)]
    timeout: Option<u64>,
    #[cfg(feature = "fake-model")]
    /// Test-only scripted model; excluded from packaged builds.
    #[arg(long)]
    fake_model: bool,
    /// Reuse an already-running loopback llama-server instead of owning one.
    #[arg(long)]
    external: bool,
    /// Seconds allowed for model-server startup.
    #[arg(long, default_value_t = 120)]
    startup_timeout: u64,
}

#[derive(Debug, Parser)]
struct StatusArgs {
    /// Run id; defaults to the most recently created run.
    run: Option<String>,
    #[arg(long)]
    db: Option<PathBuf>,
    /// Include the durable event journal in the report.
    #[arg(long)]
    events: bool,
}

#[derive(Debug, Parser)]
struct RetryArgs {
    /// Run id to retry; defaults to the most recently created run.
    run: Option<String>,
    #[arg(long)]
    db: Option<PathBuf>,
    /// Workspace to reuse; required because the journal stores no path.
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    events: Option<String>,
    #[arg(long)]
    timeout: Option<u64>,
    #[arg(long)]
    external: bool,
    #[arg(long, default_value_t = 120)]
    startup_timeout: u64,
}

/// A job runs as a loop: plan, act, observe, repeat, verify at finish.
#[derive(Debug, Parser)]
struct JobArgs {
    /// What to accomplish, in plain language.
    #[arg(long)]
    goal: String,
    /// Required output file, relative to the workspace. Detected from the goal
    /// when omitted.
    #[arg(long)]
    deliverable: Option<String>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Whole-job budget in minutes; the loop stops at the deadline.
    #[arg(long, default_value_t = 30)]
    minutes: u64,
    /// Most model turns before the loop stops.
    #[arg(long, default_value_t = 40)]
    max_turns: u32,
    /// Tool calls accepted in one turn.
    #[arg(long, default_value_t = 3)]
    max_calls: u32,
    /// JSON-lines trace: `-` for stdout or a file path.
    #[arg(long)]
    events: Option<String>,
    /// Whole-run deadline in seconds; overrides `--minutes`.
    #[arg(long)]
    timeout: Option<u64>,
    /// Reuse an already-running loopback llama-server instead of owning one.
    #[arg(long)]
    external: bool,
    /// Seconds allowed for model-server startup.
    #[arg(long, default_value_t = 120)]
    startup_timeout: u64,
}

#[derive(Debug, Parser)]
struct TaskArgs {
    /// What to accomplish, in plain language.
    #[arg(long)]
    goal: String,
    /// Optional required output file, relative to the workspace.
    #[arg(long)]
    deliverable: Option<String>,
    /// Draft and print the plan; execute nothing.
    #[arg(long)]
    plan: bool,
    /// Whole-job budget in minutes (task runs only).
    #[arg(long, default_value_t = 30)]
    minutes: u64,
    /// Most steps the plan may contain.
    #[arg(long, default_value_t = 16)]
    steps: usize,
    /// Fewest steps a plan may contain; a job must not collapse to one action.
    #[arg(long, default_value_t = 2)]
    min_steps: usize,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    db: Option<PathBuf>,
    /// Durable JSON-lines events: `-` for stdout or a file path.
    #[arg(long)]
    events: Option<String>,
    /// Whole-run deadline in seconds.
    #[arg(long)]
    timeout: Option<u64>,
    /// Reuse an already-running loopback llama-server instead of owning one.
    #[arg(long)]
    external: bool,
    /// Seconds allowed for model-server startup.
    #[arg(long, default_value_t = 120)]
    startup_timeout: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    match dispatch().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch() -> Result<ExitCode, String> {
    let cli = Cli::parse();
    let config = Config::load().map_err(|error| error.to_string())?;
    config
        .validate_local(config.profile)
        .map_err(|error| error.to_string())?;

    match cli.command {
        Command::Doctor { probe_models } => {
            let report = doctor::report(probe_models).await?;
            print_json(&report)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Missions => list_missions(&cli.missions),
        Command::Models { probe } => models(&config, probe).await,
        Command::Plan(args) => plan(&config, &cli.missions, args).await,
        Command::Run(args) => run_mission(&config, &cli.missions, args).await,
        Command::Status(args) => status(&config, args).await,
        Command::Retry(args) => retry(&config, args).await,
        Command::Task(args) => task(&config, args).await,
        Command::Job(args) => job(&config, args).await,
    }
}

fn list_missions(directory: &Path) -> Result<ExitCode, String> {
    let missions = load_missions(directory).map_err(|error| error.to_string())?;
    let values = missions
        .values()
        .map(|mission| {
            json!({
                "name": mission.name,
                "version": mission.version,
                "title": mission.title,
                "description": mission.description,
                "tools": mission.tools,
                "checks": mission.checks,
                "output": mission.output,
                "limits": mission.limits,
            })
        })
        .collect::<Vec<_>>();
    print_json(&Value::Array(values))?;
    Ok(ExitCode::SUCCESS)
}

async fn models(config: &Config, probe: bool) -> Result<ExitCode, String> {
    let (root, weights) = doctor::scan_weights();
    let text_weight = doctor::find_weight(&root, &weights, "LFM3-1.7B");
    let runtime = doctor::probe_runtime(config);
    let endpoints = if probe {
        doctor::probe(config).await
    } else {
        json!({
            "text": {"url": config.text_url.as_str(), "model": config.text_model},
            "vision": {"url": config.vision_url.as_str(), "model": config.vision_model},
            "audio": {"url": config.audio_url.as_str(), "model": config.audio_model},
            "d1": {"url": config.d1_url.as_str(), "model": config.d1_model},
        })
    };
    print_json(&json!({
        "runtime": runtime,
        "weights_root": doctor::display_path(&root),
        "weights": weights,
        "text_weight": text_weight.as_ref().map(|path| doctor::display_path(path)),
        "endpoints": endpoints,
    }))?;
    Ok(ExitCode::SUCCESS)
}

async fn plan(config: &Config, missions_dir: &Path, args: PlanArgs) -> Result<ExitCode, String> {
    let fixture = load_fixture(args.fixture.as_ref())?;
    let mission = select_mission(config, missions_dir, &args.mission, fixture.as_ref())?;
    let workspace = prepare_workspace(
        fixture.as_ref(),
        resolve_workspace(args.workspace.clone(), fixture.as_ref(), config),
    )?;
    let context = run_context(fixture.as_ref(), args.goal.clone(), &workspace)?;

    #[cfg(feature = "fake-model")]
    if args.fake_model {
        let fixture = fixture
            .as_ref()
            .ok_or_else(|| "--fake-model needs --fixture".to_owned())?;
        let gateway: Arc<dyn ModelGateway> =
            Arc::new(lfm3_cli::fake::FakeGateway::new(fixture.clone()));
        return draft_plan(config, mission, context, workspace, gateway).await;
    }

    let live = live::connect(
        config,
        args.external,
        Duration::from_secs(args.startup_timeout),
    )
    .await?;
    let result = draft_plan(config, mission, context, workspace, live.gateway()).await;
    live.shutdown()?;
    result
}

async fn draft_plan(
    config: &Config,
    mission: MissionManifest,
    context: RunContext,
    workspace: PathBuf,
    gateway: Arc<dyn ModelGateway>,
) -> Result<ExitCode, String> {
    let cancel = lfm3_harness::CancellationContext::new(Duration::from_secs(
        config.limits.run_timeout_seconds,
    ));
    let planner = Planner::new(gateway).with_max_tokens(1536);
    let plan = planner
        .draft(
            plan_request(
                mission.clone(),
                context.goal.clone(),
                context.details,
                workspace.clone(),
            ),
            &cancel,
        )
        .await
        .map_err(|error| error.to_string())?;
    print_json(&json!({
        "mission": mission.name,
        "goal": context.goal,
        "workspace": workspace,
        "steps": plan.steps,
        "note": "fixture checks are appended when a run freezes acceptance",
    }))?;
    Ok(ExitCode::SUCCESS)
}

async fn run_mission(
    config: &Config,
    missions_dir: &Path,
    args: RunArgs,
) -> Result<ExitCode, String> {
    let fixture = load_fixture(args.fixture.as_ref())?;
    let mission = select_mission(config, missions_dir, &args.mission, fixture.as_ref())?;
    let workspace = prepare_workspace(
        fixture.as_ref(),
        resolve_workspace(args.workspace.clone(), fixture.as_ref(), config),
    )?;
    let context = run_context(fixture.as_ref(), args.goal.clone(), &workspace)?;

    #[cfg(feature = "fake-model")]
    if args.fake_model {
        let fixture = fixture
            .as_ref()
            .ok_or_else(|| "--fake-model needs --fixture".to_owned())?;
        let gateway: Arc<dyn ModelGateway> =
            Arc::new(lfm3_cli::fake::FakeGateway::new(fixture.clone()));
        return execute_run(
            config,
            mission,
            Some(fixture),
            context,
            workspace,
            gateway,
            "fake-model".to_owned(),
            None,
            args.db,
            args.events,
            args.timeout,
        )
        .await;
    }

    let live = live::connect(
        config,
        args.external,
        Duration::from_secs(args.startup_timeout),
    )
    .await?;
    let model = live::describe(&live);
    let result = execute_run(
        config,
        mission,
        fixture.as_ref(),
        context,
        workspace,
        live.gateway(),
        live.identity.clone(),
        Some(model),
        args.db,
        args.events,
        args.timeout,
    )
    .await;
    live.shutdown()?;
    result
}

#[allow(clippy::too_many_arguments)]
async fn execute_run(
    config: &Config,
    mission: MissionManifest,
    fixture: Option<&DeskFixture>,
    context: RunContext,
    workspace: PathBuf,
    gateway: Arc<dyn ModelGateway>,
    identity: String,
    model_report: Option<Value>,
    db: Option<PathBuf>,
    event_target: Option<String>,
    timeout: Option<u64>,
) -> Result<ExitCode, String> {
    let store = open_store(config, db.as_deref()).await?;
    let reconciled = store
        .reconcile_interrupted()
        .await
        .map_err(|error| error.to_string())?;

    let deadline = Duration::from_secs(timeout.unwrap_or(config.limits.run_timeout_seconds));
    let cancel = lfm3_harness::CancellationContext::new(deadline);
    let signal = spawn_cancel_on_ctrl_c(cancel.clone());

    let planner = Planner::new(Arc::clone(&gateway)).with_max_tokens(1536);
    let mut plan = planner
        .draft(
            plan_request(
                mission.clone(),
                context.goal.clone(),
                context.details,
                workspace.clone(),
            ),
            &cancel,
        )
        .await
        .map_err(|error| error.to_string())?;
    let acceptance = freeze_acceptance(
        &mission,
        fixture,
        context.goal,
        context.artifacts,
        &mut plan,
    )?;

    let record = store
        .create_run(&acceptance, None)
        .await
        .map_err(|error| error.to_string())?;

    let target = event_target.as_deref().map(events::EventTarget::parse);
    if let Some(target) = &target {
        events::check_target(target)?;
    }
    let stream = match &target {
        Some(target) => Some(events::EventStream::spawn(
            store.clone(),
            record.id.clone(),
            target,
        )?),
        None => None,
    };
    let streams_to_stdout = target.as_ref().is_some_and(events::EventTarget::is_stdout);

    let executor = Executor::new(
        Arc::clone(&gateway),
        Arc::new(tool_registry(&workspace, &mission)?),
        identity.clone(),
    );
    let runner = Runner::new(
        store.clone(),
        acceptance.clone(),
        workspace.clone(),
        Arc::new(executor),
        cancel.clone(),
    )
    .map_err(|error| error.to_string())?
    .with_budget(budget(config));
    let outcome = RunCoordinator::new(store.clone())
        .start(record.id.clone(), runner)
        .await
        .map_err(|error| error.to_string())?;

    if let Some(stream) = stream {
        stream.finish().await?;
    }
    signal.abort();

    let snapshot = store
        .snapshot(&record.id)
        .await
        .map_err(|error| error.to_string())?;
    let journal = events::read_events(&store, &record.id).await?;
    let checks = journal
        .iter()
        .filter(|event| event.kind == "check.result")
        .map(|event| event.payload.clone())
        .collect::<Vec<_>>();
    let report = json!({
        "run": snapshot.run,
        "steps": snapshot.steps,
        "checks": checks,
        "outcome": outcome,
        "acceptance_hash": acceptance.hash(),
        "workspace": doctor::display_path(&workspace),
        "database": doctor::display_path(store.database_path()),
        "events": journal.len(),
        "reconciled": {
            "runs": reconciled.runs,
            "steps": reconciled.steps,
            "attempts": reconciled.attempts,
        },
        "model": model_report.unwrap_or_else(|| json!({"identity": identity})),
        "fixture": fixture.map(fixture_report),
        "output": fixture.map(|fixture| output_report(fixture, &workspace)),
    });
    if streams_to_stdout {
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    } else {
        print_json(&report)?;
    }

    Ok(if outcome.status == RunStatus::Passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Runs one stated job as a model-driven loop.
///
/// The model keeps its own todo list, acts, and sees the real tool output before
/// its next turn. The job is only done when the required output exists and is
/// non-empty; a model that merely says "finished" is refused and told to keep
/// working.
async fn job(config: &Config, args: JobArgs) -> Result<ExitCode, String> {
    let requested = args
        .workspace
        .clone()
        .or_else(|| config.workspace.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    let workspace = std::fs::canonicalize(&requested)
        .map_err(|error| format!("workspace {}: {error}", requested.display()))?;
    let route = lfm3_harness::TaskRouter::route(&args.goal, &workspace);
    let deliverable = args
        .deliverable
        .clone()
        .or_else(|| lfm3_harness::task::detect_deliverable(&args.goal));
    let deadline = args
        .timeout
        .unwrap_or_else(|| args.minutes.clamp(1, 24 * 60) * 60);
    let cancel = lfm3_harness::CancellationContext::new(Duration::from_secs(deadline));
    let signal = spawn_cancel_on_ctrl_c(cancel.clone());
    let live = live::connect(
        config,
        args.external,
        Duration::from_secs(args.startup_timeout),
    )
    .await?;

    let mut registry = ToolRegistry::new(&workspace).map_err(|error| error.to_string())?;
    for tool in route.tools() {
        registry = registry.grant(parse_tool(&tool)?);
    }
    if let Some(manager) = live.manager() {
        registry = registry.with_models(manager.clone());
    }
    let mut sink = TraceSink::new(args.events.as_deref())?;
    let runner = AgentLoop::new(live.gateway(), Arc::new(registry), live.identity.clone())
        .with_options(LoopOptions {
            max_turns: args.max_turns.max(1),
            max_calls: args.max_calls.clamp(1, 4),
            replay: 8,
        })
        .with_max_tokens(2048);
    let result = {
        let mut on_event = |value: Value| sink.write(&value);
        runner
            .run(
                &args.goal,
                deliverable.as_deref(),
                &workspace,
                &cancel,
                &mut on_event,
            )
            .await
    };
    signal.abort();
    live.shutdown()?;
    let report = result.map_err(|error| format!("job failed: {error}"))?;
    print_json(&json!({
        "job": report.report(),
        "routing": route.report(),
        "workspace": doctor::display_path(&workspace),
        "trace": sink.count(),
    }))?;
    Ok(if report.status == LoopStatus::Passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Writes one JSON line per loop event, to stdout or to a file.
struct TraceSink {
    file: Option<std::fs::File>,
    stdout: bool,
    count: usize,
}

impl TraceSink {
    fn new(target: Option<&str>) -> Result<Self, String> {
        match target {
            None => Ok(Self {
                file: None,
                stdout: false,
                count: 0,
            }),
            Some("-") => Ok(Self {
                file: None,
                stdout: true,
                count: 0,
            }),
            Some(path) => {
                let file = std::fs::File::create(path)
                    .map_err(|error| format!("events {path}: {error}"))?;
                Ok(Self {
                    file: Some(file),
                    stdout: false,
                    count: 0,
                })
            }
        }
    }

    fn write(&mut self, value: &Value) {
        use std::io::Write;
        let mut line = serde_json::to_string(value).unwrap_or_default();
        line.push('\n');
        self.count += 1;
        if let Some(file) = self.file.as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
        if self.stdout {
            print!("{line}");
        }
    }

    fn count(&self) -> usize {
        self.count
    }
}

/// Routes the goal across the local models, drafts a checked plan, and runs it.
///
/// The plan is derived from the goal, not from a mission file; verification is
/// the same frozen per-step contract the mission runner uses.
async fn task(config: &Config, args: TaskArgs) -> Result<ExitCode, String> {
    let requested = args
        .workspace
        .clone()
        .or_else(|| config.workspace.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    let workspace = std::fs::canonicalize(&requested)
        .map_err(|error| format!("workspace {}: {error}", requested.display()))?;

    let live = live::connect(
        config,
        args.external,
        Duration::from_secs(args.startup_timeout),
    )
    .await?;
    let deadline = args
        .timeout
        .unwrap_or_else(|| args.minutes.clamp(1, 24 * 60) * 60);
    let cancel = lfm3_harness::CancellationContext::new(Duration::from_secs(deadline));
    let signal = spawn_cancel_on_ctrl_c(cancel.clone());
    let steps = args.steps.clamp(1, 32);
    let min_steps = args.min_steps.clamp(1, steps);
    let request = lfm3_harness::TaskRequest::new(args.goal, workspace.clone())
        .with_deliverable(args.deliverable)
        .with_max_steps(Some(steps))
        .with_min_steps(Some(min_steps));
    let planned = lfm3_harness::task::draft_task(live.gateway(), request, &cancel).await;
    let plan = match planned {
        Ok(plan) => plan,
        Err(error) => {
            signal.abort();
            live.shutdown()?;
            return Err(format!("task planning failed: {error}"));
        }
    };
    if args.plan {
        print_json(&plan.report())?;
        signal.abort();
        live.shutdown()?;
        return Ok(ExitCode::SUCCESS);
    }

    let store = open_store(config, args.db.as_deref()).await?;
    store
        .reconcile_interrupted()
        .await
        .map_err(|error| error.to_string())?;
    let record = store
        .create_run(&plan.acceptance, None)
        .await
        .map_err(|error| error.to_string())?;
    let target = args.events.as_deref().map(events::EventTarget::parse);
    if let Some(target) = &target {
        events::check_target(target)?;
    }
    let stream = match &target {
        Some(target) => Some(events::EventStream::spawn(
            store.clone(),
            record.id.clone(),
            target,
        )?),
        None => None,
    };

    let mut registry = ToolRegistry::new(&workspace).map_err(|error| error.to_string())?;
    for tool in &plan.manifest.tools {
        registry = registry.grant(parse_tool(tool)?);
    }
    if let Some(manager) = live.manager() {
        registry = registry.with_models(manager.clone());
    }
    // One action may carry a whole file, so a job needs more room than the
    // mission executor's 512-token reply.
    let executor = Executor::new(live.gateway(), Arc::new(registry), live.identity.clone())
        .with_max_tokens(2048)
        .with_max_calls(3);
    let runner = Runner::new(
        store.clone(),
        plan.acceptance.clone(),
        workspace.clone(),
        Arc::new(executor),
        cancel,
    )
    .map_err(|error| error.to_string())?
    .with_budget(task_budget());
    let outcome = RunCoordinator::new(store.clone())
        .start(record.id.clone(), runner)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(stream) = stream {
        stream.finish().await?;
    }
    signal.abort();
    live.shutdown()?;

    let snapshot = store
        .snapshot(&record.id)
        .await
        .map_err(|error| error.to_string())?;
    let identity = live.identity.clone();
    let owned = live.owned;
    print_json(&json!({
        "run": snapshot.run,
        "steps": snapshot.steps,
        "outcome": outcome,
        "route": plan.route.report(),
        "acceptance_hash": plan.acceptance.hash(),
        "workspace": doctor::display_path(&workspace),
        "model": { "identity": identity, "owned": owned },
    }))?;
    Ok(if outcome.status == RunStatus::Passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn status(config: &Config, args: StatusArgs) -> Result<ExitCode, String> {
    let store = open_store(config, args.db.as_deref()).await?;
    let run_id = match &args.run {
        Some(raw) => parse_run_id(raw)?,
        None => latest_run_id(&store).await?,
    };
    let snapshot = store
        .snapshot(&run_id)
        .await
        .map_err(|error| error.to_string())?;
    let mut report = json!({
        "run": snapshot.run,
        "steps": snapshot.steps,
    });
    if args.events {
        let journal = events::read_events(&store, &run_id).await?;
        report["events"] = json!(journal);
    }
    print_json(&report)?;
    Ok(ExitCode::SUCCESS)
}

async fn retry(config: &Config, args: RetryArgs) -> Result<ExitCode, String> {
    let store = open_store(config, args.db.as_deref()).await?;
    let parent = match &args.run {
        Some(raw) => parse_run_id(raw)?,
        None => latest_run_id(&store).await?,
    };
    let acceptance = store
        .run_acceptance(parent.clone())
        .await
        .map_err(|error| error.to_string())?;
    let workspace = args
        .workspace
        .clone()
        .or_else(|| config.workspace.clone())
        .ok_or_else(|| {
            "retry needs --workspace or LFM3_WORKSPACE; the journal stores no workspace path"
                .to_owned()
        })?;
    let workspace = std::fs::canonicalize(&workspace)
        .map_err(|error| format!("workspace {}: {error}", workspace.display()))?;

    let live = live::connect(
        config,
        args.external,
        Duration::from_secs(args.startup_timeout),
    )
    .await?;
    let cancel = lfm3_harness::CancellationContext::new(Duration::from_secs(
        args.timeout.unwrap_or(config.limits.run_timeout_seconds),
    ));
    let signal = spawn_cancel_on_ctrl_c(cancel.clone());

    let record = store
        .create_run(&acceptance, Some(parent.clone()))
        .await
        .map_err(|error| error.to_string())?;
    let target = args.events.as_deref().map(events::EventTarget::parse);
    if let Some(target) = &target {
        events::check_target(target)?;
    }
    let stream = match &target {
        Some(target) => Some(events::EventStream::spawn(
            store.clone(),
            record.id.clone(),
            target,
        )?),
        None => None,
    };

    let executor = Executor::new(
        live.gateway(),
        Arc::new(registry_for(&workspace, &acceptance)?),
        live.identity.clone(),
    );
    let runner = Runner::new(
        store.clone(),
        acceptance.clone(),
        workspace.clone(),
        Arc::new(executor),
        cancel,
    )
    .map_err(|error| error.to_string())?
    .with_budget(budget(config));
    let outcome = RunCoordinator::new(store.clone())
        .start(record.id.clone(), runner)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(stream) = stream {
        stream.finish().await?;
    }
    signal.abort();
    live.shutdown()?;

    let snapshot = store
        .snapshot(&record.id)
        .await
        .map_err(|error| error.to_string())?;
    print_json(&json!({
        "run": snapshot.run,
        "steps": snapshot.steps,
        "outcome": outcome,
        "acceptance_hash": acceptance.hash(),
        "parent_run_id": parent,
        "workspace": doctor::display_path(&workspace),
    }))?;
    Ok(if outcome.status == RunStatus::Passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn latest_run_id(store: &Store) -> Result<RunId, String> {
    store
        .latest_run()
        .await
        .map_err(|error| error.to_string())?
        .map(|record| record.id)
        .ok_or_else(|| "no runs exist in this database".to_owned())
}

fn load_fixture(path: Option<&PathBuf>) -> Result<Option<DeskFixture>, String> {
    path.map(DeskFixture::load)
        .transpose()
        .map_err(|error| error.to_string())
}

fn select_mission(
    config: &Config,
    directory: &Path,
    name: &str,
    fixture: Option<&DeskFixture>,
) -> Result<MissionManifest, String> {
    let missions = load_missions(directory).map_err(|error| error.to_string())?;
    let mission = missions
        .get(name)
        .cloned()
        .ok_or_else(|| format!("mission {name:?} is not in {}", directory.display()))?;
    let mut mission = match fixture {
        Some(fixture) => fixture.mission(&mission),
        None => mission,
    };
    if fixture.is_some() {
        // The fixture freezes the final contract and embeds the verified
        // source text in the goal, so one grounded write is the whole plan.
        mission.limits.max_steps = 1;
    }
    mission.limits.max_steps = mission.limits.max_steps.min(config.limits.max_steps.max(1));
    mission.limits.max_attempts = mission
        .limits
        .max_attempts
        .min(config.limits.max_attempts_per_strategy.max(1));
    Ok(mission)
}

fn resolve_workspace(
    requested: Option<PathBuf>,
    fixture: Option<&DeskFixture>,
    config: &Config,
) -> PathBuf {
    requested
        .or_else(|| fixture.map(|_| PathBuf::from("tests/fixtures/desk_task/workspace")))
        .or_else(|| config.workspace.clone())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn prepare_workspace(fixture: Option<&DeskFixture>, workspace: PathBuf) -> Result<PathBuf, String> {
    match fixture {
        Some(fixture) => fixture.prepare_workspace(workspace),
        None => std::fs::canonicalize(&workspace)
            .map_err(|error| format!("workspace {}: {error}", workspace.display())),
    }
}

fn run_context(
    fixture: Option<&DeskFixture>,
    goal_arg: Option<String>,
    workspace: &Path,
) -> Result<RunContext, String> {
    let goal = match (fixture, goal_arg) {
        (Some(fixture), _) => fixture.acceptance_goal(),
        (None, Some(goal)) if !goal.trim().is_empty() => goal,
        _ => {
            return Err(
                "provide --goal or --fixture; the desk_task mission needs a goal".to_owned(),
            )
        }
    };
    let mut details = BTreeMap::new();
    let mut artifacts = BTreeMap::new();
    if let Some(fixture) = fixture {
        let digest = fixture.source_digest(workspace)?;
        let id: ArtifactId = digest.sha256[..32]
            .parse()
            .map_err(|error| format!("source artifact id: {error}"))?;
        details.insert(
            "source".to_owned(),
            format!("{} (sha256 {})", fixture.source, digest.sha256),
        );
        artifacts.insert(
            "source".to_owned(),
            InputArtifact {
                id,
                sha256: digest.sha256,
            },
        );
    }
    Ok(RunContext {
        goal,
        details,
        artifacts,
    })
}

/// Everything a run needs to describe itself to the planner.
struct RunContext {
    goal: String,
    details: BTreeMap<String, String>,
    artifacts: BTreeMap<String, InputArtifact>,
}

fn freeze_acceptance(
    mission: &MissionManifest,
    fixture: Option<&DeskFixture>,
    goal: String,
    artifacts: BTreeMap<String, InputArtifact>,
    plan: &mut PlanDraft,
) -> Result<FrozenAcceptance, String> {
    if let Some(fixture) = fixture {
        lfm3_cli::fixture::append_fixture_checks(plan, fixture)?;
    }
    mission
        .compile_acceptance(goal, artifacts, plan.clone())
        .map_err(|error| error.to_string())
}

fn tool_registry(workspace: &Path, mission: &MissionManifest) -> Result<ToolRegistry, String> {
    let mut registry = ToolRegistry::new(workspace).map_err(|error| error.to_string())?;
    for tool in &mission.tools {
        registry = registry.grant(parse_tool(tool)?);
    }
    Ok(registry)
}

fn registry_for(workspace: &Path, acceptance: &FrozenAcceptance) -> Result<ToolRegistry, String> {
    let mut registry = ToolRegistry::new(workspace).map_err(|error| error.to_string())?;
    for tool in &acceptance.spec().allowed_tools {
        registry = registry.grant(parse_tool(tool)?);
    }
    Ok(registry)
}

fn parse_tool(raw: &str) -> Result<ToolName, String> {
    ToolName::parse(raw).ok_or_else(|| format!("tool {raw:?} is not implemented"))
}

fn budget(config: &Config) -> Budget {
    let attempts = u32::try_from(config.limits.max_attempts_per_strategy)
        .unwrap_or(1)
        .max(1);
    let strategies = u32::try_from(config.limits.max_strategies)
        .unwrap_or(1)
        .max(1);
    let total = u32::try_from(config.limits.max_total_attempts)
        .unwrap_or(attempts)
        .max(attempts);
    Budget {
        attempts_per_strategy: attempts,
        strategies,
        total_attempts: total,
    }
}

/// A long job may retry far more than a single-action mission step.
fn task_budget() -> Budget {
    Budget {
        attempts_per_strategy: 4,
        strategies: 3,
        total_attempts: 16,
    }
}

async fn open_store(config: &Config, db: Option<&Path>) -> Result<Store, String> {
    let path = db
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.data_dir.join("runs.db"));
    Store::open(&path).await.map_err(|error| error.to_string())
}

fn parse_run_id(raw: &str) -> Result<RunId, String> {
    raw.parse::<RunId>().map_err(|error| error.to_string())
}

fn fixture_report(fixture: &DeskFixture) -> Value {
    json!({
        "source": fixture.source,
        "output": fixture.output,
        "required_terms": fixture.required_terms,
        "forbidden_terms": fixture.forbidden_terms,
    })
}

fn output_report(fixture: &DeskFixture, workspace: &Path) -> Value {
    let path = workspace.join(&fixture.output);
    let observed = std::fs::read(&path).ok().map(|bytes| {
        json!({
            "path": fixture.output,
            "sha256": lfm3_harness::store::sha256_hex(&bytes),
            "bytes": bytes.len(),
        })
    });
    let expected = fixture.expected_summary_sha256().ok();
    let matches = matches!(
        (&observed, &expected),
        (Some(observed), Some(expected))
            if observed.get("sha256").and_then(Value::as_str) == Some(expected.as_str())
    );
    json!({
        "observed": observed,
        "expected_sha256": expected,
        "matches_expected": matches,
    })
}

fn spawn_cancel_on_ctrl_c(
    cancel: lfm3_harness::CancellationContext,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    })
}

fn print_json(value: &Value) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| format!("serialize report: {error}"))?
    );
    Ok(())
}
