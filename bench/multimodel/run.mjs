// Real multi-model benchmark.
//
// The probe (`lfm3-desktop --benchmark-chat`) boots the production engine and
// drives the real d1 router, text conversation loop, tool calls, approval gate,
// vision model, and audio model. This script prepares an isolated workspace,
// runs the probe, grades every case independently, and writes results.json and
// REPORT.md.
//
// Usage: node bench/multimodel/run.mjs [--skip-build] [--keep]
//        node bench/multimodel/run.mjs --regrade
//
// `--regrade` re-applies the current checks to the recorded raw results and
// kept workspace without spending another model turn.
//
// The probe needs ports 8080-8083 free so its own model manager can spawn the
// pinned llama-server processes. The runner refuses to start when a port is
// already serving, rather than killing a process it does not own.

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { chat as chatCases, audio as audioCases, router as routerCases } from "./cases.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../..");
const corpus = path.join(root, "bench", "agent50", "corpus");
const runRoot = path.join(root, "tmp", "bench-multimodel");
const workspace = path.join(runRoot, "workspace");
const dataDir = path.join(runRoot, "data");
const casesFile = path.join(runRoot, "cases.json");
const rawFile = path.join(runRoot, "raw-results.json");
const logFile = path.join(runRoot, "probe.log");
const resultsFile = path.join(here, "results.json");
const reportFile = path.join(here, "REPORT.md");

const BINARY = path.join(
  root,
  "target",
  "debug",
  process.platform === "win32" ? "lfm3-desktop.exe" : "lfm3-desktop",
);
const MODELS_DIR = path.join(root, "models");
const LLAMA_DIR = path.join(root, "tools", "llama");
const MISSIONS_DIR = path.join(root, "missions");
const PROBE_TIMEOUT_MS = 45 * 60 * 1000;
const PORTS = [8080, 8081, 8082, 8083];

const args = process.argv.slice(2);
const skipBuild = args.includes("--skip-build");
const keep = args.includes("--keep");

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function copyTree(from, to) {
  fs.mkdirSync(to, { recursive: true });
  for (const entry of fs.readdirSync(from, { withFileTypes: true })) {
    const source = path.join(from, entry.name);
    const target = path.join(to, entry.name);
    if (entry.isDirectory()) copyTree(source, target);
    else fs.copyFileSync(source, target);
  }
}

async function portServing(port) {
  try {
    const response = await fetch(`http://127.0.0.1:${port}/v1/models`, {
      signal: AbortSignal.timeout(800),
    });
    return response.ok;
  } catch {
    return false;
  }
}

async function assertPortsFree() {
  const busy = [];
  for (const port of PORTS) if (await portServing(port)) busy.push(port);
  if (busy.length) {
    throw new Error(
      `ports ${busy.join(", ")} are already serving; stop the other engine first so this benchmark owns its models`,
    );
  }
}

function ensureBuilt() {
  if (skipBuild && fs.existsSync(BINARY)) return;
  const rustup = path.join(process.env.USERPROFILE ?? "", ".cargo", "bin", process.platform === "win32" ? "rustup.exe" : "rustup");
  const result = spawnSync(rustup, ["run", "stable", "cargo", "build", "-p", "lfm3-desktop"], {
    cwd: root,
    stdio: "inherit",
  });
  if (result.status !== 0) throw new Error("cargo build -p lfm3-desktop failed");
  if (!fs.existsSync(BINARY)) throw new Error(`missing built binary: ${BINARY}`);
}

function prepare() {
  fs.rmSync(runRoot, { recursive: true, force: true });
  fs.mkdirSync(runRoot, { recursive: true });
  copyTree(corpus, workspace);
  fs.mkdirSync(path.join(workspace, "out"), { recursive: true });
  fs.mkdirSync(path.join(workspace, "assets"), { recursive: true });
  fs.mkdirSync(dataDir, { recursive: true });

  const toAbsolute = (image) => (image ? path.join(here, image) : undefined);
  const cases = {
    router: routerCases.map(({ expected, ...rest }) => rest),
    audio: audioCases.map(({ expect, ...rest }) => rest),
    chat: chatCases.map(({ expect, ...rest }) => ({
      ...rest,
      ...(rest.image ? { image: toAbsolute(rest.image) } : {}),
    })),
  };
  fs.writeFileSync(casesFile, JSON.stringify(cases, null, 2));
}

