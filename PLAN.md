# LFM3 Local Harness — Rust-Only System Specification

Status: implemented through the multi-model chat-first cowork surface; verified-task release accepted  
Target: Windows 11, stable Rust MSVC, React + TypeScript, Tauri 2, CPU-first local inference  
Inference compatibility baseline: llama.cpp b11960 / v0.8.0, unchanged

## 1. Product contract

Build a **local, chat-first desktop cowork harness** with a separately verified mission engine.

The main surface is a conversation. The person types, attaches an image, or records a voice message; the local decision model routes the turn, the text model reasons with workspace tools, the vision model observes images, and the audio model transcribes speech. Any write or edit appears as an exact proposed change that executes only after explicit approval. Conversations are durable, cancellable, and entirely local.

The workspace skills are concrete rather than generic: map a folder, find a file by name or text, summarise the files that were actually opened, find related files, inspect an attached image, transcribe an audio file, and draft a Markdown report through the same approval-gated write path.

Verified work is a separate surface. The person selects a specialized mission, supplies structured inputs, reviews a machine-verifiable acceptance contract, and starts a run. A Rust engine plans and executes permitted actions, independent verifiers inspect real evidence, failed attempts produce informed retries, and the run passes only when every frozen step and the final mission output pass.

The two surfaces share the engine, model manager, tools, and process supervisor, but never their completion rules. A chat message, chat tool result, or chat approval is not evidence and can never satisfy a mission check.

The first end-to-end mission is `desk_task`, restricted to deterministic workspace transformations. `screen_qa`, `voice_intake`, and `doc_extract` are separate later missions, not modes of a general agent.

“Passed” has one meaning:

> Every frozen check passed against provenance-bound evidence, every required artifact still matches its verified hash, and the final mission-level output contract passed.

A model saying “done” is never completion evidence.

## 2. Rust-only decisions

1. The agent engine, planner, executor, tools, verifiers, persistence, model manager, CLI, and desktop supervisor are Rust.
2. React is used only for the desktop webview UI.
3. Tauri 2 embeds the Rust harness library directly. There is no Python sidecar and no separate production engine process.
4. Production UI/engine communication uses typed Tauri commands and events. There is no localhost HTTP server, bearer token, CORS policy, SSE connection, or PyInstaller bundle.
5. llama.cpp remains an external child process. Rust does not link llama.cpp through FFI and does not adopt a Rust llama.cpp binding.
6. Existing llama.cpp binaries and GGUF models are preserved. The pinned runtime remains b11960 / v0.8.0 until a newer build passes the complete compatibility suite.
7. No Python interpreter, Python package, Python test runner, Python build dependency, or Python maintenance script is part of development, packaging, or runtime.
8. A headless Rust CLI uses the same harness crate for tests, diagnostics, and real-model acceptance.

## 3. Non-goals

- No unverified work presented as a passed mission; chat remains advisory.
- No chat-originated write without a per-action approval.
- No cloud or remote speech recognition, audio upload, or hosted inference.
- No arbitrary “do anything” mission.
- No planner-authored unrestricted shell commands.
- No arbitrary browsing, remote fetching, cloud fallback, telemetry, or remote model endpoints.
- No autonomous background filesystem monitoring.
- No silent run resume after interruption.
- No unbounded retry loop.
- No model-judged override of a deterministic failure.
- No automatic model download during a run.
- No direct llama.cpp FFI or C++ ABI coupling.

## 4. Architecture

```text
┌────────────────────── Windows desktop process ──────────────────────┐
│ Tauri 2 application                                                 │
│                                                                   │
│ React + TypeScript UI                                               │
│  chat · voice · verified tasks                                      │
│                         │                                           │
│                         │ typed invoke/events                       │
│                         ▼                                           │
│ Tauri command layer                                                 │
│  chat runtime · approvals · native dialogs · artifact opening       │
│                         │                                           │
│                         ▼                                           │
│ lfm3-harness Rust crate                                             │
│  ├─ ChatStore            sessions, messages, action approvals       │
│  ├─ ChatRouter           d1 intent route + capability/role hints     │
│  ├─ ChatPrompt           system prompt, tools, bounded history      │
│  ├─ RunCoordinator       one active run, leases, interruption       │
│  ├─ Planner              structured draft plans                     │
│  ├─ AcceptanceCompiler   frozen criteria/checks/output contract     │
│  ├─ Executor             one permitted action per attempt           │
│  ├─ ToolGateway          scopes, approvals, typed results           │
│  ├─ SearchTools          name/text search and related-file ranking  │
│  ├─ Verifier             deterministic + semantic checks            │
│  ├─ RecoveryController   retry, strategy replan, no-progress guard  │
│  ├─ ModelManager         local llama.cpp clients and RAM leases     │
│  ├─ Store                SQLite, artifacts, events, migrations      │
│  └─ Cancellation       deadlines and active-work interruption       │
│                         │                                           │
│                         ▼                                           │
│ ProcessSupervisor                                                   │
│  owned llama.cpp/tool children · Windows Job Object · cleanup       │
└─────────────────────────────┬───────────────────────────────────────┘
                              │
                              ▼
        llama.cpp b11960 / v0.8.0
        LFM3-1.7B · LFM3-VL-2B · LFM3-Audio-2B · d1-2B
        llama-server for chat/vision/hearing/routing
        llama-tts for LFM3-Audio-2B speech output
```

The desktop application owns its inference children. Development may intentionally reuse externally started llama.cpp servers, but the application never kills servers it did not spawn.

## 5. Planned repository layout

