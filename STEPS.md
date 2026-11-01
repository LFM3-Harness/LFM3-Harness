# Rust-Only LFM3 Local Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Review every gate before starting the next task.

**Goal:** Build a fully local, chat-first LFM3 desktop cowork harness with a Rust engine, React UI, Tauri 2 shell, and unchanged external llama.cpp processes, while preserving the verified mission engine (frozen acceptance, provenance-bound evidence, retries).

**Architecture:** The `lfm3-harness` Rust crate owns planning, execution, verification, persistence, recovery, local model management, and the durable chat transcript. The Tauri application embeds that crate and adds the chat turn loop, approval waits, typed commands/events, and the chat-first UI. The `lfm3-cli` binary uses the same crate headlessly. No Python, production HTTP engine, sidecar, or llama.cpp FFI is used.

**Tech Stack:** Stable Rust (MSVC on Windows, GNU on Linux), Tokio, Serde, Reqwest (streaming), Rusqlite, Tauri 2, React, TypeScript, Vite, Playwright, llama.cpp b11960 / v0.8.0.

**Spec:** `PLAN.md`

> **Platform update (post-plan):** Linux x86_64 desktop support was added after Task 18. The Tauri UI now builds as `.deb` and `.AppImage`, uses Linux process groups instead of a Windows Job Object, stores state under XDG paths, and ships the Linux llama.cpp runtime. The Windows-first steps below are the historical implementation order; see `README.md` and `docs/releases/v0.1.0.md` for the current cross-platform commands.

## Global Constraints

- Windows 11 and Linux x86_64 are the desktop targets; use stable Rust with the MSVC toolchain on Windows and the GNU toolchain on Linux.
- llama.cpp remains pinned to b11960 / v0.8.0 until every model fixture qualifies a newer build.
- Packaged model endpoints must resolve to `127.0.0.1` or `::1`.
- Chat opens by default and never counts as verified work; only frozen mission checks may pass a run.
- Every chat write or edit needs an explicit per-action approval and leaves the file untouched on denial or expiry.
- Voice is captured and transcribed locally; no audio upload or remote speech recognition.
- No Python source, interpreter, build dependency, test runner, or maintenance script.
- No unrestricted planner-authored shell, cloud fallback, telemetry, browsing, or hidden downloads.
- One active run per engine and one active chat turn; model inference is sequential and lease-controlled.
- Replanning changes `ExecutionStrategy` only. `AcceptanceSpec` and its hash are immutable.
- A run passes only after all frozen step checks and final mission verification pass with typed evidence.
- Cancellation propagates through every model, tool, approval, chat turn, and child-process call.
- Trusted state lives under `%LOCALAPPDATA%\LFM3Harness` on Windows and `$XDG_DATA_HOME/LFM3Harness` (or `~/.local/share/LFM3Harness`) on Linux, not inside a mutable workspace.
- The current directory has no git repository. Do not create commits unless the user separately initializes version control and requests commits.
- Complete tasks in order. Do not start React before Task 11 passes with the real text model.

---

### Task 1: Native Rust workspace and configuration

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `crates/lfm3-harness/Cargo.toml`
- Create: `crates/lfm3-harness/src/lib.rs`, `config.rs`, `error.rs`, `ids.rs`
- Create: `crates/lfm3-cli/Cargo.toml`, `crates/lfm3-cli/src/main.rs`
- Test: `crates/lfm3-harness/tests/config.rs`

**Interfaces:**
- Produces: `Config::load`, `Config::validate_local`, `HarnessError`, `RunId`, `StepId`, `AttemptId`, `ArtifactId`.
- Later tasks consume these types instead of raw strings for identifiers and configuration errors.

- [ ] **Step 1: Write the failing configuration test**

```rust
#[test]
fn packaged_profile_rejects_remote_model_endpoints() {
    let mut config = Config::default();
    config.text_url = "https://example.com/v1".parse().unwrap();
    let error = config.validate_local(Profile::Packaged).unwrap_err();
    assert!(matches!(error, HarnessError::RemoteEndpointRejected(_)));
}
```

- [ ] **Step 2: Run the test and confirm the missing native API failure**

Run: `cargo test -p lfm3-harness --test config`  
Expected: compilation failure because `Config`, `Profile`, and `HarnessError` do not exist.

- [ ] **Step 3: Create the minimal workspace and public types**

```rust
pub fn is_loopback(url: &reqwest::Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("remote model endpoint rejected: {0}")]
    RemoteEndpointRejected(String),
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
}
```

The CLI initially supports `doctor`; it prints resolved configuration and performs no model download.