async function runProbe() {
  const log = fs.openSync(logFile, "w");
  const child = spawn(
    BINARY,
    ["--benchmark-chat", casesFile, rawFile],
    {
      cwd: root,
      env: {
        ...process.env,
        LFM3_WORKSPACE: workspace,
        LFM3_DATA_DIR: dataDir,
        LFM3_MODELS_DIR: MODELS_DIR,
        LFM3_LLAMA_SERVER: path.join(LLAMA_DIR, "llama-server.exe"),
        LFM3_LLAMA_TTS: path.join(LLAMA_DIR, "llama-tts.exe"),
        LFM3_MISSIONS_DIR: MISSIONS_DIR,
        LFM3_ALLOW_DOWNLOAD: "0",
        LFM3_REUSE_EXTERNAL_SERVERS: "0",
        RUST_LOG: "info",
      },
      stdio: ["ignore", log, log],
      windowsHide: true,
    },
  );
  const outcome = await new Promise((resolve) => {
    const timer = setTimeout(() => {
      resolve({ code: "timeout" });
    }, PROBE_TIMEOUT_MS);
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolve({ code });
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      resolve({ code: "error", error });
    });
  });
  await sleep(1500);
  if (outcome.code !== 0) {
    const tail = fs.existsSync(logFile) ? fs.readFileSync(logFile, "utf8").slice(-4000) : "";
    throw new Error(`probe exited with ${outcome.code}\n${tail}`);
  }
  if (!fs.existsSync(rawFile)) throw new Error("probe produced no results file");
  return JSON.parse(fs.readFileSync(rawFile, "utf8"));
}

// Spoken numbers and digits are the same content; normalise both directions so
// "Thursday at 3" and "Thursday at three" compare equal.
const NUMBER_WORDS = new Map([
  ["zero", "0"],
  ["one", "1"],
  ["two", "2"],
  ["three", "3"],
  ["four", "4"],
  ["five", "5"],
  ["six", "6"],
  ["seven", "7"],
  ["eight", "8"],
  ["nine", "9"],
  ["ten", "10"],
]);

function canonicalWord(word) {
  return NUMBER_WORDS.get(word.toLowerCase()) ?? word.toLowerCase();
}

