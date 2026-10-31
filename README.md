# LFM3 Local Harness

A fully local, chat-first desktop cowork harness for Liquid AI's LFM3 model family, built with Rust, React, Tauri 2, and external llama.cpp processes.

The primary surface is a conversation. You can type, attach an image, or record a voice message, and the local models can map a folder, find and summarise files, trace related files, inspect images, and propose a Markdown report or file edit. Writes and edits are shown as an exact diff and execute only after you approve them. A local decision model routes each turn, the vision model observes attached images, and LFM3-Audio-2B both transcribes speech and synthesizes spoken replies locally through `llama-tts`.

Chat is deliberately **not** verified work: anything that must be machine-certified is started as a mission under **Verified tasks**, where frozen acceptance contracts, independent checks, informed retries, and provenance-bound evidence decide whether a run passes. A chat message can never satisfy a mission check.

## Current status

This repository is an all-Rust harness with a working desktop application and headless CLI:

- `crates/lfm3-harness` contains the engine: frozen acceptance contracts, planning, confined tools, provenance-bound deterministic checks, recovery budgets, the run coordinator, the SQLite journal, model supervision, and the chat transcript store.
- `crates/lfm3-cli` is the `lfm3` binary with `doctor`, `missions`, `models`, `plan`, `run`, `status`, and `retry`.
- `apps/desktop` is the Tauri 2 shell. It embeds one engine, exposes typed commands and durable events to the webview, runs the multi-model chat turn loop and approval gates, owns all child processes through a kill-on-close Windows Job Object, and ships an NSIS installer.
- `apps/ui` is the React/TypeScript interface. It opens on chat, shows the decision route and local model roles, accepts image attachments and voice input, reads answers aloud with the local LFM3 voice, keeps verified missions one click away, and has no selectable demo bridge in the production bundle.
- The Python implementation, tests, caches, and Python project configuration have been removed.
- The local models, llama.cpp binaries, demo assets, and mission definitions have been preserved.
- [PLAN.md](PLAN.md) is the full Rust-only system specification.
- [STEPS.md](STEPS.md) is the ordered, test-driven implementation plan. Tasks 1–18 are complete; the chat surface is Tasks 19–22 and the multi-model workspace extension is Task 23. The packaged release acceptance is recorded in [`tests/golden/release-manifest.json`](tests/golden/release-manifest.json).
- Semantic holdout qualification has **not** been performed. Only `desk_task` has an end-to-end real-model acceptance; the three semantic missions remain `human_review_only`.

The desktop application performs no downloads at runtime. The Tauri CLI fetches its own NSIS packaging tooling at **build** time, never from the installed application.

## Quick launch

Windows:

```powershell
.\run.bat              # frontend + backend development stack
.\run.bat app          # built or installed desktop application
.\run.bat dev          # same as the default: Vite frontend + embedded Rust backend
.\run.bat cli doctor   # headless CLI (any CLI command works directly)
.\run.bat build        # build the NSIS installer
```

The default runs `tauri dev`, which starts the Vite frontend on `127.0.0.1:1420` and the Tauri shell with the embedded Rust engine. Use `run.bat app` when you want the already-built application rather than the development server. `run.bat` defaults `LFM3_LLAMA_SERVER` to `tools\llama\llama-server.exe` and `LFM3_MODELS_DIR` to `models` when the caller has not set them.

Linux:

```bash
./run.sh               # CLI health check and the exact next command
./run.sh build         # build the production CLI for Linux
./run.sh run --mission desk_task --input tests/fixtures/desk_task/basic.json --workspace ./workspace
```

`run.sh` runs the headless CLI only. The desktop shell is Windows-only because it owns children through a Windows Job Object and the bundled llama.cpp binaries are Windows x86_64. Linux requires a native llama.cpp 0.8.0 build 11960; set `LFM3_LLAMA_SERVER` to it. `LFM3_MODELS_DIR` defaults to `./models`. If the executable bit was lost in transfer, run `chmod +x run.sh` once.

## Headless usage