- [ ] **Step 4: Run native quality gates**

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p lfm3-cli -- doctor
```

Expected: all tests pass; `doctor` reports configuration and llama.cpp paths without contacting a remote service.

- [ ] **Step 5: Review the gate and update baseline documentation**

Record the Rust, Cargo, and llama.cpp versions in `README.md`. Confirm no build script invokes Python.

---

### Task 2: Mission manifests and immutable acceptance contracts

**Files:**
- Modify: `missions/desk_task.json`, `screen_qa.json`, `voice_intake.json`, `doc_extract.json`
- Create: `crates/lfm3-harness/src/mission/mod.rs`, `manifest.rs`, `acceptance.rs`
- Test: `crates/lfm3-harness/tests/acceptance.rs`

**Interfaces:**
- Produces: `MissionManifest::from_json`, `MissionManifest::validate_plan`, `AcceptanceSpec::freeze`, `ExecutionStrategy`.
- `ExecutionStrategy { acceptance_hash: String, actions: Vec<PlannedAction> }` is the only replanning output.

- [ ] **Step 1: Write failing tests for capability limits and criterion immutability**

```rust
#[test]
fn strategy_cannot_change_acceptance() {
    let acceptance = acceptance_fixture().freeze().unwrap();
    let strategy = ExecutionStrategy {
        acceptance_hash: "a".repeat(64),
        actions: vec![],
    };
    let error = acceptance.validate_strategy(&strategy).unwrap_err();
    assert!(matches!(error, PlanError::AcceptanceHashMismatch { .. }));
}
```

Also test that `desk_task` rejects `remember`, `recall`, `fetch_url`, and arbitrary `run_command` capabilities.

- [ ] **Step 2: Run the acceptance tests and confirm failure**

Run: `cargo test -p lfm3-harness --test acceptance`  
Expected: failure because mission and acceptance types are absent.

- [ ] **Step 3: Implement canonical freezing and versioned manifests**

```rust
pub fn freeze(mut self) -> Result<FrozenAcceptance, PlanError> {
    self.validate()?;
    self.steps.sort_by_key(|step| step.index);
    let canonical = serde_json::to_vec(&self)?;
    let hash = format!("{:x}", Sha256::digest(&canonical));
    Ok(FrozenAcceptance { spec: self, hash })
}
```

Manifest validation must reject unknown fields, empty check conditions, paths outside declared scopes, non-finite thresholds, and budgets above global maxima. Preserve the four mission names while converting their manifests to the versioned schema.

- [ ] **Step 4: Run mission and acceptance gates**

Run: `cargo test -p lfm3-harness --test acceptance`  
Expected: valid plans freeze deterministically; any acceptance mutation or capability expansion fails.

- [ ] **Step 5: Verify serialized compatibility**

Serialize a frozen contract twice and assert identical bytes and hashes. Save the fixture under `tests/fixtures/acceptance/desk_task.json`.

---

### Task 3: SQLite journal, artifacts, and interruption reconciliation

**Files:**
- Create: `crates/lfm3-harness/src/store/mod.rs`, `migrations.rs`, `events.rs`, `artifacts.rs`
- Test: `crates/lfm3-harness/tests/store.rs`

**Interfaces:**
- Produces: `Store::open`, `Store::create_run`, `Store::persist_pending_steps`, `Store::start_attempt`, `Store::finish_attempt`, `Store::snapshot`, `Store::events_after`, `Store::reconcile_interrupted`.
- `Store::finish_attempt` commits results, state, and event atomically before live broadcast.

- [ ] **Step 1: Write the failing transactional event test**

```rust
#[tokio::test]
async fn attempt_result_and_event_share_one_transaction() {
    let store = Store::open(temp_database()).await.unwrap();
    let ids = finished_attempt_fixture(&store).await;
    let snapshot = store.snapshot(&ids.run).await.unwrap();
    let events = store.events_after(&ids.run, 0).await.unwrap();
    assert_eq!(snapshot.steps[0].attempts[0].status, AttemptStatus::Passed);
    assert!(events.iter().any(|event| event.kind == "step.passed"));
}
```

Add a crash-fixture test proving a previous engine’s `running` rows become `interrupted` without executing an action.

- [ ] **Step 2: Run store tests and confirm failure**

Run: `cargo test -p lfm3-harness --test store`  
Expected: missing store and migration implementation.

- [ ] **Step 3: Implement migrations, atomic artifacts, and durable events**

Use WAL, foreign keys, `user_version` migrations, and one store actor. Artifact writes use a temporary file, flush, hash, and atomic rename. State/event writes use one SQLite transaction; `tokio::sync::broadcast` publishes only after commit.

- [ ] **Step 4: Run persistence gates**

Run: `cargo test -p lfm3-harness --test store`  
Expected: migrations are repeatable, snapshots include every pending step, events replay exactly, and restart reconciles stale state.

- [ ] **Step 5: Verify trusted storage separation**

Create a workspace fixture and assert that writes beneath it cannot alter `runs.db`, trusted artifacts, logs, or `engine.lock`.

---

### Task 4: Cancellation and owned-process supervision

**Files:**
- Create: `crates/lfm3-harness/src/cancel.rs`, `supervisor.rs`
- Create: `crates/lfm3-harness/src/supervisor/windows.rs`
- Test: `crates/lfm3-harness/tests/cancellation.rs`, `supervisor_windows.rs`

**Interfaces:**
- Produces: `CancellationContext::cancel`, `is_cancelled`, `child`, `remaining`.
- Produces: `ProcessSupervisor::spawn`, `terminate_owned`, `shutdown`; `OwnedChild` exposes typed exit results.

- [ ] **Step 1: Write failing cancellation and process cleanup tests**

```rust
#[tokio::test]
async fn cancellation_interrupts_active_wait() {
    let context = CancellationContext::new(Duration::from_secs(30));
    let child = context.child();
    let waiter = tokio::spawn(async move { child.cancelled().await });
    context.cancel();
    tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
}
```

The Windows test spawns a long-running owned child, shuts down the supervisor, and asserts that the exact PID no longer exists.

- [ ] **Step 2: Run the tests and confirm failure**

Run: `cargo test -p lfm3-harness --test cancellation --test supervisor_windows`  
Expected: missing cancellation/supervisor implementation.

- [ ] **Step 3: Implement cancellation propagation and Job Object ownership**

Use `tokio_util::sync::CancellationToken` and per-call deadlines. On Windows, create children suspended, assign them to a kill-on-close Job Object, resume them, and retain exact process ownership. Never terminate by executable name.

- [ ] **Step 4: Run cancellation and supervisor gates**

Run: `cargo test -p lfm3-harness --test cancellation --test supervisor_windows`  
Expected: cancellation completes within one second, deadlines expire inside active waits, and no owned test child survives shutdown.

- [ ] **Step 5: Verify external process preservation**

Start an external fixture process not owned by the supervisor. Supervisor shutdown must leave that PID running; clean it up with the fixture’s own handle afterward.

---

### Task 5: Local llama.cpp client and model/resource manager

**Files:**
- Create: `crates/lfm3-harness/src/model/mod.rs`, `client.rs`, `manager.rs`, `process.rs`, `types.rs`
- Test: `crates/lfm3-harness/tests/model_client.rs`, `model_manager.rs`

**Interfaces:**
- Produces: `ModelGateway` trait with `plan`, `act`, `look`, `transcribe`, `decide` methods using typed requests/results.
- Produces: `ModelManager::acquire(ModelKey, CancellationContext) -> ModelLease`.
- Tests use a Rust fake HTTP server; production uses Reqwest against llama.cpp.

- [ ] **Step 1: Write failing protocol and lease tests**

```rust
#[tokio::test]
async fn text_client_preserves_native_tool_calls() {
    let server = FakeLlamaServer::start(chat_tool_call_fixture()).await;
    let client = LlamaClient::new(server.url(), "lfm3-1.7b");
    let response = client.chat(chat_request_fixture(), test_cancel()).await.unwrap();
    assert_eq!(response.tool_calls[0].function.name, "write_file");
}
```

Manager tests cover RAM reservation, no eviction while leased, sequential inference, endpoint identity mismatch, and external-server ownership.

- [ ] **Step 2: Run model tests and confirm failure**

Run: `cargo test -p lfm3-harness --test model_client --test model_manager`  
Expected: missing model protocols and manager.

- [ ] **Step 3: Implement typed protocols and shared model scheduling**

Include native `tool_calls`, `reasoning_content` separation, multimodal inputs, `/v1/systemone`, projector-aware launch settings, d1 batch flags, and `--jinja`. Abort in-flight Reqwest calls on cancellation. Reuse `ProcessSupervisor` for owned servers.

- [ ] **Step 4: Run model gates**

Run: `cargo test -p lfm3-harness --test model_client --test model_manager`  
Expected: protocol fixtures pass; model leases cannot overcommit or evict active models; cancellation aborts a stalled response.

- [ ] **Step 5: Run the pinned-runtime compatibility probe**

Run: `cargo run -p lfm3-cli -- doctor --probe-models`  
Expected: the CLI reports b11960 / v0.8.0 and each available model identity. Missing weights produce setup instructions and no download.

---

### Task 6: Scoped tools and typed ToolResult evidence

**Files:**
- Create: `crates/lfm3-harness/src/tools/mod.rs`, `fs.rs`, `command.rs`, `senses.rs`
- Create: `crates/lfm3-harness/src/verify/evidence.rs`
- Test: `crates/lfm3-harness/tests/tools.rs`

**Interfaces:**
- Produces: async `Tool::execute(ToolRequest, CancellationContext) -> ToolResult`.
- `ToolResult` contains attempt ID, typed status, optional exit code, artifact references, hashes, and timing.
- `ToolRegistry` contains only mission-granted tools.

- [ ] **Step 1: Write failing sandbox and provenance tests**

```rust
#[tokio::test]
async fn file_tool_rejects_workspace_escape() {
    let tools = file_tools_fixture().await;
    let request = write_request("../outside.txt", "blocked");
    let error = tools.execute(request, test_cancel()).await.unwrap_err();
    assert!(matches!(error, ToolError::OutsideWorkspace { .. }));
}
```

Also cover absolute paths, symlink/reparse-point escapes, atomic write failure, fixed command-template executables, invalid argument slots, and cancellation of an active command.

- [ ] **Step 2: Run tool tests and confirm failure**

Run: `cargo test -p lfm3-harness --test tools`  
Expected: missing tool registry and typed results.

- [ ] **Step 3: Implement narrow tools**

Implement `list_files`, `read_file`, `write_file`, and `edit_file` first. `CommandTemplate` stores a fixed executable and typed argument slots; it never accepts a planner-authored command line. Sense adapters call only the shared `ModelManager`.

- [ ] **Step 4: Run tool gates**

Run: `cargo test -p lfm3-harness --test tools`  
Expected: path escapes fail before side effects, successful results reference hashed artifacts, and command cancellation terminates the owned child.

- [ ] **Step 5: Verify observation spoofing is impossible**

Return text `exit=0` from `read_file` and assert its `ToolResult.exit_code` remains `None`.

---

### Task 7: Deterministic verifier and provenance-bound CheckResult

**Files:**
- Create: `crates/lfm3-harness/src/verify/mod.rs`, `deterministic.rs`
- Test: `crates/lfm3-harness/tests/deterministic_checks.rs`

**Interfaces:**
- Produces: async `Check::evaluate(CheckSpec, CheckContext) -> CheckResult`.
- `CheckContext` binds the frozen acceptance hash, attempt ID, typed `ToolResult`, workspace, before/after manifests, and artifact store.

- [ ] **Step 1: Write failing anti-false-pass tests**

```rust
#[tokio::test]
async fn command_exit_requires_same_attempt_command_result() {
    let context = CheckContext::fixture_with_observation("exit=0");
    let result = evaluate(command_exit_spec(0), context).await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.evidence.len(), 0);
}
```

Add cases for empty `content_matches`, wrong-artifact JSON, missing file, changed hash, failed machine-readable tests, and incorrect numeric totals.

- [ ] **Step 2: Run verifier tests and confirm failure**

Run: `cargo test -p lfm3-harness --test deterministic_checks`  
Expected: missing check registry.

- [ ] **Step 3: Implement deterministic checks**

Implement file existence/change, content matching, JSON schema, numeric consistency, typed command exit, machine-readable tests, and structured report completeness. Every pass requires non-empty evidence references and matching attempt/artifact provenance.

- [ ] **Step 4: Run verifier gates**

Run: `cargo test -p lfm3-harness --test deterministic_checks`  
Expected: every valid deterministic result passes and every spoofed/vacuous/provenance-mismatched input fails or errors.

- [ ] **Step 5: Run all native regressions**

Run: `cargo test --workspace`  
Expected: all previous gates remain green.

---

### Task 8: Durable run/step/attempt state machine and final verification

**Files:**
- Create: `crates/lfm3-harness/src/agent/mod.rs`, `runner.rs`
- Create: `crates/lfm3-harness/src/coordinator.rs`
- Test: `crates/lfm3-harness/tests/runner.rs`

**Interfaces:**
- Consumes: frozen acceptance, model/tool/check traits, store, cancellation, coordinator.
- Produces: `Runner::execute(run_id) -> RunOutcome` and `RunCoordinator::start`.
- Runner uses no concrete llama.cpp or Tauri types.

- [ ] **Step 1: Write failing state-machine tests**

```rust
#[tokio::test]
async fn run_cannot_pass_without_final_output_verification() {
    let fixture = RunnerFixture::new().with_final_check(CheckStatus::Fail);
    let outcome = fixture.execute().await;
    assert_eq!(outcome.status, RunStatus::Incomplete);
    assert_eq!(outcome.reason, Some(IncompleteReason::FinalVerificationFailed));
}
```

Also test all steps persisted before execution, no pass without check evidence, second active run returns `RunBusy`, and cancellation during verification cannot become passed.

- [ ] **Step 2: Run runner tests and confirm failure**

Run: `cargo test -p lfm3-harness --test runner`  
Expected: missing runner/coordinator.

- [ ] **Step 3: Implement explicit transitions**

Attempt rows commit before side effects. Every transition validates the prior state. After the last step, rerun final mission checks against actual artifacts and hashes. The coordinator holds one database-backed engine/run lease.

- [ ] **Step 4: Run runner gates**

Run: `cargo test -p lfm3-harness --test runner`  
Expected: all legal transitions pass; illegal transitions, concurrent starts, false completion, and cancellation races fail closed.

- [ ] **Step 5: Add golden journals**

Store expected event sequences for pass, step failure, final failure, cancellation, and interrupted restart under `tests/golden/` and compare normalized journals exactly.

---

### Task 9: Evidence-driven retry and immutable strategy replanning

**Files:**
- Create: `crates/lfm3-harness/src/agent/recovery.rs`, `packets.rs`
- Test: `crates/lfm3-harness/tests/recovery.rs`

**Interfaces:**
- Produces: `RecoveryController::next(FailureRecord) -> RecoveryDecision`.
- Decisions: `Retry`, `Backoff`, `Replan`, `NeedsReview`, `Incomplete`, `Cancelled`.
- Replanning returns `ExecutionStrategy` with the original acceptance hash.

- [ ] **Step 1: Write failing recovery tests**

```rust
#[test]
fn replanning_does_not_reset_total_attempt_budget() {
    let mut recovery = RecoveryController::new(Budget::fixture(3, 2, 6));
    recovery.record(FailureRecord::fixture(1));
    recovery.record(FailureRecord::fixture(2));
    recovery.record(FailureRecord::fixture(3));
    assert_eq!(recovery.next(FailureRecord::fixture(4)), RecoveryDecision::Replan);
    recovery.record_replan();
    recovery.record(FailureRecord::fixture(5));
    recovery.record(FailureRecord::fixture(6));
    assert!(matches!(recovery.next(FailureRecord::fixture(7)), RecoveryDecision::Incomplete { .. }));
}
```

Also test duplicate actions, alternating cycles, same failure with different actions, transient-only identical retry, inconclusive review, and acceptance-hash mismatch.

- [ ] **Step 2: Run recovery tests and confirm failure**

Run: `cargo test -p lfm3-harness --test recovery`  
Expected: missing recovery controller.

- [ ] **Step 3: Implement canonical signatures and compact packets**

Track action hashes, normalized failure signatures, and action/failure pairs across the complete step. Failure packets contain typed evidence references and remaining budgets, not full transcripts. Wire every decision into runner events and persistence.

- [ ] **Step 4: Run recovery gates**

Run: `cargo test -p lfm3-harness --test recovery --test runner`  
Expected: retries use failure evidence, no-progress forces strategy change, total budgets hold across replans, and no recovery path mutates acceptance.

- [ ] **Step 5: Verify retry artifacts remain auditable**

Inspect a golden retry run and assert each attempt has a distinct ID, reason, strategy number, evidence, and state transition.

---

### Task 10: Real structured planner and one-action executor with fake-model gates

**Files:**
- Create: `crates/lfm3-harness/src/agent/planner.rs`, `executor.rs`
- Test: `crates/lfm3-harness/tests/planner.rs`, `executor.rs`

**Interfaces:**
- Produces: `Planner::draft`, `Planner::repair_once`, `Executor::next_action`, `Executor::replan_strategy`.
- All methods accept `CancellationContext` and use `ModelGateway` plus mission schemas.

- [ ] **Step 1: Write failing planner/executor protocol tests**

```rust
#[tokio::test]
async fn invalid_plan_receives_exactly_one_repair() {
    let gateway = FakeModelGateway::new(vec![invalid_plan(), valid_plan()]);
    let planner = Planner::new(gateway.clone());
    let plan = planner.draft(plan_request_fixture(), test_cancel()).await.unwrap();
    assert_eq!(gateway.calls(), 2);
    assert_eq!(plan.steps.len(), 1);
}
```

Executor cases: no tool call, multiple tool calls, disallowed tool, malformed arguments, tool error, model timeout, cancellation, and replan attempting a different acceptance hash.

- [ ] **Step 2: Run planner/executor tests and confirm failure**

Run: `cargo test -p lfm3-harness --test planner --test executor`  
Expected: missing planner/executor.

- [ ] **Step 3: Implement constrained prompting and strict decoding**

The planner brief contains mission purpose, typed inputs, allowed tools/check schemas, output contract, and real target paths. Strip code fences, require one complete JSON value, validate all fields, and permit one validator-error repair. The executor requests exactly one tool action and returns a typed invalid-action failure otherwise.

- [ ] **Step 4: Run fake-model gates**

Run: `cargo test -p lfm3-harness --test planner --test executor --test runner`  
Expected: all malformed/adversarial responses fail closed; valid plans/actions drive complete fake-model runs without a scripted production executor.

- [ ] **Step 5: Run the full native suite**

Run: `cargo fmt --all -- --check; cargo clippy --workspace --all-targets -- -D warnings; cargo test --workspace`  
Expected: all commands exit zero.

---

### Task 11: Headless real-model `desk_task` acceptance

**Files:**
- Modify: `crates/lfm3-cli/src/main.rs`, `missions/desk_task.json`
- Create: `tests/fixtures/desk_task/`, `tests/golden/desk_task/`
- Modify: `README.md`

**Interfaces:**
- Produces CLI: `doctor`, `missions`, `plan`, `run`, `status`, `retry`, `models`.
- `run --events -` emits durable JSON events to stdout while the engine executes normally.

- [ ] **Step 1: Add the real acceptance fixture and expected deterministic checks**

Create a source note fixture, a fixed output path, required source terms, a forbidden invented term, and a final output hash/evidence contract. Include a wrong-content case and a recoverable first-write-failure case driven by test configuration, never by a production scripted executor.

- [ ] **Step 2: Verify the fake-model CLI path before loading weights**

Run: `cargo run -p lfm3-cli -- run --mission desk_task --fixture tests/fixtures/desk_task/basic.json --fake-model`  
Expected: explicit test-only fake mode passes and prints checks. Packaged builds exclude `--fake-model`.

- [ ] **Step 3: Run the real pinned text model**

Run:

```powershell
cargo run -p lfm3-cli -- run --mission desk_task --input tests/fixtures/desk_task/basic.json --workspace tests/fixtures/desk_task/workspace --events -
```

Expected: LFM3-1.7B plans and acts through llama.cpp b11960 / v0.8.0; every step and final check passes with evidence.

- [ ] **Step 4: Run adversarial real-model gates**

Exercise wrong content, impossible output scope, model unavailable, cancellation during generation, duplicate action, no progress, and engine interruption. Expected: no false pass, no unlimited loop, no automatic resume, and no owned model process left behind.

- [ ] **Step 5: Record the release-blocking result**

Document model hash, llama.cpp build, acceptance hash, attempts, wall time, peak memory, and evidence location. Do not start Task 12 unless the real run passes or the plan is revised based on measured model limitations.

---

### Task 12: React bridge and application foundation

**Files:**
- Create: `apps/ui/package.json`, `apps/ui/src/App.tsx`, `apps/ui/src/bridge/types.ts`
- Create: `apps/ui/src/bridge/EngineBridge.ts`, `TauriBridge.ts`, `DemoBridge.ts`
- Test: `apps/ui/src/bridge/bridge.test.ts`

**Interfaces:**
- Produces TypeScript equivalents of mission, acceptance, run, step, attempt, tool result, check result, event, and model status contracts.
- `EngineBridge` exposes the exact commands/events from `PLAN.md` §17.

- [ ] **Step 1: Write failing bridge reducer tests**

Test snapshot hydration, ordered event application, duplicate event suppression, gap detection, and snapshot refetch after reconnect.

- [ ] **Step 2: Run UI tests and confirm failure**

Run: `npm test -- --run` in `apps/ui`  
Expected: missing bridge/reducer implementation.

- [ ] **Step 3: Implement the bridge and reducer**

`TauriBridge` uses `@tauri-apps/api/core.invoke` and event listeners. `DemoBridge` uses local fixtures only and displays a permanent demo badge. Production build configuration excludes demo selection.

- [ ] **Step 4: Run UI build and tests**

Run: `npm test -- --run; npm run build` in `apps/ui`  
Expected: typecheck, tests, and production build pass.

- [ ] **Step 5: Verify no production HTTP engine dependency**

Search `apps/ui/src` for `fetch(`, `EventSource`, localhost URLs, and bearer tokens. Expected: none in the production bridge.

---

### Task 13: Mission, acceptance, run, evidence, and history UI

**Files:**
- Create: `apps/ui/src/components/MissionPicker.tsx`, `AcceptanceReview.tsx`, `RunTimeline.tsx`
- Create: `apps/ui/src/components/EvidenceInspector.tsx`, `ApprovalDialog.tsx`, `ModelDock.tsx`, `RunHistory.tsx`
- Modify: `apps/ui/src/App.tsx`

**Interfaces:**
- Consumes `EngineBridge`; components emit typed user decisions and never mutate run state directly.

- [ ] **Step 1: Write component tests for critical behavior**

Cover mission-specific inputs, acceptance hash display, approve/reject plan, failed criterion text, retry/strategy history, approval allow/deny, active cancellation, semantic-confidence labels, interrupted history, and linked retry run.

- [ ] **Step 2: Run component tests and confirm failure**

Run: `npm test -- --run` in `apps/ui`  
Expected: missing components.

- [ ] **Step 3: Implement the six views**

Use accessible native controls and dense evidence presentation. Every terminal state remains distinguishable. `passed` always links to final evidence; `incomplete`, `cancelled`, and `interrupted` are never styled as success.

- [ ] **Step 4: Run component and build gates**

Run: `npm test -- --run; npm run build` in `apps/ui`  
Expected: all component tests and production build pass.

- [ ] **Step 5: Inspect the complete demo workflow**

Confirm the verified-task views contain no free-form chat input and that demo status cannot be hidden.

---

### Task 14: Playwright UI acceptance

**Files:**
- Create: `apps/ui/playwright.config.ts`, `apps/ui/e2e/harness.spec.ts`
- Create: `apps/ui/src/bridge/fixtures/`

**Interfaces:**
- Uses only explicit `DemoBridge` fixtures in browser tests.

- [ ] **Step 1: Write the failing end-to-end scenarios**

Scenarios: start a mission, review and approve acceptance, observe a failed attempt and retry, approve and deny a command, cancel during execution, reconnect through an event gap, reject a second active run, and open a linked retry from history.

- [ ] **Step 2: Run Playwright and confirm the initial failure**

Run: `npx playwright test` in `apps/ui`  
Expected: scenarios fail against the uncompleted integration.

- [ ] **Step 3: Complete bridge wiring and deterministic fixture timing**

Use fixture-controlled promises rather than arbitrary sleeps. Sequence gaps must trigger snapshot recovery; cancellation must immediately disable further pass transitions in displayed state.

- [ ] **Step 4: Run the complete UI gate**

Run: `npx playwright test; npm run build` in `apps/ui`  
Expected: all browser scenarios and production build pass.

- [ ] **Step 5: Save screenshots for the principal states**

Save mission, acceptance, active retry, evidence, incomplete, and interrupted views as reviewed test artifacts.

---

### Task 15: Embedded Tauri 2 commands and durable event forwarding

**Files:**
- Create: `apps/desktop/src-tauri/Cargo.toml`, `tauri.conf.json`, `capabilities/default.json`
- Create: `apps/desktop/src-tauri/src/main.rs`, `commands.rs`, `events.rs`
- Modify: root `Cargo.toml`

**Interfaces:**
- Maps `EngineBridge` directly to Tauri commands; embeds `lfm3-harness` without a sidecar or HTTP listener.

- [ ] **Step 1: Write failing command-state tests**

Test acceptance-hash mismatch, expired/foreign approval ID, cancellation of an unknown run, model release while leased, second active run, artifact opening outside trusted storage, and event replay after a cursor.

- [ ] **Step 2: Run Rust command tests and confirm failure**

Run: `cargo test -p lfm3-desktop`  
Expected: missing command layer.

- [ ] **Step 3: Implement typed commands and post-commit event forwarding**

Tauri setup constructs one `Harness`, one coordinator, and one event forwarder. Frontend capabilities include only required custom commands/events; no shell execute or broad filesystem scope.

- [ ] **Step 4: Launch the development desktop**

Run: `npx tauri dev` from `apps/desktop`  
Expected: the native window executes the same real headless engine path, displays live durable events, and performs no loopback engine connection.

- [ ] **Step 5: Run desktop and native regressions**

Run: `cargo test --workspace`  
Expected: all engine and command tests pass.

---

### Task 16: Desktop process containment, setup, and installers

**Files:**
- Create: `apps/desktop/src-tauri/src/job_windows.rs`, `apps/desktop/src-tauri/src/setup.rs`, `crates/lfm3-harness/src/supervisor/unix.rs`
- Create: `apps/desktop/src-tauri/tauri.linux.conf.json`, `scripts/smoke.ps1`
- Modify: `apps/desktop/src-tauri/tauri.conf.json`, `README.md`

**Interfaces:**
- Desktop and CLI share `ProcessSupervisor`; setup validates absolute model/binary paths and pinned runtime identity.

- [ ] **Step 1: Add packaged lifecycle tests**

Cover normal exit, cancel during inference, engine error, forced application termination, missing model, path containing spaces, and externally started development server preservation.

- [ ] **Step 2: Build the NSIS application**

Run: `npm --prefix apps/desktop run build` from the repository root  
Expected: the NSIS installer builds on Windows and the `.deb`/`.AppImage` packages build on Linux, without Python, downloads, or repository-relative resource assumptions.

- [ ] **Step 3: Implement first-run local setup and job supervision**

Setup locates or asks for `llama-server` (`llama-server.exe` on Windows), `llama-tts`, GGUF directories, and projectors. It performs read-only probes and never auto-downloads missing weights. All owned inference/tool children join the kill-on-close Job Object on Windows or their own process group on Linux.

- [ ] **Step 4: Run installed-application smoke acceptance**

Install into a path containing spaces. Launch offline, run the real `desk_task`, cancel a second run, close during another run, and force-terminate the app. Expected: truthful state and no surviving owned engine/model/tool PID.

- [ ] **Step 5: Verify external process ownership**

With an external llama.cpp development server running, close the app. Expected: the external PID remains available and is explicitly marked externally owned in diagnostics.

---

### Task 17: Semantic checks and additional specialized missions

**Files:**
- Create: `crates/lfm3-harness/src/verify/semantic.rs`
- Modify: `missions/screen_qa.json`, `voice_intake.json`, `doc_extract.json`
- Create: `tests/fixtures/semantic/dev/`, `tests/fixtures/semantic/holdout/`
- Create: `scripts/accept.ps1`

**Interfaces:**
- Adds d1/VL semantic `Check` implementations using the same provenance-bound `CheckResult`.
- Semantic automatic pass is a mission capability enabled only after holdout qualification.

- [ ] **Step 1: Add false-pass and provenance fixtures**

Include unsupported transcript fields, wrong numeric totals, false visual claims, omitted visual details, ambiguous evidence, unavailable judge, and deterministic failure plus semantic pass. Expected: deterministic failure always wins and inconclusive never passes.

- [ ] **Step 2: Run semantic fixtures before enabling automatic pass**

Run: `cargo test -p lfm3-harness --test semantic`  
Expected: protocol and provenance tests pass while mission manifests remain human-review-only.

- [ ] **Step 3: Implement typed semantic evidence**

Persist source hashes, exact prompts, model identity, raw responses, confidence, and output JSON pointers. VL descriptions judged by d1 are labelled a single evidence chain, not independent verification.

- [ ] **Step 4: Evaluate development and frozen holdout sets separately**

Run each mission with real models. Any confident false pass on the holdout keeps automatic semantic passing disabled. Do not tune thresholds against holdout results.

- [ ] **Step 5: Enable only qualified missions**

A qualified mission completes three packaged real-model runs with no false completion. Others remain visible as unavailable or human-review-only with an explicit reason.

---

### Task 18: Packaged release acceptance and handoff

**Files:**
- Modify: `README.md`, `PLAN.md`
- Create: `tests/golden/release-manifest.json`

**Interfaces:**
- Produces a reproducible release-evidence manifest for the exact installer, models, fixtures, and acceptance contracts.

- [ ] **Step 1: Run every static and native gate**

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all exit zero with no Python invocation.

- [ ] **Step 2: Run every UI and installer gate**

Run UI unit tests, Playwright, `npx tauri build`, and `scripts/smoke.ps1`. Expected: browser and native workflows pass and the installed app runs offline.

- [ ] **Step 3: Run packaged real-model adversarial acceptance**

Verify deterministic success/failure, genuine retry recovery, immutable replanning, impossible goals, no progress, model crash, active cancellation, interruption, concurrent start, missing weights, and process cleanup. Expected: every outcome is truthful and evidence-backed.

- [ ] **Step 4: Record reproducibility evidence**

Record installer hash, Rust dependency lock hash, llama.cpp build, model hashes, mission/acceptance hashes, sampling settings, event journals, runtime, and peak memory.

- [ ] **Step 5: Perform the final scope audit**

Search the repository and packaged application for Python files, Python launch commands, remote model endpoints, arbitrary shell surfaces, hidden download behavior, and scripted production fallbacks. Expected: none. Update documentation with measured limitations and exact verified commands.

---

### Task 19: Durable chat sessions, prompt, and tool schemas

**Files:**
- Create: `crates/lfm3-harness/src/chat/mod.rs`, `crates/lfm3-harness/src/chat/store.rs`, `crates/lfm3-harness/src/chat/prompt.rs`
- Modify: `crates/lfm3-harness/src/lib.rs`, `crates/lfm3-harness/Cargo.toml`
- Test: module tests in `store.rs` and `prompt.rs`

**Interfaces:**
- Produces: `ChatStore::open`, `list_sessions`, `create_session`, `session`, `rename_session`, `delete_session`, `view`, `messages`, `append_message`, `finish_message`, `maybe_retitle`, `create_action`, `resolve_action`, `actions`.
- Produces: `ChatSession`, `ChatMessageRecord`, `ChatActionRecord`, `ChatSessionView`.
- Produces: `system_message`, `tool_definitions`, `to_model_messages`, `fallback_tool_call`, `SYSTEM_PROMPT`.
- Consumes: `ChatRole`, `ChatMessage`, `ToolDefinition`, `ToolCall`, and workspace path rules.

- [ ] **Step 1: Write the failing store tests**

```rust
#[test]
fn sessions_titles_and_messages_round_trip() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = ChatStore::open(&directory.path().join("chat.db")).expect("open");
    let session = store.create_session(None).expect("session");
    assert_eq!(session.title, "New chat");
    let message = store
        .append_message(&session.id, ChatRole::User, "Plan my week", "complete", json!({}))
        .expect("append");
    assert_eq!(message.seq, 1);
    store.rename_session(&session.id, "Weekly plan").expect("rename");
    assert_eq!(store.session(&session.id).unwrap().title, "Weekly plan");
    assert_eq!(store.view(&session.id).unwrap().messages.len(), 1);
}
```

Also assert that `resolve_action` accepts only a `pending` action, and that reopening a store turns `streaming` messages into `cancelled` and `pending` actions into `expired`.

- [ ] **Step 2: Run the tests and confirm the missing module failure**

Run: `cargo test -p lfm3-harness chat`  
Expected: compilation failure because `ChatStore` and the chat types do not exist.

- [ ] **Step 3: Implement `chat.db`**

Create the three tables, sequence-numbered messages, status transitions, session title truncation, and reopen reconciliation. Keep the database separate from `runs.db`. Add `pub mod chat;` and the re-exports in `lib.rs`.

- [ ] **Step 4: Write the failing prompt tests**

```rust
#[test]
fn transcript_keeps_the_newest_messages_within_budget() {
    let records = vec![
        record(1, ChatRole::User, "old ".repeat(400), json!({})),
        record(2, ChatRole::Assistant, "answer ".repeat(60), json!({})),
        record(3, ChatRole::User, "latest question", json!({})),
    ];
    let messages = to_model_messages(&records, 100);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, MessageContent::Text("latest question".to_owned()));
}
```

Add tests that assistant `tool_calls` metadata is reconstructed and that a fenced ```tool block becomes one `ToolCall` with the fence removed from prose.