```text
Cargo.toml                       Rust workspace
Cargo.lock                       Reproducible Rust dependency lock
rust-toolchain.toml              Stable MSVC toolchain policy
missions/                        Versioned mission JSON manifests
crates/
  lfm3-harness/
    Cargo.toml
    src/
      lib.rs                     Public engine API
      config.rs                  Validated local configuration
      error.rs                   Typed errors and outcome reasons
      cancel.rs                  Cancellation and deadline context
      ids.rs                     Strong run/step/attempt/artifact IDs
      agent/
        mod.rs
        planner.rs               Plan drafting and one repair pass
        executor.rs              Model action selection
        runner.rs                Run/step/attempt state machine
        recovery.rs              Retry and strategy-only replanning
        packets.rs               Compact failure packets
      mission/
        mod.rs
        manifest.rs              Mission schema and capability validation
        acceptance.rs            Frozen AcceptanceSpec and hashing
      model/
        mod.rs
        client.rs                llama.cpp HTTP protocols
        manager.rs               RAM reservations and model leases
        process.rs               Owned server lifecycle
        types.rs                 Typed chat/tool/sense requests
      chat/
        mod.rs                   Chat transcript feature surface
        store.rs                 chat.db sessions, messages, actions
        prompt.rs                System prompt, tool schemas, history window
        router.rs                d1 intent routing with keyword fallback
      tools/
        mod.rs                   Tool trait and registry
        fs.rs                    Sandboxed file transformations
        search.rs                Bounded name/text search and related files
        command.rs               Predefined command templates
        senses.rs                Vision/audio tool adapters
      verify/
        mod.rs                   Check registry and provenance rules
        deterministic.rs         Files, hashes, schemas, commands
        semantic.rs              d1/VL assessments and uncertainty
        evidence.rs              Typed evidence and artifact binding
      store/
        mod.rs                   SQLite actor and snapshots
        migrations.rs            Versioned schema
        events.rs                Durable event journal and broadcast
        artifacts.rs             Atomic content-addressed artifacts
      coordinator.rs             Single active run and engine lease
      supervisor.rs              Owned child-process abstraction
  lfm3-cli/
    Cargo.toml
    src/main.rs                  doctor/plan/run/status/models commands
apps/
  ui/                            Vite + React + TypeScript
    src/bridge/                  EngineBridge, TauriBridge, DemoBridge
    src/chat/                    ChatView, sidebar, composer, action cards, reducer, audio
    src/components/              Mission/run/evidence components
    src/App.tsx                  Chat-first shell with a verified-tasks switch
    src/WorkspaceView.tsx        Verified mission workspace
    src/chat.css                 Chat surface styles
  desktop/
    src-tauri/
      Cargo.toml
      tauri.conf.json
      capabilities/default.json
      src/main.rs                Tauri setup and shutdown
      src/chat.rs                Chat turn loop, approvals, voice, live events
      src/commands.rs            Typed UI command surface
      src/events.rs              Durable event forwarding
      src/job_windows.rs         Windows Job Object ownership
scripts/
  smoke.ps1                      Packaged native smoke checks
  accept.ps1                     Static, UI, installer, and installed-smoke gates
  release-real-model.ps1         Packaged real-model adversarial acceptance
tests/
  fixtures/                      Inputs and adversarial acceptance cases
  golden/                        Expected run journals
  golden/release/                Per-case real-model reports and event journals
  golden/release-manifest.json   Reproducibility and scope-audit record
```

No source file should mix UI commands, model networking, run-state transitions, and chat-turn orchestration. The harness crate must run headlessly without Tauri; the desktop `chat.rs` owns turn lifecycle and approval waits but delegates transcripts, prompting, tools, and model access to the harness.

## 6. Rust technology boundaries

Core dependencies are selected for a narrow purpose:

- `tokio`: async runtime, timers, child processes, cancellation-aware waits.
- `serde` and `serde_json`: persisted and wire contracts.
- `reqwest` with Rust TLS: loopback llama.cpp HTTP only.
- `thiserror`: typed internal errors.
- `uuid`: strong identifiers.
- `sha2`: acceptance and artifact hashes.
- `rusqlite` with bundled SQLite: durable local state.
- `axum` is not required in production because there is no HTTP engine API.
- `jsonschema`: mission input and output validation.
- `regex`: bounded content checks.
- `tracing` and `tracing-subscriber`: structured local logs.
- `directories`: per-user application-data paths.
- `sysinfo`: physical memory and resource observations.
- `windows`: Windows Job Object supervision in the desktop/supervisor boundary.
- `tauri` v2 and `tauri-plugin-single-instance`: desktop shell.

Dependencies must not introduce a Python runtime, a remote telemetry default, or a second model runtime.

## 7. Local model routing

| Model | Harness role | Never permitted |
|---|---|---|
| LFM3-1.7B | Conversational replies, workspace tool calls, structured plan drafts, one-action selection, failure explanation, strategy proposals | Self-certification, frozen-check mutation, direct visual/audio interpretation |
| LFM3-VL-2B | Bounded observation and visual semantic assessment; chat `look` over workspace images | Planning, tool authority, non-visual reasoning |
| LFM3-Audio-2B | Local voice-input transcription for the chat composer, LFM3 speech output through `llama-tts`; chat `transcribe` over workspace audio files | Planning, inventing fields, judging correctness, fabricating a transcript |
| d1-2B | Calibrated classification and chat intent routing, semantic support, advisory risk labels | Security enforcement, plan generation, overriding deterministic failures |
| Windows SAPI | Fallback rendering of one chat answer to WAV only when the LFM3 voice pair is incomplete | Being presented as an LFM3 model, used while LFM3 TTS is available, cloud speech, or evidence |
| Rust verifier | Deterministic artifact/process/schema checks | Subjective claims without measurable evidence |

Every llama.cpp endpoint must resolve to `127.0.0.1` or `::1` in the packaged profile. Startup records the server build, model identity, context settings, projector paths, and endpoint capabilities.

Chat uses the text model's streaming chat-completions endpoint. When d1 is installed, the chat runtime first asks it for one intent label and confidence, then gives the text model a route hint and only the tools that route can serve. A low-confidence or malformed decision falls back to deterministic keyword routing, so routing can slow a turn but cannot fail it. The manager serializes inference behind one lease, so a chat turn and a mission planner never call the model concurrently. Voice input is an optional audio lease: the runtime registers the audio model only when both its GGUF and an `mmproj` exist, and the UI disables the microphone path otherwise. Audio reaches the model as an `input_audio` content part; it is never uploaded.