```powershell
cargo run -p lfm3-cli -- doctor
cargo run -p lfm3-cli -- missions
cargo run -p lfm3-cli -- models --probe

# Real pinned text model, durable events to stdout or a file.
cargo run -p lfm3-cli -- run --mission desk_task --input tests/fixtures/desk_task/basic.json --workspace tests/fixtures/desk_task/workspace --events -

# Test-only scripted model for the developer CLI; the installed desktop app does not include it.
cargo run -p lfm3-cli -- run --mission desk_task --fixture tests/fixtures/desk_task/basic.json --fake-model

cargo run -p lfm3-cli -- status --events
cargo run -p lfm3-cli -- retry --workspace tests/fixtures/desk_task/workspace
```

`run` exits 0 only when every frozen step check and the final output contract pass. Each report includes the acceptance hash, per-step attempts, check evidence, observed versus expected output digests, and the event journal count. `--events -` writes durable JSON lines to stdout and the human-readable report to stderr.

The `desk_task` fixture set under `tests/fixtures/desk_task/` covers a passing run, a recoverable first-write failure, a wrong-content run that must never pass, and adversarial fixtures for an unsatisfiable required term and an output path outside the workspace. Expected facts live in `tests/golden/desk_task/`.

## Agent loop

`lfm3 job` runs a stated job the way an interactive coding agent does. There is no frozen step list: the acting model keeps its own todo list, calls tools, reads the real output of each call, and chooses the next action from what it observed.

```powershell
cargo run -p lfm3-cli -- job --goal "Read notes/ and write out/report.md, one section per note" --workspace ./demo --minutes 30 --events -
```

The loop is bounded the same way the rest of the harness is:

- **The model cannot grade itself.** `finish` is accepted only when the required output exists and is non-empty; the report carries the measured bytes and digest. A bare "finished" is refused and the model is told to keep working.
- **Every turn is evidence.** Each tool call and its real output is recorded, and a repeated identical call is refused instead of quietly looping.
- **The harness steers, it does not just hope.** Unread sources are named in the prompt, and once they have all been read the tool set narrows to `write_file` so the only way forward is the artifact.
- **Writes are guarded.** An empty or placeholder write is refused, and a source file the job is supposed to read cannot be overwritten.
- **It stops honestly.** Status is `passed`, `unverified`, `turn_budget`, `deadline`, or `stalled`; six turns without a write, or a turn cap, is never reported as success.

`--minutes` is the whole-job deadline, `--max-turns` caps model turns, and `--max-calls` caps tool calls per turn.

## General tasks

`lfm3 task` is the plan-first mode: it turns a stated goal into a frozen, verified plan without a pre-authored mission. Every step is approved before it runs, which is the stricter path when you want to review the plan up front.

```powershell
cargo run -p lfm3-cli -- task --goal "Summarise demo/notes.md into out/summary.md" --workspace ./demo
cargo run -p lfm3-cli -- task --goal "..." --workspace ./demo --plan          # plan only
cargo run -p lfm3-cli -- task --goal "..." --deliverable out/result.md --events -
cargo run -p lfm3-cli -- task --goal "..." --minutes 30 --steps 16 --min-steps 3
```

The task router reads the goal and the workspace file names, then grants only the capabilities those models can serve:

| Role | Model | Routed when |
| --- | --- | --- |
| `plan`, `act` | LFM3-1.7B | every task |
| `observe` | LFM3-VL-2B | the workspace holds images, or the goal asks about one |
| `hear` | LFM3-Audio-2B | the workspace holds audio, or the goal asks for a transcript |

`look` and `transcribe` are granted exactly when their model is routed, and the CLI and desktop shell register whichever weights are provisioned; nothing is downloaded. The d1 classifier stays with the mission semantic checks, which general tasks cannot use: task verification is deterministic.

A job is multi-step by design. `--minutes` sets the whole-job deadline (default 30), `--steps` caps the plan (default 16), and `--min-steps` refuses a plan that collapses into one call (default 2). Each planned step is one verified workspace action, and every completed step's output is carried into the next step's prompt, so a later step can use what an earlier step actually read. The plan still names only real workspace files: the planner receives a bounded file listing, and an output path stated in the goal (for example `out/report.md`) is frozen as the required artifact.