- [ ] **Step 5: Implement the system prompt, four tool schemas, bounded history, and fallback parser**

Run: `cargo test -p lfm3-harness chat`  
Expected: all chat store and prompt tests pass.

---

### Task 20: Streaming model calls and local audio content

**Files:**
- Modify: `crates/lfm3-harness/src/model/types.rs`, `client.rs`, `manager.rs`
- Modify: `Cargo.toml`, `crates/lfm3-harness/Cargo.toml`
- Test: `crates/lfm3-harness/tests/model_client.rs`

**Interfaces:**
- Produces: `ContentPart::InputAudio { input_audio: InputAudio }` with `data` and `format`.
- Produces: `LlamaClient::chat_stream(request, cancel, on_delta)` and `ModelLease::chat_stream`.
- Consumes: the existing `ChatRequest`, `AssistantMessage`, `ToolCall`, and fake llama.cpp server fixtures.

- [ ] **Step 1: Write the failing streaming test**

Extend `FakeLlamaServer` with an SSE response that emits two content deltas and one fragmented tool call across chunks, then ends with `[DONE]`. Assert the callback receives `Hello ` then `world`, and the final response carries exactly one call with the complete argument JSON.

- [ ] **Step 2: Run the test and confirm `chat_stream` is missing**

Run: `cargo test -p lfm3-harness --test model_client`  
Expected: compilation failure because `chat_stream` does not exist.