Spoken replies use the same LFM3-Audio-2B any-to-any model through `llama-tts`. The model card documents the pinned b11960 runtime, the main audio GGUF plus the audio projector, `--tts-voice female|male`, and audio sampling at temperature 1.0, top-k 128, repetition penalty 1.1. `synthesize_speech` returns `backend: "lfm3-tts"`, `model: "LFM3-Audio-2B"`, and `lfm3_tts_weights: true` when the binary and both files exist. The output is a local WAV data URL played by the webview; nothing is uploaded and no voice is downloaded. If the pair is incomplete, Windows falls back to the offline system voice and reports `system-sapi` with `lfm3_tts_weights: false`; the UI repeats whichever note the engine reports.

The initial pinned flags remain part of compatibility fixtures: `--jinja` for text/tool use, matching `mmproj` for VL/audio, d1 `-b 8192 -ub 8192`, CPU `-ngl 0`, and the existing `/v1/systemone` protocol. A llama.cpp upgrade is a separate qualification task, never an implicit application update.

## 8. Missions and capability limits

A mission manifest is versioned data validated at startup. It defines:

- Mission name, version, purpose, and input schema.
- Permitted tool names and argument constraints.
- Permitted check kinds and check-spec schemas.
- Read and write scope rules.
- Optional predefined command-template IDs.
- Required models.
- Final output schema and required artifacts.
- Maximum steps, total attempts, strategies, duration, and model calls.

The planner cannot add a tool, check kind, model, write scope, or network capability.

### 8.1 First mission: `desk_task`

Initial tools: `list_files`, `read_file`, `write_file`, `edit_file`.

Optional test execution uses only user-configured, versioned command templates with fixed executable paths and validated argument slots. The model cannot supply an arbitrary command line.

The initial mission excludes browsing, memory, audio, vision, and arbitrary shell. Its first acceptance example is:

```text
Read demo/notes.md, write a specified summary to a run-approved output path,
and prove existence, non-empty content, required source coverage, and final hash.
```

### 8.2 Later independent missions

- `screen_qa`: imported images plus explicit criteria; visual observations and structured verdict evidence. It does not control the desktop.
- `voice_intake`: imported audio plus a fixed JSON schema; the transcript is an immutable source artifact.
- `doc_extract`: imported document pages plus requested fields; output values are read from the actual result artifact for grounding and numeric checks.

A mission that cannot meet its semantic-verifier gate ships human-review-only or not at all.

## 9. Frozen acceptance contract

Planning produces two different objects:

- `AcceptanceSpec`: what must be true.
- `ExecutionStrategy`: how the agent intends to achieve it.

`AcceptanceSpec` contains:

```text
acceptance_id and canonical SHA-256 hash
mission name/version and user goal
ordered steps and dependencies
frozen criterion per step
frozen checks, arguments, thresholds, and output targets
input artifact IDs and source hashes
required final artifacts and output schema
verifier protocol version
```

The flow is:

1. Validate mission inputs and copy imported inputs into trusted run storage.
2. Ask LFM3-1.7B for a structured draft within the mission vocabulary.
3. Validate every path, tool, check, threshold, and output requirement.
4. Permit exactly one plan-repair response containing validator errors.
5. Present the proposed acceptance contract in the UI.
6. On approval, canonicalize and hash it, persist every step as `pending`, and freeze it.
7. Attach the acceptance hash to every strategy, action, attempt, tool result, and check result.

Replanning returns only a new `ExecutionStrategy`. It must reference the same acceptance hash. Changing a criterion, removing a check, lowering a threshold, changing a target, or replacing the final schema requires a new run and new user approval.

## 10. Agent state machine

### 10.1 Run states

```text
created → planning → awaiting_plan_approval → running → final_verifying → passed

Active states may also enter:
awaiting_user
cancelling → cancelled
incomplete
interrupted
```

Terminal states are `passed`, `incomplete`, `cancelled`, and `interrupted`. `incomplete` includes a machine-readable reason such as `budget_exhausted`, `no_progress`, `approval_denied`, `unverifiable`, or `model_unavailable`.

### 10.2 Step states

```text
pending → running → verifying → passed
                    │  └→ retrying → running
                    └→ replanning → running

running/verifying/retrying/replanning may become blocked or cancelled.
```

### 10.3 Attempt states

```text
proposed → guarded → executing → verifying → passed
                                      ├──────→ failed
                                      ├──────→ inconclusive
                                      └──────→ aborted
```

An attempt row is committed before any side effect. It records the action, acceptance hash, model identity, starting artifact manifest, deadline, and strategy number.

After all steps pass, the runner executes `final_verifying`. Any later artifact-hash mismatch or final-contract failure makes the run `incomplete`, never silently passed.

## 11. One attempt end to end

1. Acquire the single active-run lease and required model lease.
2. Build a compact context containing the mission purpose, frozen step, current strategy, relevant input manifests, previous failure signatures, and remaining budgets.
3. Ask LFM3-1.7B for exactly one permitted tool action.
4. Validate the action against the mission and current state.
5. Persist the proposed attempt and canonical action hash.
6. Run risk/policy checks and request approval when the tool template requires it.
7. Execute with cancellation and deadline contexts.
8. Store a typed `ToolResult` and all output artifacts.
9. Run each frozen check against the required provenance.
10. Commit attempt results and state-transition events in one transaction.
11. Advance only if all checks pass; otherwise classify the failure and recover according to policy.

Tool output, file content, transcripts, and model observations are untrusted data. They cannot alter the system prompt, acceptance contract, tool registry, or approval policy.

## 12. Typed tools and evidence

### 12.1 ToolResult