function normalizeWords(text) {
  return text
    .toLowerCase()
    .replace(/[^a-z0-9'\s]/g, " ")
    .split(/\s+/)
    .filter(Boolean)
    .map(canonicalWord);
}

function wordErrorRate(expected, actual) {
  const reference = normalizeWords(expected);
  const hypothesis = normalizeWords(actual);
  const rows = reference.length + 1;
  const columns = hypothesis.length + 1;
  const table = Array.from({ length: rows }, (_, row) =>
    Array.from({ length: columns }, (_, column) => (row === 0 ? column : column === 0 ? row : 0)),
  );
  for (let row = 1; row < rows; row += 1) {
    for (let column = 1; column < columns; column += 1) {
      const cost = reference[row - 1] === hypothesis[column - 1] ? 0 : 1;
      table[row][column] = Math.min(
        table[row - 1][column] + 1,
        table[row][column - 1] + 1,
        table[row - 1][column - 1] + cost,
      );
    }
  }
  return reference.length ? table[rows - 1][columns - 1] / reference.length : 0;
}

function matchesAll(text, patterns) {
  return (patterns ?? []).every((pattern) => pattern.test(text));
}

function matchesAny(text, patterns) {
  return (patterns ?? []).some((pattern) => pattern.test(text));
}

function toolNames(record) {
  return (record.tool_messages ?? [])
    .map((message) => message?.meta?.tool)
    .filter(Boolean);
}

function gradeRouter(raw) {
  const records = raw.router ?? [];
  const rows = records.map((record) => {
    const expected = routerCases.find((item) => item.id === record.id)?.expected;
    const heuristic = record.heuristic_intent;
    const final = record.route?.intent;
    const consulted = [...record.prompt.trim()].length >= 24;
    return {
      id: record.id,
      expected,
      heuristic,
      final,
      source: record.route?.source,
      confidence_bps: record.route?.confidence_bps ?? 0,
      milliseconds: record.milliseconds,
      consulted,
      final_correct: final === expected,
      heuristic_correct: heuristic === expected,
      override: consulted && heuristic !== expected && final === expected,
      regression: heuristic === expected && final !== expected,
      blind_spot: expected !== "general" && heuristic === "general",
      roles: record.route?.roles ?? [],
      note: record.note,
    };
  });
  const count = (predicate) => rows.filter(predicate).length;
  const summary = {
    cases: rows.length,
    final_correct: count((row) => row.final_correct),
    heuristic_correct: count((row) => row.heuristic_correct),
    decision_consulted: count((row) => row.consulted),
    decision_used: count((row) => row.source === "decision"),
    overrides_attempted: count((row) => row.consulted && row.heuristic !== row.expected),
    overrides_won: count((row) => row.override),
    regressions: count((row) => row.regression),
    blind_spots: count((row) => row.blind_spot),
    blind_spots_fixed: count((row) => row.blind_spot && row.final_correct),
    median_ms: rows.length
      ? [...rows].map((row) => row.milliseconds).sort((a, b) => a - b)[Math.floor(rows.length / 2)]
      : 0,
  };
  summary.final_accuracy = rows.length ? summary.final_correct / rows.length : 0;
  summary.eligible = rows.filter((row) => row.consulted).length;
  summary.eligible_accuracy = summary.eligible
    ? rows.filter((row) => row.consulted && row.final_correct).length / summary.eligible
    : 0;
  summary.heuristic_accuracy = rows.length ? summary.heuristic_correct / rows.length : 0;
  return { rows, summary };
}

function gradeAudio(raw) {
  return (raw.audio ?? []).map((record) => {
    const expected = audioCases.find((item) => item.id === record.id);
    const transcript = record.transcript ?? "";
    const wer = expected ? wordErrorRate(expected.text, transcript) : null;
    const spoken = normalizeWords(transcript);
    const keywords = (expected?.expect ?? []).map((keyword) => ({
      keyword,
      found: spoken.includes(canonicalWord(keyword)),
    }));
    return {
      id: record.id,
      text: expected?.text,
      transcript,
      wav: record.wav,
      backend: record.backend,
      note: record.note,
      wav_bytes: record.wav_bytes,
      wav_header: record.wav_header,
      synthesis_ms: record.synthesis_ms,
      transcription_ms: record.transcription_ms,
      wer,
      keywords,
      pass:
        Boolean(transcript.trim()) &&
        record.wav_header === true &&
        keywords.every((item) => item.found) &&
        !record.error &&
        !record.transcription_error,
      error: record.error ?? record.transcription_error,
    };
  });
}

function gradeChat(raw) {
  return (raw.chat ?? []).map((record) => {
    const spec = chatCases.find((item) => item.id === record.id)?.expect ?? {};
    const reply = record.final ?? "";
    const tools = toolNames(record);
    const actions = record.actions ?? [];
    const failures = [];

    if (!record.sent) failures.push("turn was not sent");
    if (record.timed_out) failures.push("turn timed out");
    if ((record.errors ?? []).length) failures.push(`errors: ${record.errors.join("; ")}`);
    if (record.unscripted_action) failures.push("unscripted approval request");

    // Always report the observed route so the table shows which model actually
    // answered, not only for the cases that assert a specific route.
    const route = record.route?.intent ?? null;
    if (spec.route) {
      if (route !== spec.route) failures.push(`route ${route} != ${spec.route}`);
      const roles = record.route?.roles ?? [];
      if (spec.route === "vision" && roles.find((role) => role.role === "observe")?.used !== true) {
        failures.push("route did not mark the vision role used");
      }
      if (spec.route === "voice" && roles.find((role) => role.role === "hear")?.used !== true) {
        failures.push("route did not mark the audio role used");
      }
      if (roles.find((role) => role.role === "answer")?.used !== true) {
        failures.push("route did not mark the text role used");
      }
    }

    if (!matchesAll(reply, spec.contains)) {
      failures.push("reply misses required content");
    }
    if (matchesAny(reply, spec.absent)) failures.push("reply contains forbidden content");

    if (spec.tools === "none") {
      if (tools.length) failures.push(`unexpected tools: ${tools.join(", ")}`);
    } else if (Array.isArray(spec.tools)) {
      for (const expected of spec.tools) {
        if (!tools.includes(expected)) failures.push(`missing tool ${expected}`);
      }
    }

    if (spec.approval === "none") {
      if (actions.length) failures.push("unexpected approval request");
    } else if (spec.approval === "allowed" || spec.approval === "denied") {
      const requested = actions.find((action) => action.requested);
      if (!requested) failures.push("no approval was requested");
      else {
        const wantAllow = spec.approval === "allowed";
        if (requested.allow !== wantAllow) failures.push(`approval sent allow=${requested.allow}`);
        const resolved = actions.find((action) => action.resolved);
        const status = resolved?.resolved?.status;
        if (!status) failures.push("approval was never resolved");
        else if (wantAllow && !/approv|allow|applied|complete|done/i.test(status)) {
          failures.push(`approval status ${status}`);
        } else if (!wantAllow && !/den|reject|block/i.test(status)) {
          failures.push(`denial status ${status}`);
        }
      }
    }

    if (spec.file) {
      const target = path.join(workspace, spec.file.path);
      if (!fs.existsSync(target)) failures.push(`missing file ${spec.file.path}`);
      else if (!matchesAll(fs.readFileSync(target, "utf8"), spec.file.contains)) {
        failures.push(`file ${spec.file.path} misses required content`);
      }
    }
    if (spec.fileAbsent && fs.existsSync(path.join(workspace, spec.fileAbsent))) {
      failures.push(`forbidden file ${spec.fileAbsent} exists`);
    }

    const toolText = (record.tool_messages ?? []).map((message) => message?.content ?? "").join("\n");
    const toolCalls = (record.assistant_messages ?? []).flatMap(
      (message) => message?.meta?.tool_calls ?? [],
    );
    const parseArguments = (call) => {
      try {
        return JSON.parse(call?.function?.arguments ?? "");
      } catch {
        return null;
      }
    };
    if (spec.tools?.includes("look")) {
      if (!/observation \(LFM3-VL-2B\)/.test(toolText)) {
        failures.push("vision observation missing from tool output");
      }
      const call = toolCalls.find((item) => item?.function?.name === "look");
      if (!call) failures.push("no look tool call");
      else {
        const lookArgs = parseArguments(call);
        if (!lookArgs?.instruction || lookArgs.instruction.length < 10) {
          failures.push("look call carried no real instruction");
        }
        const basename = (value) => String(value ?? "").split(/[\\/]/).pop();
        if (
          record.attachment?.path &&
          basename(lookArgs?.path) !== basename(record.attachment.path)
        ) {
          failures.push(`look inspected ${lookArgs?.path}, not the attachment`);
        }
      }
    }
    if (spec.tools?.includes("transcribe")) {
      if (!/transcript \(LFM3-Audio-2B/.test(toolText)) {
        failures.push("audio transcript missing from tool output");
      }
      const call = toolCalls.find((item) => item?.function?.name === "transcribe");
      if (!call) failures.push("no transcribe tool call");
      else if (!/assets[\\/]audio-[^"\\]+\.wav/i.test(parseArguments(call)?.path ?? "")) {
        failures.push("transcribe call did not target the synthesized clip");
      }
    }

    return {
      id: record.id,
      prompt: record.prompt_sent ?? record.prompt,
      route,
      source: record.route?.source ?? null,
      confidence_bps: record.route?.confidence_bps ?? null,
      roles: record.route?.roles ?? [],
      tools,
      actions: actions.length,
      approve: record.approve ?? null,
      milliseconds: record.milliseconds,
      deltas: record.deltas,
      reply,
      failures,
      pass: failures.length === 0,
    };
  });
}

function modelDelivery(chat, router) {
  const vision = chat.filter((record) => record.tools.includes("look"));
  const voice = chat.filter((record) => record.tools.includes("transcribe"));
  const text = chat.filter((record) => (record.reply ?? "").trim().length > 0);
  return {
    text: { cases: text.length, pass: text.filter((record) => record.pass).length },
    vision: { cases: vision.length, pass: vision.filter((record) => record.pass).length },
    audio: { cases: voice.length, pass: voice.filter((record) => record.pass).length },
    d1_decisions: router.summary.decision_used,
    d1_consulted: router.summary.decision_consulted,
  };
}

function percent(value) {
  return `${(value * 100).toFixed(1)}%`;
}

function buildReport(raw, router, audio, chat) {
  const delivery = modelDelivery(chat, router);
  const chatPass = chat.filter((record) => record.pass).length;
  const audioPass = audio.filter((record) => record.pass).length;

  const routerRows = router.rows
    .map(
      (row) =>
        `| ${row.id} | ${row.expected} | ${row.heuristic} | ${row.final} (${row.source ?? "-"}) | ${row.confidence_bps} | ${row.final_correct ? "pass" : "FAIL"} |`,
    )
    .join("\n");
  const chatRows = chat
    .map(
      (record) =>
        `| ${record.id} | ${record.route ?? "-"} (${record.source ?? "-"}) | ${record.tools.join(", ") || "-"} | ${record.actions} | ${(record.milliseconds / 1000).toFixed(1)}s | ${record.pass ? "pass" : "FAIL"} |`,
    )
    .join("\n");
  const audioRows = audio
    .map(
      (record) =>
        `| ${record.id} | ${record.backend ?? "-"} | ${(record.wer * 100).toFixed(0)}% | ${record.keywords.map((item) => `${item.keyword}:${item.found ? "yes" : "no"}`).join(" ")} | ${record.pass ? "pass" : "FAIL"} |`,
    )
    .join("\n");

  return `# Multi-model benchmark — measured results

Probe: \`lfm3-desktop --benchmark-chat\` (production engine, no webview).
Workspace: \`${workspace.replaceAll("\\", "/")}\`.
Every number below comes from one real run on this machine; nothing is scripted
or replayed.

## Verdict

| Area | Result |
| --- | --- |
| Router final accuracy | ${router.summary.final_correct}/${router.summary.cases} (${percent(router.summary.final_accuracy)}) |
| Router accuracy where d1 is consulted | ${router.summary.eligible ? Math.round(router.summary.eligible_accuracy * router.summary.eligible) : 0}/${router.summary.eligible} (${percent(router.summary.eligible_accuracy)}) |
| Keyword-only accuracy (same cases) | ${router.summary.heuristic_correct}/${router.summary.cases} (${percent(router.summary.heuristic_accuracy)}) |
| d1 actually consulted | ${router.summary.decision_consulted}/${router.summary.cases} |
| d1 overrides attempted / won | ${router.summary.overrides_attempted} / ${router.summary.overrides_won} |
| d1 regressions against the keyword route | ${router.summary.regressions} |
| Keyword blind spots (no group fired) | ${router.summary.blind_spots} (${router.summary.blind_spots_fixed} routed correctly) |
| Chat turns | ${chatPass}/${chat.length} passed |
| Vision role | ${delivery.vision.pass}/${delivery.vision.cases} passed |
| Audio role | ${delivery.audio.pass}/${delivery.audio.cases} passed through the chat tool |
| Audio synthesis + ASR | ${audioPass}/${audio.length} passed |
| Text conversation | ${delivery.text.pass}/${delivery.text.cases} graded turns passed |

## Decision-model routing

Heuristic and final routes are both recorded for every case. Every message of
at least 24 characters is shown to d1; only tiny greetings skip the model. A
confident non-general choice replaces the keyword route, while a confident
\`general\` answer leaves conversation alone. A \`heuristic\` final source in the
table below therefore means d1 agreed with the keyword reading, not that d1 was
skipped: all router prompts are longer than 24 characters. Keyword blind spots
are cases where no keyword group fires, so only d1 can recover the route; a
decision override is the expected outcome.

| Case | Expected | Heuristic | Final (source) | bps | Outcome |
| --- | --- | --- | --- | --- | --- |
${routerRows}

- Median route latency: ${router.summary.median_ms} ms, prompt to final route.
- d1 was consulted for ${router.summary.decision_consulted} of ${router.summary.cases} messages; its choice replaced the keyword route in ${router.summary.decision_used} and agreed with it in ${router.summary.decision_consulted - router.summary.decision_used}. Every router prompt here is at least 24 characters, so none was skipped.
- Blind spots are cases where no keyword group fires, so d1 is the only signal
  that can recover the route. They are counted as final failures when the
  keyword guess is wrong and d1 does not correct it.

## Chat turns, tools, and approvals

| Case | Route (source) | Tools | Approvals | Time | Result |
| --- | --- | --- | --- | --- | --- |
${chatRows}

Failures:

${chat.filter((record) => !record.pass).map((record) => `- **${record.id}**: ${record.failures.join("; ")}`).join("\n") || "- none"}

A pass is only granted after route, tool, approval, file, and content checks
all succeed; the failures column above is exhaustive.

## Vision and audio delivery

- Vision turns used the \`look\` tool and the tool output contained an
  \`observation (LFM3-VL-2B)\` block in ${delivery.vision.pass}/${delivery.vision.cases} cases.
- Voice turns used the \`transcribe\` tool and the tool output contained a
  \`transcript (LFM3-Audio-2B...)\` block in ${delivery.audio.pass}/${delivery.audio.cases} cases.
- Vision fixtures are deterministic PNGs generated by \`bench/multimodel/assets.mjs\`
  (the number \`42\` and a red circle beside a blue square).

## Audio synthesis and transcription

Each case is spoken locally by LFM3-Audio-2B (\`llama-tts\`), then transcribed by
the same weight through the chat runtime. WER is computed against the spoken
sentence after normalisation.

| Case | Backend | WER | Keywords | Result |
| --- | --- | --- | --- | --- |
${audioRows}

${audio
  .map(
    (record) =>
      `- **${record.id}** spoken: "${record.text}"\ntranscript: "${record.transcript || record.error || record.transcription_error || ""}"`,
  )
  .join("\n")}

## Scope and caveats

- This run measures one local machine and one pinned runtime; it does not
  qualify other hardware.
- The router's keyword gate is part of the measured system. Blind-spot cases
  show its cost in accuracy; they are not hidden from the score.
- A chat "pass" requires the production route, the expected tool, the approval
  outcome, and the graded content. Nothing is graded from the probe's own
  claims.
- Audio WER is text-level; it does not measure diarisation or punctuation.
- No data leaves the machine; the probe only calls loopback ports.
`;
}

function gradeAndWrite(raw) {
  const router = gradeRouter(raw);
  const audio = gradeAudio(raw);
  const chat = gradeChat(raw);
  const report = {
    generated_at: new Date().toISOString(),
    health: raw.health,
    models: raw.models,
    router_available: raw.router_available,
    router: { summary: router.summary, cases: router.rows },
    audio,
    chat,
    delivery: modelDelivery(chat, router),
    workspace,
  };
  fs.writeFileSync(resultsFile, JSON.stringify(report, null, 2));
  fs.writeFileSync(reportFile, buildReport(raw, router, audio, chat));
  console.log(
    `router ${router.summary.final_correct}/${router.summary.cases}, ` +
      `chat ${chat.filter((record) => record.pass).length}/${chat.length}, ` +
      `audio ${audio.filter((record) => record.pass).length}/${audio.length}`,
  );
}

async function main() {
  if (args.includes("--regrade")) {
    if (!fs.existsSync(rawFile)) throw new Error(`no recorded results at ${rawFile}`);
    console.log("regrading recorded results (no model turns)");
    gradeAndWrite(JSON.parse(fs.readFileSync(rawFile, "utf8")));
    return;
  }
  console.log("preparing isolated workspace and cases");
  prepare();
  ensureBuilt();
  await assertPortsFree();
  console.log("running the production engine probe (d1, text, vision, audio)");
  const raw = await runProbe();
  console.log("grading");
  gradeAndWrite(raw);
  if (!keep) console.log("workspace kept for inspection under tmp/bench-multimodel");
}

main().catch((error) => {
  console.error(error.message ?? error);
  process.exit(1);
});