Every step must declare at least one check, and a `write_file` or `edit_file` step whose checks name no file path is rejected before the acceptance is frozen, so an untestable step never runs. A read step is verified by the source it reads; when a plan emits only checks that cannot apply to a read, the harness keeps the runnable source-existence check instead of accepting a check that could never pass. General tasks have no command templates, so they cannot author a shell command. The run keeps the usual approval gate, budgets, retries, and journal.

The desktop job form is the loop, not the plan-first path. **Describe a job** asks for a goal, an optional required output, a minute budget, and a turn cap; the engine then runs the same model-driven loop as `lfm3 job` and streams every turn to the app. The panel below it shows the model's own todo list, each turn's tool calls with their real outputs, and the measured output bytes and digest — no acceptance hash, because a job is not verified mission work. Jobs live in `jobs.db` in the trusted data directory, events arrive on `lfm3:job-event`, and a job the engine stopped in the middle of is reported as `interrupted` rather than resumed. `lfm3 task` is still available for the stricter plan-first path, and verified missions keep their frozen acceptance flow.

## Desktop application

The desktop shell lives in `apps/desktop` and the React harness UI in `apps/ui`. The shell embeds the harness engine in-process: there is no sidecar, no loopback engine server, and no shell command surface. The webview talks to the engine only through the typed commands in `apps/desktop/src-tauri/src/commands.rs` and listens for durable journal events on `lfm3:run-event` (verified runs), `lfm3:job-event` (job turns), and `lfm3:chat-event` (chat).

Build and run in development:

```powershell
cd apps/desktop
npm install
npx tauri dev
```

Vite serves the UI on `127.0.0.1:1420`; the window is 1280×1000 with a 960×720 minimum. The cargo workspace must be on `PATH` for the Tauri CLI.

Package the installer:

```powershell
cd apps/desktop
npx tauri build --bundles nsis
```

Installer: `target/release/bundle/nsis/LFM3 Workspace_0.1.0_x64-setup.exe`. The bundle ships `missions/` as a resource, so an installed application never reads the source tree. The installer defaults to `%LOCALAPPDATA%\LFM3 Workspace`, a path containing a space.

### Chat and voice

The application opens on chat. Conversations are stored in `chat.db` inside the trusted data directory, so they survive restarts. The sidebar lists past conversations and links to **Verified tasks**; chat and mission runs share the engine, model manager, and process supervisor, but not their completion rules.