- [ ] **Step 3: Implement SSE parsing**

Use `reqwest` streaming plus `futures_util::StreamExt`. Accept `data:` lines, accumulate content and tool-call fragments by index, treat `[DONE]` as terminal, flush a final partial line, and propagate cancellation. `chat()` delegates to the same wire helper without streaming.

- [ ] **Step 4: Add `input_audio` to the typed request**

Serialize an audio part as `{"type":"input_audio","input_audio":{"data":"...","format":"wav"}}`. Add a model-client test asserting the JSON shape.

- [ ] **Step 5: Run the model-client suite**

Run: `cargo test -p lfm3-harness --test model_client`  
Expected: every existing protocol test plus the two new tests pass.

---

### Task 21: Chat runtime, commands, and live events

**Files:**
- Create: `apps/desktop/src-tauri/src/chat.rs`
- Modify: `apps/desktop/src-tauri/src/engine.rs`, `setup.rs`, `commands.rs`, `lib.rs`
- Test: `#[cfg(test)]` module in `chat.rs`

**Interfaces:**
- Produces: `ChatRuntime::open`, `set_emitter`, `list_sessions`, `create_session`, `session_view`, `rename_session`, `delete_session`, `send`, `cancel`, `decide_action`, `transcribe_voice`.
- Produces: `ChatEnvelope` and `ChatEvent` serialized as `{ session_id, kind, ... }`.
- Consumes: `ChatStore`, prompt helpers, `ModelManager`, `ProcessSupervisor`, `ToolRegistry`, `CancellationContext`.