Every tool returns structured data containing:

```text
attempt_id, tool name/version, validated arguments
success | tool_error | infrastructure_error | timeout | cancelled
optional process exit code
stdout/stderr artifact references
input/output artifact references and SHA-256 hashes
model/server metadata when applicable
start/end timestamps and typed error details
```

### 12.2 CheckResult

Every check returns:

```text
frozen check_id and attempt_id
pass | fail | inconclusive | error
verifier name/version
expected and observed values
non-empty typed evidence references
optional model confidence and model identity
```

### 12.3 Provenance rules

- `command_exit` reads the typed exit code from the command executed in the same attempt.
- `tests_pass` uses the process result and, where supported, a machine-readable test report. Arbitrary text containing `exit=0` is not evidence.
- `file_exists` checks type and non-zero size, then records the hash.
- `file_changed` compares before/after hashes from the same scoped target.
- `content_matches` rejects an empty condition set.
- `json_schema` validates the actual output artifact, not a model claim about it.
- `d1_grounded` reads output values through explicit JSON pointers and uses the immutable source transcript artifact.
- `vl_criterion` records the source image hash, exact prompt, model identity, and raw response. A d1 judgment over a VL description is not independent pixel evidence.
- `report_complete` validates structured report sections, not substring presence.
- A check error or unavailable judge is not a pass.

Deterministic failures cannot be overridden by d1, VL, or the text model.

## 13. Retry, replanning, and no-progress control

The agent retries every recoverable failure while a new, permitted strategy remains. It does not repeat an action forever and does not claim success when progress is impossible.

Default `desk_task` budgets:

- 8 steps.
- 3 attempts per strategy.
- 2 alternative strategies per step.
- 6 total attempts per step across all strategies.
- 15-minute run deadline.
- Explicit total model-call and semantic-judge budgets.

All limits are finite and validated. Replanning never resets the total-attempt counter.

Failure classes:

- **Check failed:** retry with the compact failure packet if attempts remain.
- **Transient infrastructure error:** bounded exponential backoff; an identical retry is allowed only for this class.
- **Invalid model action:** record failure and request a corrected action.
- **Repeated action:** reject before side effects and request a new strategy.
- **Repeated failure signature:** trigger no-progress handling even when actions differ.
- **Alternating cycle:** detect canonical action/failure pairs across the full step history.
- **Inconclusive semantic check:** request better evidence or human review; never treat it as pass.
- **Approval denied/missing model/unverifiable goal:** stop the affected step truthfully.
- **Budget exhausted:** mark the run incomplete with the failed criterion and evidence.

A failure packet contains the acceptance hash, criterion, failed checks, typed evidence references, relevant output excerpts, artifact diffs, prior action/failure signatures, and remaining budgets. It does not replay the entire transcript.

## 14. Cancellation and deadlines

A per-run `CancellationContext` is passed through planning, model requests, tools, commands, senses, approvals, and verification.

Cancellation must:

1. Transition the run to `cancelling` and persist the event.
2. Resolve pending approvals as cancelled.
3. Abort active HTTP requests.
4. Terminate the active owned child process tree.
5. Prevent every later transition to `passed`.
6. Persist `cancelled` only after active work stops or a hard-stop timeout records the remaining uncertainty.

Every network and process call receives the remaining run deadline. Checking a deadline only between calls is insufficient.

## 15. Persistence and interruption recovery

Trusted state is stored outside mutable mission workspaces:

```text
%LOCALAPPDATA%\LFM3Harness\
  runs.db
  chat.db
  artifacts\
  logs\
  engine.lock
```

SQLite tables:

```text
schema_migrations
engine_instances
runs
acceptance_specs
steps
strategies
attempts
tool_results
check_results
artifacts
events
approvals
model_leases
run_links
```

`chat.db` is a separate database with `chat_sessions`, `chat_messages`, and `chat_actions`. Its messages, tool results, and approvals never enter `runs.db` and cannot be cited as mission evidence.

Artifacts are content-addressed and written atomically. Large stdout, transcripts, images, and reports are artifact references rather than inline database text.

State changes and their durable events commit in the same SQLite transaction. Live Tauri events are emitted only after commit. Each event has a monotonically increasing sequence and run ID.

At startup:

1. Acquire the engine lock and reconcile the previous engine instance.
2. Mark its non-terminal runs `interrupted`.
3. Reconcile non-terminal steps, attempts, approvals, and leases.
4. Append interruption events.
5. Reconcile `chat.db`: unfinished assistant messages become `cancelled` and pending actions become `expired`.
6. Execute nothing automatically.

“Retry run” creates a linked run with the same acceptance hash. A previously passed step can be skipped only after fresh checks and artifact-hash verification. An interrupted side-effecting attempt has unknown outcome and is not blindly replayed.

## 16. Single-run and resource coordination

`RunCoordinator` permits one active run per engine. A second start returns a typed `RunBusy` error; no queue is added initially.

The same coordinator is used by desktop and CLI. A database engine lease prevents a CLI and desktop instance from mutating runs concurrently.

Model execution is sequential. `ModelManager`:

- Reserves memory before loading a model.
- Pins a model only for an explicit lease.
- Rejects eviction while leased.
- Reuses the same scheduler for planner, executor, senses, judges, and chat.
- Verifies endpoint model identity before reuse.
- Fails clearly when the reservation cannot be satisfied.
- Tracks process ownership so external development servers are never terminated.

### 16.1 Chat sessions and the conversational loop

Chat is a first-class, durable surface next to verified runs, with its own storage and lifecycle:

```text
chat.db
  chat_sessions(id, title, created_at, updated_at)
  chat_messages(id, session_id, seq, role, content, status, meta, created_at)
  chat_actions(id, session_id, message_id, tool, arguments, summary, status, created_at, decided_at)
```