- **One bounded turn at a time.** A turn streams the model response, can use at most twelve tool rounds, and then returns control. Stop cancels the stream and any pending approval. A second send while a turn is active is refused with a visible error rather than queued.
- **Decision-model routing.** When `d1-2B` is installed, each non-trivial request is classified into a task shape (`general`, `explore`, `summarize`, `related`, `report`, `vision`, `voice`) before the text model runs. A calibrated keyword route is used when d1 is absent, unavailable, or below 55% confidence. The chosen intent, source, confidence, and the local model roles for the turn are shown in the transcript and carried into the system prompt. Routing selects capability hints only; it never grants authority or bypasses approval.
- **Workspace-confined tools.** The text model can call `list_files`, `read_file`, `search_files`, `related_files`, `write_file`, and `edit_file`. `search_files` matches names and text across the workspace without a shell; `related_files` compares one file's distinctive terms with the other text files. Paths outside the workspace are rejected by the same `ToolRegistry` used by verified missions. `look` is offered only when the vision model is installed, and `transcribe` only when the audio model is installed. Reads and searches execute immediately; writes and edits create a pending action.
- **Folder analysis and reports.** The system prompt requires the model to map a folder, open the files it summarises, and gather evidence before proposing a report. A report is a normal `write_file` action to a named Markdown path, so it appears as an exact diff and is written only after approval.
- **Image attachments.** The composer's **+img** control stores a user-chosen PNG, JPEG, WebP, or GIF under `.attachments/<uuid>-<name>.<ext>` inside the workspace (20 MB limit), then inserts a workspace-relative instruction. The model inspects it with `look`, which loads the image through the local vision model. This attachment is user-initiated, is not a model write, and never touches the verified run journal.
- **Explicit approval.** A write or edit appears inline with its path and exact content or diff. **Allow once** executes it; **Deny** leaves the file untouched and tells the model not to retry. Pending actions expire after ten minutes. Actions are persisted with the transcript.
- **Small-model resilience.** The runtime accepts native streamed tool calls and a fenced ```tool JSON block when the model cannot emit a native call. Both paths use the same approval gate, so the fallback cannot bypass consent.
- **Bounded context.** Each request carries a fixed system prompt, the newest 240 messages capped at about 18,000 characters, `temperature 0.3`, and at most 1,024 output tokens.
- **Local voice input.** The browser records with `MediaRecorder`, decodes and resamples the clip to 16 kHz mono WAV, and sends it as base64 to Rust. The local audio model receives an `input_audio` content part; no audio leaves the machine. The transcript is placed in the composer for review before you send it. Voice requires both the audio GGUF and its `mmproj`; otherwise the microphone path reports that the voice model is unavailable.
- **Local LFM3 voice output.** **Speak** and the header **Voice replies** toggle call `synthesize_speech`, which renders one answer with `llama-tts` using the same LFM3-Audio-2B weight and `mmproj` that listen. The health report exposes `voice_output.backend = "lfm3-tts"`, `model = "LFM3-Audio-2B"`, and `lfm3_tts_weights = true` when the binary and both files are present. The model card specifies the pinned b11960 runtime, `--tts-voice female|male`, and audio sampling at temperature 1.0, top-k 128, repetition penalty 1.1; this harness uses the documented defaults. A 120-second watchdog kills the owned `llama-tts` process, and the temporary WAV is removed after it is returned. Nothing is uploaded, and no voice is downloaded. If the voice pair is incomplete, Windows falls back to the offline system voice and labels it `system-sapi`.
- **Separation from verification.** Chat messages, tool output, and approvals never satisfy a mission check, and a verified run never writes through the chat approval path. Verified work still starts from the mission forms and is judged only by frozen acceptance checks.

The chat runtime has unit, reducer, and component coverage, but real-model routing, folder search, tool calling, streaming, vision attachments, and audio transcription have **not** been qualified end-to-end. Speech output is further along: `llama-tts` rendered 2.96 s of 24 kHz audio in 0.59 s, and the ignored runtime test exercises the same owned-process path and validates the returned WAV. The Tauri chat flow and in-app playback still need a measured run. If a model emits no valid tool call or an optional model is absent, the UI surfaces the failure instead of fabricating success. The demo fixture mode (`?demo=1` in development) renders the verified-task surface only; chat is intentionally unavailable there and says so.

### Local setup

Setup is read-only. At boot the engine locates an explicit path first, then the usual local locations for `llama-server.exe`, `llama-tts.exe`, the GGUF directory, and the vision projector. It never downloads or copies weights. Override discovery with:

| Variable | Purpose |
| --- | --- |
| `LFM3_LLAMA_SERVER` | pinned b11960 / v0.8.0 `llama-server.exe` |
| `LFM3_LLAMA_TTS` | local `llama-tts.exe` |
| `LFM3_MODELS_DIR` | directory containing the GGUF weights |
| `LFM3_DATA_DIR` | trusted harness state (default `%LOCALAPPDATA%\LFM3Harness`) |

or write `setup.json` into the data directory using absolute paths:

```json
{
  "llama_server": "F:\\lfm3\\tools\\llama\\llama-server.exe",
  "llama_tts": null,
  "models_dir": "F:\\lfm3\\models",
  "projector": null
}
```

Every supplied path must be absolute and present; a malformed file is reported and left untouched. The health report lists every missing item. The runtime must report build `11960` and release `0.8.0` to be accepted as pinned; the UI shows `verified pin` only in that case.

### Installed smoke acceptance

`scripts/smoke.ps1` installs the NSIS build silently into a path containing spaces and drives the packaged executable's embedded-engine probes:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/smoke.ps1
```

- `--smoke` starts the real `desk_task` through the local model, proves an overlapping start is refused, cancels the run, starts and cancels a second run, and releases the model.
- `--smoke-close` starts a run, closes the engine, and proves the next boot reports it `interrupted` with no active run.
- `--smoke-hold` holds a run open until the script force-terminates the process; the owned `llama-server` must die with the job.
- An externally started `llama-server` runs throughout and must survive every phase.
- The script also verifies the packaged missions resource, an install path with a space, and that the installed tree contains no Python source.