- [ ] **Step 1: Write the failing runtime tests**

```rust
#[test]
fn live_events_flatten_their_kind_inside_the_session_envelope() {
    let envelope = ChatEnvelope {
        session_id: "session-1".to_owned(),
        event: ChatEvent::AssistantDelta {
            message_id: "message-1".to_owned(),
            delta: "hello".to_owned(),
        },
    };
    let value = serde_json::to_value(&envelope).expect("serialize");
    assert_eq!(value["kind"], "assistant_delta");
    assert_eq!(value["session_id"], "session-1");
}
```

Also assert approval summaries include the path and write size.

- [ ] **Step 2: Implement the single-turn lifecycle**

Validate content, append the user message, retitle once, acquire one text lease, build the bounded prompt, and run up to six streamed rounds. Persist assistant/tool messages and emit events after each durable step. A second send during a turn returns a conflict.

- [ ] **Step 3: Implement approval-gated tools**

Grant only the four chat tools. For `write_file` and `edit_file`, create a pending action, emit `action_requested`, wait up to ten minutes for `decide_action`, and execute only after **Allow once**. Resolve the action to `denied`, `applied`, or `failed`, and return a tool message to the model. Cancellation expires pending actions and marks the assistant message `cancelled`.

- [ ] **Step 4: Implement cancellation and voice**

`cancel_chat` cancels the active turn for that session. `transcribe_voice` validates base64, size, and `wav|mp3`, acquires the audio lease, sends an `input_audio` part plus a transcription instruction, and returns trimmed text.

- [ ] **Step 5: Wire the engine, commands, and event channel**