A message status is `complete`, `streaming`, `cancelled`, or `error`. An action status is `pending`, `approved`, `denied`, `applied`, `failed`, or `expired`. On open, the store repairs an unclean shutdown: `streaming` messages become `cancelled`, and `pending` actions become `expired`. Only `pending` may transition to a decided or expired state.

One chat turn may be active per engine. A turn:

1. Requires a non-empty message of at most 32,000 characters and appends it durably.
2. Builds a fixed system prompt with the workspace root, the decision route, the capability-appropriate tool schemas, and the newest 240 messages capped near 18,000 characters.
3. Routes the turn. If d1 is installed and the keyword route is not trivially `general`, one d1 lease classifies the request into `general`, `explore`, `summarize`, `related`, `report`, `vision`, or `voice`. A route below 55% confidence, an invalid label, or a d1 failure keeps the deterministic keyword intent but records that d1 was consulted. The route, its source, confidence, reason, and model roles are emitted as a `route` event and stored in the first assistant message's metadata.
4. Acquires one text-model lease for the whole turn, so mission inference waits between turns and chat never races another model call.
5. Streams the reply over SSE, accumulating content and native tool-call fragments by index. It allows at most twelve tool rounds, `temperature 0.3`, and 1,024 output tokens.
6. Accepts a fenced ```tool JSON block as a fallback when a small model cannot emit a native call.
7. Grants `list_files`, `read_file`, `search_files`, `related_files`, `write_file`, and `edit_file` through the same workspace-confined `ToolRegistry` as missions. `look` is added only when the vision model is installed, and `transcribe` only when the audio model is installed; the tool definitions and grants are derived from the same capability check. `search_files` and `related_files` walk regular files with depth, entry, and byte caps and skip links and dependency folders. Read-only tools run immediately; writes and edits require approval.
8. Persists every assistant message, tool result, and action, then ends the turn with one terminal event.

A write or edit creates a pending action with a human-readable summary and the exact arguments. The runtime emits the action card, waits up to ten minutes for a decision, and executes only after **Allow once**. Denial returns a tool message telling the model not to retry; expiry and cancellation leave the file untouched. Action resolution is persisted before the tool result reaches the model.

`cancel_chat` cancels the active turn for that session, marks a streaming assistant message `cancelled`, and expires pending actions. It never turns a partial reply into a completed one. A second send while a turn is active is refused with a typed conflict instead of queued.

Voice input is client-side capture only. The webview records with `MediaRecorder`, decodes the clip, resamples it to 16 kHz mono PCM, writes a WAV container, and sends base64 to `transcribe_voice`. Rust validates the format and size, acquires the audio-model lease, and sends a chat request whose user content contains an `input_audio` part. The resulting transcript is returned to the composer for review; it is never auto-sent. Missing audio weights or `mmproj` disable the path with an explicit error.

Image input is also user-initiated and local. `attach_image` accepts a name and base64 for a PNG, JPEG, WebP, or GIF up to 20 MB, writes it under `.attachments/<uuid>-<sanitized-stem>.<ext>` inside the workspace, and returns the relative path. The composer appends an instruction naming that path. The model may then call `look`, which reads the confined path, checks the media type, and sends a base64 data URL to the local vision model. This path is not a model-authored write and has no run-journal effect.

Spoken replies use `synthesize_speech`. It renders LFM3 speech with `llama-tts -m <audio weight> -mm <audio projector> -p <text> --tts-voice female --temp 1.0 --top-k 128 --repeat-penalty 1.1 -o <wav>`, capped at 1,500 characters. The process is spawned owned by the supervisor, observed with a 120-second watchdog, killed on timeout, and its WAV is checked for the RIFF/WAVE header before base64 is returned; the temporary directory is always removed. Only one synthesis runs at a time. If the executable, weight, or projector is missing, Windows falls back to a 60-second `System.Speech` render and returns `backend: "system-sapi"` with an explicit note. The UI repeats the engine's note and never presents the fallback as an LFM3 model.

Chat has no acceptance authority. It never writes to the run journal, never starts a run, and its approvals are scoped to a single file mutation. Any task that needs certified completion must use the verified-task surface.

### 16.2 The stated-job loop and the model envelope

A stated job (`lfm3 job`, the desktop job form, `start_job`) is not a mission: there is no frozen plan or acceptance hash. It is a bounded loop over the same workspace-confined tools, and its shape is derived from the measured strengths and weaknesses of LFM3-1.7B rather than a general-purpose agent default.

`agent::profile::ModelProfile` is the single source of the envelope. `LFM3_TEXT` names the model, both sides of its behavior, written guidance, and hard limits (`max_turns`, `max_calls`, `replay`, `max_output_tokens`, `max_plan_steps`). `loop_options(requested_turns)` clamps a caller's request to the profile, so no surface can ask for more turns than the model can sustain. `/health` reports the same profile.

The loop has three phases. The harness, not the prompt, enforces which tools each phase may execute:

| Phase | Offered tools | Rule |
|---|---|---|
| `plan` | `todo_write` | One to eight steps before any workspace action. Empty plans and `finish` are refused. |
| `work` | Granted workspace tools, then `write_file` only once all sources are read | One bounded action per turn. A `done` claim is accepted only if its deterministic check passes. |
| `review` | `read_file`, `write_file`, `edit_file`, `finish` | Opens when a non-empty deliverable is written. Re-read the output, repair it, or finish. |

A plan step may carry a check: `file_exists`, `dir_exists`, or `content_matches` with a non-empty `contains`. Checks are evaluated against the real workspace with the same workspace-confinement rules as tool paths. A failed check demotes the step to open and reports the concrete reason, so the model's "done" is measured rather than asserted. Checks are advisory for progress, never a substitute for the final proof.

Completion is unchanged: `finish` passes only when the required output exists and is non-empty, and the report carries the measured bytes and digest. In the review phase, a first `finish` without a re-read is refused once with a concrete instruction; if the model insists, the measured proof still decides. A stalled review with a valid artifact passes; a job with no plan after three turns, no write after six turns, an expired deadline, or an exhausted turn budget stops with `stalled`, `deadline`, or `turn_budget`, never as a pass.

Job tool grants follow the route. `search_files` and `related_files` are always granted; `look` and `transcribe` only when the route selects `observe` or `hear`. Routing widens capability, never authority; writes still require approval and paths remain confined to the workspace.

The plan phase, the check gate, the write-narrowing rule, and the review turn each compensate for a named weakness (long-horizon planning without externalization, self-certification, drift to sources, and skipping verification), while the bounded call count and strict envelopes use the model's strengths.

### 16.3 One envelope per local model, with a helper for every weakness

The harness addresses four local models, and each is trusted only with what its published evaluation supports. `agent::profile` records all four as `ModelProfile` values; `/health` reports every envelope, including the helpers that cover its weaknesses. Observation and decision models are marked `agentic: false`, and `loop_options` returns a single bounded turn for them, so they can never be handed the planning loop by mistake.

| Model | Used for | Documented weakness | Harness helper |
|---|---|---|---|
| LFM3-1.7B | planning, bounded acting, short-extract synthesis | long-horizon planning, unread recall, self-certification, invented figures, failure loops | plan gate, deterministic todo checks, read-before-write guard, review turn, measured digest, advisory grounding note, harness-owned stall budget |
| LFM3-VL-2B | OCR, documents, charts, GUI screenshots, grounding | no native tool calling, weak coding/multi-turn/non-visual reasoning, cannot judge correctness or know file properties | one bounded instruction per call, observer-only prompt, harness-measured format/bytes/dimensions, and the text model synthesises |
| LFM3-Audio-2B | verbatim transcription in 18 languages | weak audio understanding and reasoning, no documented speaker ID or timestamps | transcription-only contract, harness-measured container facts and duration when the container exposes it, size cap, text model analyses the transcript |
| d1-2B | advisory routing and typed decisions | not a chat or knowledge model, uncalibrated confidence, estimated counts, degrades on long input | closed label set, bounded evidence, harness-owned 5,500-bps floor with keyword fallback, never an authority |

Three details carry the design:

- **Sampling follows the model card.** LFM3-1.7B acts at temperature 0.1, top-k 50, repetition penalty 1.1, sent only when set. Observation and decision calls run greedily at temperature 0.
- **Facts and observations stay separate.** `look` and `transcribe` return a measured `harness facts` line (format, bytes, dimensions; duration and sample rate for WAV) before the model's observation or transcript. The observer is never asked for a file property and never asked to judge.
- **Grounding is advisory, not a verdict.** During the review turn the harness compares multi-digit numbers in the deliverable with the goal, the plan, and successful observations. Unsourced numbers are listed in the brief so the model can verify or remove them. The check never changes the loop status; the measured deliverable proof remains the only completion authority.

## 17. Tauri 2 command and event contract

Production uses direct typed commands, not HTTP. Representative commands:

```text
health()
list_missions()
list_runs(limit)
start_run(mission, inputs) -> run_id
get_run_snapshot(run_id)
get_run_events(run_id, after)
approve_plan(run_id, acceptance_hash)
decide_approval(approval_id, allow)
cancel_run(run_id)
retry_run(run_id) -> new_run_id
list_models()
release_model(model_key)
choose_input_files(mission, input_key)
open_artifact(artifact_id)