The probes never approve a plan, so no mission step executes. They require the local models and the pinned runtime and write JSON reports under `%TEMP%\LFM3 Smoke Reports`. Pass `-ReuseInstall` to skip reinstalling. Exit code 0 means every phase passed and no owned process survived.

### Semantic verification

Four check kinds consult a bounded local judge: `vl_criterion` (vision observation, then d1), `d1_noul` (supported / unsupported / ambiguous), `d1_choice` (a chosen label), and `d1_grounded` (each output field against an immutable input). Every result carries provenance: the evidence chain (`d1` or `vl->d1`), the model identity, the exact prompt and raw response, the label, the confidence in basis points, the frozen threshold, and the output pointers that were judged.

Semantic evidence can never manufacture a pass:

- A mission must declare a `semantic` policy. Packaged missions using semantic checks (`screen_qa`, `voice_intake`, `doc_extract`) are `human_review_only`: the engine refuses to start them and the UI shows the badge and disables Start with the stated reason.
- `automatic` passing requires a frozen holdout qualification: at least three packaged runs and zero confident false completions. No such qualification has been performed, so automatic passing is disabled everywhere.
- `d1_grounded` first requires each field value to occur literally in the frozen input bytes; a fabricated value fails before any model is consulted.
- An unavailable judge, an invalid protocol answer, low confidence, or an ambiguous verdict is `error`, which is never treated as a pass. A deterministic failure in the same step always wins.

`crates/lfm3-harness/tests/semantic.rs` exercises these gates with a scripted judge; `tests/fixtures/semantic/` holds the source transcript, grounded and fabricated outputs, a screenshot, and the frozen holdout manifest.

### Release acceptance

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/accept.ps1
```

The script runs, in order: `cargo fmt --check`, clippy with warnings denied, the full Rust workspace tests, a check that no Python project files remain, UI typecheck and unit tests, Playwright end-to-end tests, the Tauri NSIS build, and the installed smoke acceptance. Use `-SkipUi`, `-SkipE2e`, `-SkipBuild`, or `-SkipSmoke` for partial runs, and pass `-ModelsDir`, `-LlamaServer`, or `-InstallDir` when discovery is not enough.

The script ends by stating that semantic qualification was **not** performed: all packaged semantic missions remain `human_review_only` and automatic semantic passing stays disabled.

The full run passed every gate in 170.0 s on the release machine. A final static/UI rerun with `-SkipBuild -SkipSmoke` passed in 29.2 s: fmt 0.3 s, clippy 0.5 s, workspace tests 11.7 s, no-Python check 4.1 s, UI typecheck 1.4 s, 34 UI unit tests 1.9 s, and 8 Playwright tests 9.2 s. The installer was deliberately not rebuilt after recording its hash.

The chat-first and multi-model updates add reducer, message-bubble, router, search, and prompt tests (57 UI unit tests in total). The static gates were rerun after the changes; the installer hash in the manifest still describes the pre-chat build, so rebuild the installer before publishing.

### Verification after the multi-model chat update

Run from the repository root unless noted:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/ui; npm run typecheck; npm test; npm run build; npx playwright test
```

Observed on 2026-10-31: formatting and clippy clean; all workspace Rust tests passed, including 41 harness library tests, 18 desktop tests, and 14 tool integration tests with `search_files` coverage; UI typecheck and production build passed; 57 Vitest tests passed (including decision-route reducer and message-bubble coverage). The eight Playwright demo scenarios were previously green and target only the verified-task surface. The ignored `lfm3_voice_supervisor_renders_a_valid_wav` test also passed against the installed pair through the real owned-process path. Real-model chat routing, folder search, report writing, vision, audio, and UI playback of the LFM3 voice remain open.

### Packaged real-model acceptance