Open `ChatRuntime` during engine assembly; register `list_chat_sessions`, `create_chat_session`, `get_chat_session`, `rename_chat_session`, `delete_chat_session`, `send_chat_message`, `cancel_chat`, `decide_chat_action`, and `transcribe_voice`. Emit `lfm3:chat-event` after durable writes during `boot`.

Run: `cargo test -p lfm3-desktop --lib chat` and `cargo check --workspace`  
Expected: tests pass and the workspace compiles without warnings.

---

### Task 22: Chat-first React interface, verification, and docs

**Files:**
- Create: `apps/ui/src/chat/ChatView.tsx`, `ChatSidebar.tsx`, `Composer.tsx`, `MessageBubble.tsx`, `ActionCard.tsx`, `RichText.tsx`, `reducer.ts`, `audio.ts`, `reducer.test.ts`
- Create: `apps/ui/src/chat.css`
- Modify: `apps/ui/src/App.tsx`, `WorkspaceView.tsx`, `main.tsx`, `bridge/types.ts`, `EngineBridge.ts`, `TauriBridge.ts`, `DemoBridge.ts`
- Modify: `README.md`, `PLAN.md`, `STEPS.md`

**Interfaces:**
- Produces: `ChatEvents` mirror types, `subscribeChat`, and all chat bridge commands.
- Produces: `applyChatEvent`, `mergeView`, `encodeVoiceClip`, `canRecord`.
- Consumes: the Task 21 command/event contract.

- [ ] **Step 1: Write the failing reducer tests**

```ts
it('streams assistant text and clears it when the durable message arrives', () => {
  let state = stateWithView();
  state = applyChatEvent(state, {
    session_id: 'session-1',
    kind: 'assistant_start',
    message: message(),
  });
  state = applyChatEvent(state, {
    session_id: 'session-1',
    kind: 'assistant_delta',
    message_id: 'message-1',
    delta: 'Hello',
  });
  expect(state.streaming['message-1']).toBe('Hello');
  state = applyChatEvent(state, {
    session_id: 'session-1',
    kind: 'assistant_done',
    message: message({ content: 'Hello', status: 'complete' }),
  });
  expect(state.streaming).toEqual({});
  expect(state.view?.messages[0].content).toBe('Hello');
});
```

Cover status/running transitions, action upserts, tool-result ordering, session rename, errors, and `mergeView`.

- [ ] **Step 2: Build the chat surface**

Create the session sidebar with new/rename/two-step delete, model badges, and the verified-tasks switch. Build the transcript with rich text, tool output, action cards, status line, and auto-scroll. Build the composer with Enter/Shift+Enter, Send/Stop, a 120-second recording cap, local WAV resampling, transcription, and error reporting.

- [ ] **Step 3: Make chat the default shell**

`App.tsx` renders chat by default and the verified workspace on demand. Keep `?demo=1` in development rendering the verified fixtures with their existing headings and flows; chat remains unavailable there. Preserve every existing Playwright selector.

- [ ] **Step 4: Run UI gates**

Run:

```powershell
cd apps/ui
npm run typecheck
npm test
npm run build
npx playwright test
```

Expected: typecheck and build pass; 41 Vitest tests and 8 Playwright scenarios pass.

- [ ] **Step 5: Run the complete static gate and document the result**

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all pass. Update README and PLAN with the chat surface, approval semantics, voice limitations, chat-versus-verified separation, and the fact that real-model chat/tool/voice qualification is still open. Rebuild the installer before publishing, because the recorded installer hash predates the chat work.

---

### Task 23: Multi-model workspace agent — routing, search, vision, and local voice

**Status:** implemented on 2026-10-31; real-model qualification is still open. LFM3-Audio-2B speech rendering was verified directly through `llama-tts` and through the runtime's owned-process test with the installed weight and projector; Tauri playback still needs a measured run.

**Goal:** make the chat surface useful for folder analysis, file discovery, summarisation, related-file discovery, report writing, image inspection, and spoken replies, using every locally installed LFM3 role.

**Files:**
- Create: `crates/lfm3-harness/src/chat/router.rs`, `crates/lfm3-harness/src/tools/search.rs`
- Modify: `crates/lfm3-harness/src/chat/mod.rs`, `chat/prompt.rs`, `lib.rs`, `tools/mod.rs`, `mission/manifest.rs`, `agent/executor.rs`, `model/manager.rs`
- Modify: `apps/desktop/src-tauri/src/chat.rs`, `commands.rs`, `engine.rs`, `lib.rs`
- Modify: `apps/ui/src/bridge/types.ts`, `EngineBridge.ts`, `TauriBridge.ts`, `DemoBridge.ts`
- Modify: `apps/ui/src/chat/ChatView.tsx`, `ChatSidebar.tsx`, `Composer.tsx`, `MessageBubble.tsx`, `reducer.ts`, `chat.css`
- Tests: `crates/lfm3-harness/tests/tools.rs`, `apps/desktop/src-tauri/src/chat.rs` unit tests, `apps/ui/src/chat/reducer.test.ts`, `apps/ui/src/chat/MessageBubble.test.tsx`

**Interfaces:**
- Produces: `ChatIntent`, `ChatRoute`, `RouteSource`, `RouteRole`, `route_request(models, message, cancel) -> ChatRoute`, `ChatCapabilities { vision, audio }`, `system_message_with_route(workspace, route)`, `tool_definitions(capabilities)`.
- Produces: `ModelManager::available_models()` and `ModelManager::has_model(key)`.
- Produces: `search_files` and `related_files` tools and their `KNOWN_TOOLS` entries.
- Produces: Tauri commands `synthesize_speech(text) -> SpeechReply` and `attach_image(name, data_base64) -> relative_path`; chat event `route`.
- Consumes: the existing d1 `DecisionRequest`/`ModelLease::decide`, the shared `ToolRegistry`, the vision `look` and audio `transcribe` adapters, and the existing chat turn loop.

- [ ] **Step 1: Add bounded workspace discovery tools**

Add `search_files` with `query`, optional `path`, `mode` (`name`/`content`/`both`), and `max_results` (1–40, default 20). Add `related_files` with `path` and `max_results` (1–30, default 10). Both walk only regular files beneath the confined workspace, skip links and `target`, `.git`, `node_modules`, `.svn`, `dist`, `.lfm3`, enforce `MAX_DEPTH = 10`, `MAX_WALK_ENTRIES = 4000`, and `MAX_SCAN_BYTES = 512 KiB`, and never write or spawn a process. Register both in `ToolName`, `manifest::KNOWN_TOOLS`, and the executor's exhaustive definition match.

```rust
#[tokio::test]
async fn search_files_matches_names_and_text_inside_the_workspace() {
    // notes/budget.md, notes/roadmap.md, and target/ignored.md exist.
    // A content search for "migration" finds notes/budget.md and skips target/.
}
```

- [ ] **Step 2: Add decision-model routing with a keyword fallback**

Define the seven intents and the `ChatRoute` provenance. `route_request` skips d1 for requests shorter than 24 characters or an obvious `general` keyword route, otherwise acquires a d1 lease and sends a `DecisionRequest` with all seven labels. A confidence below 5,500 basis points, an unknown label, or any model error keeps the keyword intent but records that d1 was consulted. `roles_for` reports the route, answer, observe, and hear roles and whether each is used.

```rust
#[test]
fn photo_requests_route_to_vision_with_both_models() {
    let route = ChatRoute::heuristic("What is in this screenshot?");
    assert_eq!(route.intent, ChatIntent::Vision);
    assert!(route.tools().contains(&ToolName::Look));
}
```

- [ ] **Step 3: Make prompts and grants capability-aware**

Always advertise `list_files`, `read_file`, `search_files`, `related_files`, `write_file`, and `edit_file`. Add `look` only when `has_model(Vision)` and `transcribe` only when `has_model(Audio)`. Extend the system prompt with the folder workflow, evidence-before-report rule, related-file guidance, and the statement that spoken replies are produced by the interface with the local LFM3 voice. Grant exactly the advertised tools in `execute_tool_call` and attach the model manager with `ToolRegistry::with_models` so `look` and `transcribe` can acquire their leases.

- [ ] **Step 4: Route each chat turn and expose the decision**

In `run_loop`, read the newest user message, compute capabilities, emit a `routing` status and then a `route` event, build the system message with the route note, and store the route in the first assistant message metadata. Raise the bounded turn to 12 rounds, 240 messages, 18,000 history characters, and 1,024 output tokens. Keep the same approval gate, cancellation path, and terminal refetch.

- [ ] **Step 5: Add image attachments and local speech output**