start_job(goal, deliverable, minutes, max_turns) -> job_id
get_job(job_id)
get_job_events(job_id, after)
list_jobs(limit)
cancel_job(job_id)

list_chat_sessions(limit)
create_chat_session(title?)
get_chat_session(session_id)
rename_chat_session(session_id, title)
delete_chat_session(session_id)
send_chat_message(session_id, content) -> message
cancel_chat(session_id)
decide_chat_action(action_id, allow)
transcribe_voice(audio_base64, format) -> transcript
synthesize_speech(text) -> speech_reply
attach_image(name, data_base64) -> workspace_relative_path
```

The command layer validates current run state. For example, plan approval must match the current acceptance hash, and an approval ID must be pending, unexpired, and bound to the active run. Chat commands validate session ownership, non-empty bounded content, a single active turn, the allowed audio format, and a pending unexpired action.

Tauri events mirror durable store events on `lfm3:run-event`: `run.started`, `plan.ready`, `step.started`, `attempt.started`, `tool.result`, `check.result`, `attempt.failed`, `step.replanned`, `step.passed`, `approval.requested`, `model.loading`, `model.ready`, `model.evicted`, and `run.finished`.

Live chat events use a separate channel, `lfm3:chat-event`. Each event is a flat payload tagged with `session_id` and one `kind`: `user_message`, `session_updated`, `assistant_start`, `assistant_delta`, `assistant_done`, `route`, `action_requested`, `action_resolved`, `tool_result`, `status`, `error`, and `done`. The UI filters by active session; a `done` event triggers an authoritative refetch of the session view and the session list.

The UI keeps the last applied sequence. On reconnect or a gap it calls `get_run_snapshot` and `get_run_events(after)` before resuming live events.

Frontend capabilities exclude shell execution and broad filesystem access. Native dialogs and artifact opening are mediated by Rust commands.

## 18. React UI

Use Vite, React, TypeScript, and small reducer-based stores for runs and chat. Define an `EngineBridge` interface with two implementations:

- `TauriBridge`: production typed commands/events.
- `DemoBridge`: explicitly labelled browser fixtures for component and Playwright tests. It serves verified-run scenarios only and refuses chat. It cannot be selected in the packaged application.

The application opens on chat. A sidebar holds conversation history, new-chat, model status, and the switch to verified tasks. Views:

1. **Chat:** durable transcript, streaming reply, typing/status line, live decision-route banner, per-message route badge, workspace tool output, inline approval cards with exact content or diffs, stop control, and a composer with Enter-to-send, Shift+Enter newline, local voice recording, and a **+img** attachment control. Assistant answers have a **Speak** action and the header has a persistent **Voice replies** toggle; both use the local LFM3 voice and display the engine's backend note when enabled. An empty conversation offers folder, related-file, report, and image prompts. Errors are shown without inventing a reply.
2. **Verified tasks:** mission forms showing capabilities and verification methods, with the model dock and setup state.
3. **Acceptance review:** exact steps, checks, paths, schemas, permissions, budgets, and acceptance hash.
4. **Run workspace:** steps, attempts, retries, strategy changes, evidence, approvals, and always-available cancellation.
5. **Evidence inspector:** diffs, hashes, JSON validation, typed command results, transcripts, and clearly labelled semantic verdicts.
6. **History:** passed, incomplete, cancelled, and interrupted runs with linked retry runs.

Chat messages, tool notes, and action cards are visually distinct from verified evidence. The header labels chat "not verified"; verified views carry the frozen acceptance hash. Every failure names the failed criterion and what evidence would pass. Demo data is visibly labelled. Spinners include the current operation and model/resource state.

A live chat event is applied only when it belongs to the active session. Streaming text is held in a per-message overlay and replaced by the durable message on `assistant_done`; the view is refetched after `done`. Approvals update in place through `action_resolved`, and a turn running in another session disables sending there while keeping Stop available.

The route is visible rather than implied: a live banner names the intent, whether d1 or keywords chose it, the confidence, and the local roles used; the first assistant message keeps a badge and the decision reason. The sidebar reports the state of the text, vision, audio-input, decision, and voice-output capabilities independently, so a missing optional model is explicit. Starting a new turn, switching sessions, creating a chat, or unmounting pauses any spoken reply; an in-flight synthesis is invalidated so it cannot play into the wrong conversation.

## 19. Windows process ownership and packaging

There is one desktop executable containing Tauri and the harness library. There is no engine sidecar.

`ProcessSupervisor` creates a Windows Job Object with kill-on-close semantics. Owned llama.cpp, TTS, and approved command children are started suspended, assigned to the job, and resumed. Shutdown terminates only owned processes and waits for their exit.

The supervisor is also used by the CLI so headless acceptance tests receive the same cleanup behavior.

Packaging:

- Stable Rust MSVC and required Microsoft C++ build tools.
- Tauri 2 NSIS installer.
- React production assets embedded by Tauri.
- Mission manifests included as validated resources or compiled-in immutable assets.
- Absolute configured paths for models and llama.cpp binaries; never depend on current working directory.
- GGUF models and llama.cpp binaries remain external to the installer.
- The installed app must work from a path containing spaces, without the repository, Python, or a network connection.

## 20. Testing strategy

### 20.1 Fast Rust tests

- Unit tests for state transitions, acceptance hashing, check validation, failure signatures, deadlines, and cancellation.
- SQLite integration tests for migrations, transactional events, snapshots, leases, and interruption reconciliation.
- Chat tests for session/message round-trips, allowed action transitions, reopen reconciliation, bounded history reconstruction, tool-call metadata, fenced-call fallback parsing, decision-route selection and fallback, capability-aware tool definitions, and route-event serialization.
- Search tests for name/content matching, skip directories, depth and entry caps, escape rejection, related-file ranking, and seed exclusion.
- Streaming-client tests for SSE deltas, fragmented tool calls, `[DONE]`, and cancellation of a stalled response.
- Fake llama.cpp HTTP fixtures for chat/tool calls, `/v1/systemone`, vision, and audio protocol parsing.
- Tool tests for path traversal, reparse points, atomic writes, command-template validation, and typed provenance.
- Golden journals for pass, retry, replan, no-progress, inconclusive, cancellation, and interruption.

Commands:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### 20.2 UI tests

- Component tests for mission forms and state rendering.
- Chat reducer tests for streaming accumulation, terminal replacement, status transitions, decision-route display and clearing, action resolution, message ordering, session renames, and authoritative view merges.
- Chat component tests for route badges, the speak control, and tool-call-only messages.
- Playwright against the development-only `DemoBridge` fixture for plan review, retries, approvals, cancellation, reconnect, and history. The bridge is gated by `import.meta.env.DEV` and is absent from the production bundle; chat is explicitly unavailable in demo mode.
- Packaged Tauri smoke tests for native command wiring and process cleanup.

### 20.3 Real-model acceptance

Use the actual pinned GGUFs and llama.cpp binaries. Record model hashes, fixture hashes, llama.cpp build, acceptance hashes, sampling settings, attempts, evidence, wall time, and peak memory.

`scripts/release-real-model.ps1` performs this acceptance against the optimized CLI and writes per-case reports, event journals, stderr, and result JSON to `tests/golden/release/`. The consolidated record is `tests/golden/release-manifest.json`. The accepted release run covered eight cases: success, unsatisfiable failure, output escape, transient-write retry, owned-model crash, unavailable endpoint, whole-run timeout, and missing models. Every case ended with no leftover process and no false pass.

Measured limits of that run:

- Chat routing, folder search, related files, report writing, native streamed tool calls, the fenced-call fallback, approval expiry, vision attachments and voice transcription are covered by Rust/UI tests but have not been qualified end-to-end with the real models. `llama-tts` speech rendering was verified directly and through the runtime's owned-process test with the installed LFM3-Audio-2B pair; Tauri playback still needs a measured run.
- Semantic holdout qualification was not performed. `screen_qa`, `voice_intake`, and `doc_extract` remain `human_review_only`; automatic semantic passing is disabled.
- Real-model replanning was not triggered in the eight cases. It is covered by automated recovery/executor tests only.
- The executor uses its built-in 512-token completion cap and the planner a fixed 1536. `limits.max_tokens` (default 4096) is parsed and validated but not applied to those requests, so the configuration surface overstates what is wired.
- Wall times and peak memory are machine-specific; the NSIS installer is not claimed to be byte-reproducible across rebuilds.

Semantic judges use separate development and frozen holdout fixtures. Any confident false pass on the holdout disables automatic semantic passing for that mission. A calibration percentage alone is not sufficient evidence for automatic certification.

## 21. Implementation phases

1. **Native foundation:** Rust workspace, configuration, IDs, errors, mission schemas, CLI skeleton, and test fixtures.
2. **Durable truth:** SQLite, artifacts, events, frozen acceptance, typed evidence, and interruption reconciliation.
3. **Execution safety:** cancellation, one-run coordination, process supervision, model leases, and scoped tools.
4. **Agent loop:** planner, executor, verifier, immutable recovery, and final verification using fake models.
5. **Real `desk_task`:** actual LFM3 text planning/action with deterministic acceptance and adversarial recovery.
6. **React UI:** complete mission/run/evidence workflow through the bridge contract.
7. **Tauri desktop:** native commands, Job Object cleanup, setup, and installer.
8. **Additional missions:** visual/audio/document contracts with independent semantic gates.
9. **Packaged release:** offline real-model acceptance, crash/fault injection, performance records, and no process leakage.
10. **Chat-first cowork:** durable sessions, streaming text and tool calls, approval-gated workspace edits, local voice input, and a chat-first interface that keeps verified tasks one click away.
11. **Multi-model workspace agent:** d1 decision routing, bounded folder search and related-file discovery, capability-aware tool grants, user-initiated image attachments with VL `look`, report drafting, and local LFM3-Audio-2B speech output with an honest system-voice fallback.

Each phase must pass its gate before the next phase begins. `STEPS.md` defines the task-level order and exact interfaces.

## 22. Release definition of done

The first release is complete only when:

- No Python source, runtime, build dependency, or test dependency remains.
- The installed desktop application contains the Rust engine and works offline.
- The pinned llama.cpp runtime serves all required local models without an unqualified upgrade.
- A real LFM3 text model completes the first `desk_task` without scripted planning or actions.
- Acceptance criteria cannot change during retry or replanning.
- Wrong output cannot be certified as complete.
- Every pass links to inspectable provenance-bound evidence.
- Cancellation interrupts active work and cannot later become a pass.
- Interruption recovery is truthful and never silently resumes side effects.
- Only one active run can mutate a workspace.
- No owned llama.cpp or tool processes survive shutdown.
- Chat opens by default, persists sessions, streams local replies, and never represents itself as verified work.
- Chat-originated writes and edits require an explicit per-action approval and leave the file untouched on denial or expiry.
- Voice input is captured and transcribed locally; no audio or transcript is uploaded.
- Folder search, related-file discovery, image inspection, and report drafting are available through approval-gated workspace tools, not unrestricted shell or browsing.
- A decision route selects only local capability hints and is displayed with its source and confidence; it cannot grant authority or weaken a check.
- Spoken replies are synthesized locally by LFM3-Audio-2B through `llama-tts`; the operating-system voice appears only as a labelled fallback when the voice pair is incomplete.
- Arbitrary browsing, unrestricted shell, cloud fallback, hidden downloads, and remote speech recognition are absent.

Status at 2026-10-31: the verified-task items above are satisfied and recorded in `tests/golden/release-manifest.json`. The chat-first surfaces are implemented and covered by Rust and UI tests; real-model chat, tool-call, and voice qualification remain open and are documented as such. The release scope audit found zero Python files, project files, caches, or launch commands; no remote model endpoints; no Tauri shell plugin or broad filesystem capability; no download path; and no unguarded scripted fallback. The installed tree contains only the desktop executable, uninstaller, and four mission files whose hashes match the source.

This definition does not include semantic holdout qualification or real-model replanning. Both remain open for the next task: the three semantic missions cannot start, and replanning is proven only by automated tests.

## 23. Principal risks and responses

| Risk | Required response |
|---|---|
| Small text model emits invalid plans | Constrain schemas, allow one repair, then reject truthfully; use a labelled-line fallback only if it preserves the same typed AcceptanceSpec |
| Model proposes easier checks after failure | Impossible by design: recovery accepts only ExecutionStrategy with the same acceptance hash |
| Typed verifier still certifies the wrong artifact | Bind checks to attempt, artifact ID, path scope, and hash; add adversarial provenance fixtures |
| Semantic judge produces a confident false pass | Disable automatic semantic pass for that mission and require human review |
| App crash leaves inference children running | Windows Job Object kill-on-close plus packaged crash testing |
| CLI and desktop run concurrently | Shared engine lease and typed RunBusy outcome |
| Small chat model cannot emit native tool calls | Accept a fenced tool block, keep the same validation and approval gate, and surface a prose-only answer honestly |
| Chat reply is mistaken for verified work | Separate `chat.db` and event channel, "not verified" labels, and no path from chat events to the run journal |
| Workspace text in a chat prompt induces an unwanted write | Workspace-confined tools plus per-action approval; denial and expiry leave files untouched |
| A chat turn monopolizes the model | One lease per turn, twelve-round and token caps, whole-turn timeout, and cancellation that expires approvals |
| Decision routing mislabels a request | Routing only selects hints and tools; keyword fallback handles absence and low confidence, and the text model still needs the same evidence and approval |
| A small model invents a folder summary it did not read | The system prompt requires list/search/read before summarising, search tools are bounded, and the UI shows every tool result |
| Voice audio leaves the machine | Capture in the webview, transcribe through the loopback audio model only, and disable the path without local weights |
| Speech output is mislabelled | The health report names `lfm3-tts` and `LFM3-Audio-2B` when ready, or `system-sapi` with `lfm3_tts_weights` false on fallback; the UI repeats the engine note |
| Rust rewrite creates a large untested surface | Headless crate first, narrow modules, fake-model golden runs, UI only after the real desk_task gate |
| llama.cpp upgrade breaks LFM3 protocols | Keep b11960 / v0.8.0 pinned and qualify upgrades separately against all model fixtures |