`scripts/release-real-model.ps1` drives the optimized CLI against the pinned models and writes one report, event journal, stderr capture, and result JSON per case, plus `summary.json`, into `tests/golden/release/`:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release-real-model.ps1
```

| Case | Result |
| --- | --- |
| `success` | exit 0, `passed`, 1 attempt, 5.99 s, 1,603,924 KB peak |
| `failure` | exit 1, `incomplete` / `no_progress`, 4 attempts, 15.49 s, no false pass |
| `escape` | exit 1 before planning, no file outside the workspace, 0.25 s |
| `retry` | exit 0, `passed` on attempt 2 after a denied first write, 8.65 s |
| `crash` | owned model killed after `attempt.started`; exit 1, `incomplete` / `no_progress`, no false pass, 10.94 s |
| `unavailable` | closed loopback endpoint; exit 1, no owned server, 2.27 s |
| `timeout` | one-second deadline; exit 1, owned server terminated, 2.46 s |
| `missing` | empty models directory; exit 1, no download, no server, 0.25 s |

Every case ended with no leftover PIDs and no surviving owned server. `step_replanned` was false in all eight cases: real-model replanning was not exercised here and remains covered only by the automated recovery and executor tests.

Reproducibility evidence lives in [`tests/golden/release-manifest.json`](tests/golden/release-manifest.json): installer and Cargo.lock hashes, rustc/cargo versions, the llama.cpp build and commit, all seven model hashes, mission and fixture hashes, acceptance hashes, sampling and recovery budgets, each case's wall time, peak memory, report/event hashes, and the scope audit.

## Real-model release record

The real pinned text model passes the frozen `desk_task` acceptance contract. The release acceptance run below is the Task 18 measurement; the original Task 11 run completed the same contract in 1.86 s with a 1,603,404 KB peak.

| Field | Value |
| --- | --- |
| Model | `LFM3-1.7B-Q4_K_M.gguf` |
| Model SHA-256 | `3793e1e6e2266919d6e015287221b41100175a98eddaba92e111aed3b05c93c4` |
| llama.cpp | b11960 / v0.8.0-dev, commit `8642541b2`, `tools/llama/llama-server.exe` |
| Acceptance SHA-256 | `f7ed5f17bda3c957797569028aab3e375b42998cbf65d489bdc7a9e8e7b6aba5` |
| Steps / attempts | 1 step, 1 attempt, all checks pass |
| Wall time | 5.99 s whole CLI case, including model startup |
| Peak llama-server memory | 1,603,924 KB (about 1.53 GiB, sampled every 200 ms) |
| Output | `out/summary.md`, 222 bytes, SHA-256 `90b13d095feff174e625031a6859b6c03180a34d0b1f3f9f4ceb944e140c597d` |
| Evidence | `tests/golden/release/success.report.json`, `tests/golden/release/success.events.jsonl`, `tests/golden/release-manifest.json` |

The real summary states the required facts but is not byte-identical to the fake-mode golden summary. Acceptance is decided by the frozen term and absence checks over provenance-bound file digests, not by prose equality; `matches_expected` in the report is an informational comparison against the scripted summary.

Adversarial gates exercised with the real model and the CLI:

- Wrong content and no progress: a fixture requiring a term the source cannot support ends `incomplete` with reason `no_progress` after 4 failed attempts, exit code 1, no expected output digest match, and no model process left.
- Impossible output scope: an output path of `../escaped.md` is rejected before planning; no file is created outside the workspace.
- Model unavailable: `--external` against an empty loopback endpoint fails immediately with exit code 1 and leaves no owned process.
- Cancellation during generation: a one-second whole-run deadline cancels planning, exits 1, and terminates the owned llama-server.
- Owned-model crash: force-killing the owned llama-server after `attempt.started` ends the run `incomplete` with `no_progress`; it never passes and nothing survives.
- Transient write failure: denying file creation in `out/` makes the first write fail; lifting the rule on `step.failed` lets attempt 2 pass with the same frozen acceptance.
- Duplicate action: the fake wrong-content case repeats the identical write, stops at the attempt budget with `no_progress`, and never passes.
- Engine interruption: killing the engine tree mid-attempt leaves the run `interrupted`; the next run reconciles one run, one step, and one attempt, does not resume it, and then passes normally.

## Architecture

```text
React + TypeScript UI
  chat · attachments · voice · verified tasks
        │ typed Tauri commands and events
        ▼