`attach_image` validates a 32 MB base64 bound, a 20 MB decoded bound, a PNG/JPEG/WebP/GIF extension, and a sanitized stem; it writes `.attachments/<uuid>-<stem>.<ext>` inside the workspace. `synthesize_speech` caps text at 1,500 characters and renders LFM3 speech with `llama-tts -m <audio weight> -mm <audio projector> -p <text> --tts-voice female --temp 1.0 --top-k 128 --repeat-penalty 1.1 -o <wav>`. The child is supervisor-owned, observed with a 120-second watchdog, and its WAV is checked for RIFF/WAVE before base64 is returned; one synthesis runs at a time. If the executable, weight, or projector is missing, Windows falls back to `System.Speech` and labels that result `system-sapi`. Add `voice_output` to the health report with `available`, `backend` (`lfm3-tts` or `system-sapi`), `model`, `lfm3_tts_weights`, and the engine note. Register both commands.

- [ ] **Step 6: Surface routing, attachments, and voice in the UI**

Add the `route` event and `ChatRoute`/`SpeechReply` types. Show a live route banner while a turn runs and a route badge on the first assistant message. Add a hidden image input and **+img** control that reads the file as base64 and calls `attach_image`. Add a **Speak** action per answer and a persisted header **Voice replies** toggle. Stop audio on send, new chat, session switch, toggle-off, and unmount; invalidate in-flight synthesis with a token. Add model chips for text, vision, voice input, decision, and voice output, where voice output reads `LFM3` for `lfm3-tts` and `system` only for the fallback. Style `.route-badge`, `.route-live`, `.speak-button`, `.badge-voice`, `.voice-note`, `.attach-button`, and `.visually-hidden`.

- [ ] **Step 7: Verify**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/ui
npm run typecheck
npm test
npm run build
```

Expected: all Rust tests pass (41 harness library at this task; Task 25 raises it to 60), 18 desktop plus one ignored real-voice test, 14 tool integration, 57 Vitest tests pass, and the production build succeeds. Run `cargo test -p lfm3-desktop lfm3_voice_supervisor -- --ignored` on a machine with the installed pair to exercise real speech. Do not claim full real-model qualification: routing, search, vision, audio, report writing, Tauri playback, and the profile-shaped job phases still need a measured run with the installed GGUFs. `llama-tts` itself is verified directly and through the runtime path.

---

### Task 24: Shape the stated-job loop around the LFM3 model envelope

**Status:** implemented on 2026-10-31. The scripted loop tests pass and all Rust and UI gates are green. A measured real-model run of the new phases is still open.

**Goal:** replace the generic free-form job loop with phases, a plan gate, deterministic step checks, and a review turn derived from the known strengths and weaknesses of LFM3-1.7B, so the harness compensates for the model instead of assuming general-agent behavior.

**Files:**
- Create: `crates/lfm3-harness/src/agent/profile.rs`, `crates/lfm3-harness/tests/loop_agent.rs`
- Modify: `crates/lfm3-harness/src/agent/mod.rs`, `agent/loop_agent.rs`, `lib.rs`, `job.rs`, `task.rs`
- Modify: `apps/desktop/src-tauri/src/jobs.rs`, `engine.rs`
- Modify: `crates/lfm3-cli/src/main.rs`
- Modify: `apps/ui/src/bridge/types.ts`, `bridge/jobReducer.ts`, `components/TodoTimeline.tsx`, `components/TaskComposer.tsx`, `index.css`

**Interfaces:**
- Produces: `ModelProfile { name, role, strengths, weaknesses, guidance, max_turns, max_calls, replay, max_output_tokens, max_plan_steps }`, `LFM3_TEXT`, `ModelProfile::loop_options(requested_turns) -> LoopOptions`, `ModelProfile::report() -> serde_json::Value`.
- Produces: `TodoCheck`, `TodoCheckKind` (`FileExists`, `DirExists`, `ContentMatches`), `LoopPhase` (`Plan`, `Work`, `Review`); `TodoItem.check`, `LoopTurn.phase`, `LoopOptions.plan_first`, `LoopOptions.max_plan_steps`.
- Produces: `JobTodoCheck`, `JobTodo.check`, `JobTurn.phase`; the `loop.turn` payload carries `phase`, and each todo carries its check.
- Produces: `Route::tools()` always includes `search_files` and `related_files`, plus `look`/`transcribe` only for routed senses; `/health` carries `model_profile`.
- Consumes: the existing `ToolRegistry`, `ChatRequest`, the job store and event stream, and the route from `TaskRouter`.

- [ ] **Step 1: Codify the model profile and derive loop limits**

Add `agent/profile.rs` with the envelope for LFM3-1.7B: strengths (bounded instruction following, short-context extraction, strict tool/JSON envelopes, concrete local edits, using concrete failure feedback) and weaknesses (long-horizon planning without a written plan, recalling unread content, self-certification, recovering from repeated failures, large working sets). `LFM3_TEXT` fixes `max_turns: 64`, `max_calls: 3`, `replay: 8`, `max_output_tokens: 2048`, `max_plan_steps: 8`. `loop_options` clamps a requested turn budget to the profile and always sets `plan_first`. Export the types from `agent/mod.rs` and `lib.rs`.

```rust
#[test]
fn the_profile_bounds_a_requested_turn_budget() {
    let options = LFM3_TEXT.loop_options(999);
    assert_eq!(options.max_turns, 64);
    assert!(options.plan_first);
}
```

- [ ] **Step 2: Add the plan gate and check-verified steps**

Expand `TodoItem` with an optional `TodoCheck`. In `apply_todos`, parse the check, reject a check path that leaves the workspace, require a non-empty `contains` for `content_matches`, and evaluate every `done` claim against the real workspace. A failed check demotes the step and returns the concrete reason in the tool output. Empty first plans and plans over `max_plan_steps` are refused. In the loop, start in `Plan` when `plan_first` and more than one turn is allowed: offer only `todo_write`, refuse `finish`, and move to `Work` only after a non-empty plan. A refused pre-execution call must not enter the duplicate-call set.

```rust
#[test]
fn a_finished_step_must_pass_its_real_check() {
    // out/report.md contains "alpha beta".
    // done + content_matches "alpha" stays done;
    // done + content_matches "gamma" is demoted with a reason.
}
```

- [ ] **Step 3: Enforce phase tool policy and add the review turn**

Add `tool_allowed(tool, phase, must_write)` and derive both the offered definitions and the executable set from it: no workspace tools in `Plan`, reads/edits/writes in `Review`, and only `write_file` in `Work` once `must_write` is set. Writing a non-empty deliverable transitions to `Review`, clears `must_write` so the output can be read, and records `reviewed` when the deliverable is read back. A first `finish` in `Review` without a re-read is refused once with a concrete instruction; completion still requires `verify_deliverable`. A stalled review with a valid proof passes; a plan-phase stall after three turns stops with `stalled`.

```rust
#[tokio::test]
async fn the_plan_gate_refuses_workspace_actions_on_the_first_turn() {
    // Turn 1 scripts read_file; it is refused with "plan phase" and the
    // job still reaches a measured pass after planning and writing.
}
```

- [ ] **Step 4: Route job tools and report the profile**

Change `Route::tools()` to always include `search_files` and `related_files` before read/write/edit, adding `look` and `transcribe` only for selected senses. In the desktop engine, grant exactly `route.tools()` through `ToolName::parse` and refuse an unknown route tool. Use `LFM3_TEXT.loop_options(request.max_turns)` and `max_output_tokens` for both the desktop job runner and the CLI, and add `model_profile` to `/health`.

```rust
let runner = AgentLoop::new(gateway, Arc::new(registry), model.clone())
    .with_options(LFM3_TEXT.loop_options(request.max_turns))
    .with_max_tokens(LFM3_TEXT.max_output_tokens);
```

- [ ] **Step 5: Surface phases and checks in the job UI**

Add `JobTodoCheck` and optional `check` on `JobTodo`, plus optional `phase` on `JobTurn`. Validate both in `jobReducer` (dropping a malformed check rather than rendering it), and render a phase chip per turn and a per-todo status of `verified`, `done (unchecked)`, or `open`. Update the job composer copy to explain the short checked plan, one bounded action, check-gated done, and re-read before finish. Add `.todo-check` and `.turn-phase` styles.

- [ ] **Step 6: Test and verify**

Add `tests/loop_agent.rs` with a scripted gateway covering: a planned, checked, reviewed pass; the plan gate refusing a workspace action on turn 1; `finish` refused once before a re-read; and the profile as the turn ceiling. Update the route tool expectation in `task.rs` and the reducer expectation for checks. Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/ui
npm run typecheck
npm test
npm run build
```

Expected: 47 harness library tests, 4 `loop_agent` integration tests, all other Rust suites pass, 57 Vitest tests pass, and the production build succeeds. Real-model qualification of the new phases remains open; do not claim it from these scripted tests.

### Task 25: Cover each local model's documented weaknesses with a harness helper

**Status:** implemented on 2026-10-31. All Rust and UI gates are green, and a measured real-model job still passes with the new sampling and advisory grounding note.

**Goal:** record the published envelope of all four local models in one place, lean acting on LFM3-1.7B's documented strengths, and add a deterministic helper for every documented weakness: measured sense facts, a d1 confidence floor, and an advisory grounding check.

**Files:**
- Create: `crates/lfm3-harness/src/agent/grounding.rs`
- Modify: `crates/lfm3-harness/src/agent/profile.rs`, `agent/mod.rs`, `agent/loop_agent.rs`, `agent/executor.rs`
- Modify: `crates/lfm3-harness/src/model/types.rs`, `model/client.rs`, `tools/senses.rs`, `chat/router.rs`, `lib.rs`
- Modify: `apps/desktop/src-tauri/src/engine.rs`
- Modify: `crates/lfm3-harness/tests/loop_agent.rs`, `tests/tools.rs`, `tests/model_client.rs`
- Modify: `README.md`, `PLAN.md`, `STEPS.md`

**Interfaces:**
- Produces: `ModelProfile { name, role, strengths, weaknesses, helpers, guidance, agentic, sampling, max_turns, max_calls, replay, max_output_tokens, max_plan_steps }`; constants `LFM3_TEXT`, `LFM3_VL`, `LFM3_AUDIO`, `D1`; `ModelProfile::ALL`, `all_reports()`.
- Produces: `model::Sampling { temperature, top_k, repetition_penalty }`, `ChatRequest::with_sampling`, optional wire fields `top_k` and `repetition_penalty`.
- Produces: `MIN_DECISION_CONFIDENCE_BPS`, `ChatRoute.confidence_floor_bps`; measured `harness facts` lines from `look` and `transcribe`.
- Produces: `agent::grounding::{Grounding, check}`; the review brief carries an advisory note, never a status change.
- Consumes: the job loop from Task 24, the shared model manager, and the route from Task 23.

- [ ] **Step 1: Complete the four envelopes and the model-card sampling**

Extend `ModelProfile` with `helpers`, `agentic`, and `sampling`. Add `LFM3_VL`, `LFM3_AUDIO`, and `D1` beside `LFM3_TEXT`, each naming at least three strengths, three weaknesses, and the helpers that cover them. Mark observation and decision models `agentic: false`; `loop_options` must return a single bounded turn for them. LFM3-1.7B uses temperature 0.1, top-k 50, repetition penalty 1.1; the others run greedily. Report all four from `/health` as `model_profiles` while keeping `model_profile` for the text model.

```rust
#[test]
fn observers_and_the_classifier_never_plan() {
    for profile in [LFM3_VL, LFM3_AUDIO, D1] {
        assert!(!profile.agentic);
        assert_eq!(profile.loop_options(64).max_turns, 1);
        assert!(!profile.loop_options(64).plan_first);
    }
}
```

- [ ] **Step 2: Send sampling on the wire**

Add optional `top_k` and `repetition_penalty` to `ChatRequest` and the wire body, with `with_sampling(Sampling)`. Use `LFM3_TEXT.sampling` in the acting loop instead of a hard-coded temperature. Omitted fields stay out of the JSON so an observation call at temperature 0 remains unchanged.

```rust
let chat = ChatRequest::new(messages)
    .with_tools(offered)
    .with_tool_choice("required")
    .with_sampling(LFM3_TEXT.sampling);
```

- [ ] **Step 3: Return measured facts with each observation**

In `look`, cap the image at 20 MB, parse PNG/JPEG/GIF/WebP dimensions, and precede the observation with `harness facts (measured): file=... format=... bytes=... dimensions=...`. Use `LFM3_VL.guidance` and `LFM3_VL.sampling`, and cap output at the profile budget. In `transcribe`, cap the recording at 64 MB, parse RIFF/WAVE `fmt `/`data` chunks for duration, sample rate, channels, and bit depth, and precede the verbatim transcript with the same measured header. Never guess a property the container does not expose.

```rust
#[test]
fn wav_facts_carry_duration_and_format() {
    let facts = audio_facts("note.wav", "audio/wav", &wav(16_000, 1, 16, 16_000), None);
    assert!(facts.contains("duration=1.00s"));
}
```

- [ ] **Step 4: Make the d1 confidence floor explicit**

Name the 5,500-bps floor, apply it through one predicate, bound the evidence to 8,000 characters, and expose the floor on `ChatRoute` for diagnostics. Below the floor, or for a `general` label, the deterministic keyword route wins. Routing still never grants tool authority.

```rust
fn accepts_decision(confidence_bps: u16, intent: ChatIntent) -> bool {
    confidence_bps >= MIN_DECISION_CONFIDENCE_BPS && intent != ChatIntent::General
}
```

- [ ] **Step 5: Add the advisory grounding note to review**

Extract distinct multi-digit numbers (and decimals) from the deliverable and compare them with numbers in the goal, the plan, and successful `read_file`, `look`, and `transcribe` outputs. Exclude a re-read of the deliverable itself. Append unsourced numbers to the review brief with wording that they must be verified or removed. Never alter `LoopStatus` and never block `finish`.

```rust
#[test]
fn a_number_the_model_never_saw_is_flagged() {
    let grounding = check("Revenue was 1,200 units in 2019.", &["Revenue was 1200 units."]);
    assert_eq!(grounding.unsupported, vec!["2019".to_owned()]);
}
```

- [ ] **Step 6: Test and verify**

Extend the scripted loop test to assert the review brief carries the grounding note, that a sourced number is not flagged, and that the acting request carries temperature 0.1, top-k 50, and repetition penalty 1.1. Extend the tools test to assert both measured headers, and the client test to assert the sampling fields on the wire. Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/ui
npm run typecheck
npm test
npm run build
```

Expected: 60 harness library tests, 5 `loop_agent` integration tests, 14 tool integration tests, all other Rust suites pass, 57 Vitest tests pass, and the production build succeeds. Then run one real LFM3-1.7B job to confirm the loop still plans, writes, reviews, and passes with the new sampling and grounding note.

---

## Dependency Order

```text
1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11
                                           ├→ 12 → 13 → 14 → 15 → 16 → 18 → 19 → 20 → 21 → 22 → 23 → 24 → 25
                                           └→ 17 ────────────────────→ 18
```

Task 17 may begin after Task 11 only if it does not delay the first desktop release. Task 18 is the verified-task release gate. Tasks 19–22 add the chat-first cowork surface on top of the accepted engine; Task 22 is the final static, UI, and documentation gate for the current release. Task 24 reshapes the stated-job loop around the measured LFM3 envelope and depends on the job runner from Task 15 and the route-aware tools from Task 23. Task 25 completes the envelope for all four models and adds a helper for each documented weakness; it depends on the loop from Task 24.

## Stop-and-Rethink Signals

- More than 40% invalid real-model plans after one repair: simplify mission contracts or use a stricter labelled-line parser producing the same `AcceptanceSpec`; do not loosen validation.
- A routine mission needs more than eight steps: split the mission or goal rather than raising budgets.
- A deterministic check can be spoofed through provenance: block release and fix the evidence model.
- Any confident semantic false pass on the frozen holdout: disable automatic semantic passing for that mission.
- Cancellation cannot interrupt an active call within the required timeout: block UI/release work and fix the execution boundary.
- The real chat model emits neither a native tool call nor a fenced tool block: keep chat prose-only and tighten the prompt or tool schema; never parse file access out of arbitrary prose.
- Any chat path can mutate a file without an explicit approval or can retire a pending action without a decision: block release and fix the approval gate.
- A chat message or tool result can be cited as mission evidence: block release and separate the stores and event channels.
- Voice falls back to a remote or hidden transcription service: block release; disable the microphone path instead.
- Any owned process survives packaged shutdown: block release and fix Job Object assignment or child creation flags.
- A real job spends three turns failing the plan gate, or a checked step repeatedly fails a check the model itself wrote: tighten the plan/check schema or the profile guidance; do not remove the gate or let `done` be self-certified.
- The model keeps calling `finish` in review without a re-read: keep the measured proof as the pass condition and surface the skipped review; do not accept prose as evidence.
- The grounding note blocks a job or rewrites the output on its own: keep it advisory; the measured deliverable proof remains the only completion authority.
- A profile claims a capability that this machine's weights have not demonstrated: remove the claim or mark it unqualified; do not let a model card outweigh measured behavior.
- An observation or decision model is offered the planning loop, or its confidence is treated as authority: fix the envelope wiring; none of the three is `agentic`.
- A llama.cpp compatibility probe fails: retain b11960 / v0.8.0; do not patch around it with an unqualified runtime upgrade.