Tauri 2 desktop application
  chat runtime · routing · approval gates · durable events
        │ embeds
        ▼
Rust lfm3-harness engine
  chat transcript store · decision router · search/related tools
  planning · execution · verification · retries · model leases
  SQLite run journal and chat.db · artifacts
        │ owns child processes
        ▼
llama.cpp b11960 / v0.8.0
  LFM3-1.7B · LFM3-VL-2B · LFM3-Audio-2B · d1-2B
  llama-server for chat, vision, hearing, and routing
  llama-tts for LFM3-Audio-2B speech output
```

Production uses no Python sidecar and no localhost engine HTTP server. The headless `lfm3-cli` binary uses the same Rust harness for diagnostics and real-model acceptance.

## Preserved assets

| Path | Purpose |
| --- | --- |
| `missions/` | Mission JSON definitions preserved before Python removal |
| `models/` | Local GGUF weights and projectors |
| `tools/` | Existing local tooling and llama.cpp binaries |
| `demo/` | Notes, screenshots, and demonstration inputs |

The inference backend remains **llama.cpp b11960 / v0.8.0**. Using Rust does not require changing llama.cpp, and an upgrade requires a separate compatibility qualification across all four models.

## Product constraints

- Chat is conversation-scoped. No chat message, tool result, or approval can satisfy a mission check.
- No file write or edit from chat without an explicit per-action user approval.
- No remote speech recognition, audio upload, or cloud transcription; voice uses the local audio model.
- Spoken replies use the local LFM3-Audio-2B voice through `llama-tts`; no audio is uploaded. The operating-system voice is only a fallback when the voice pair is incomplete.
- Image attachments are user-initiated, stay inside the workspace, and are inspected only by the local vision model.
- A decision route selects capability hints, never authority; it cannot grant a tool, weaken a check, or skip approval.
- No arbitrary planner-authored shell.
- No browsing, cloud fallback, telemetry, or hidden model downloads.
- No pass without provenance-bound evidence.
- No acceptance-criterion weakening during retry or replanning.
- No concurrent workspace-mutating runs.
- No silent resume after interruption.
- No owned model or tool process left behind after shutdown.

## Stack

- Rust stable with the MSVC toolchain.
- Tokio, Serde, Reqwest, Rusqlite, SHA-256, and typed local workers.
- Tauri 2 for the native desktop boundary and Windows Job Object supervision.
- React 19, TypeScript 5.9, and Vite 8 for the UI; Vitest and Playwright for UI acceptance.
- External llama.cpp for inference; no Rust FFI and no llama.cpp version change.

## Repository status details

Legacy Python runtime data under `.lfm3/`, if present, is not used by the Rust application and is not automatically migrated. New trusted state lives under `%LOCALAPPDATA%\LFM3Harness`.

The first release ships the chat surface plus four verified missions. The verified path is accepted end-to-end for `desk_task` with the real LFM3 text model. `screen_qa`, `voice_intake`, and `doc_extract` remain `human_review_only` until a frozen semantic holdout is evaluated with the real models. Chat is available but is not part of that acceptance record.

The release scope audit found no Python source, project files, caches, or launch commands; no remote model endpoints, shell plugin, broad filesystem capability, hidden download path, or unguarded scripted fallback. The installed tree contains only `lfm3-desktop.exe`, `uninstall.exe`, and the four mission files, whose hashes match the source. The production UI bundle contains no `DemoBridge` symbol.

Known gaps for the next task: qualify decision routing, folder search, related-file discovery, report writing, native streamed tool calls, the fenced-call fallback, cancellation, and approval expiry with the real text and decision models; qualify vision attachments with the real VL model and projector; qualify voice transcription with the real audio model and `mmproj`; exercise the LFM3 speech-output path end-to-end in the desktop app (llama-tts itself was verified directly on this machine); perform the semantic holdout qualification and decide whether to enable automatic semantic passing; exercise replanning with the real model (it is verified only by automated tests today); and either wire `limits.max_tokens` into executor/planner requests or remove it from the configuration surface, since the executor currently uses its built-in 512-token cap and the planner a fixed 1536.
