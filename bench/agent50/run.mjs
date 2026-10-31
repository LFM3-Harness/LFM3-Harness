// Runs the 50-task benchmark through the real LFM3 text model.
//
// The harness decides pass/fail on its own terms (deliverable exists,
// non-empty, measured, re-read). The checks in tasks.mjs are external and
// deterministic; they measure content quality, never harness acceptance.
//
// Usage: node bench/agent50/run.mjs [--only t01,t02] [--limit N]
//
// The script reuses a loopback llama-server on port 8080 when it already
// serves lfm3-1.7b; otherwise it starts the pinned binary and stops it on exit.

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { tasks } from "./tasks.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../..");
const corpus = path.join(here, "corpus");
const runsDir = path.join(root, "tmp", "bench-agent50", "runs");
const eventsRoot = path.join(root, "tmp", "bench-agent50");
const resultsFile = path.join(here, "results.json");
const reportFile = path.join(here, "REPORT.md");
const ndjsonFile = path.join(eventsRoot, "results.ndjson");
const serverLog = path.join(eventsRoot, "llama-server.log");

const CLI = path.join(root, "target", "debug", process.platform === "win32" ? "lfm3-cli.exe" : "lfm3-cli");
const MODEL = "lfm3-1.7b";
const URL = "http://127.0.0.1:8080";
const MAX_TURNS = 16;
const TASK_TIMEOUT_MS = 300_000;

const args = process.argv.slice(2);
const onlyArg = valueAfter("--only");
const limitArg = Number(valueAfter("--limit") ?? 0);
let selected = tasks;
if (onlyArg) {
  const wanted = new Set(onlyArg.split(",").map((s) => s.trim()).filter(Boolean));
  selected = selected.filter((task) => wanted.has(task.id));
}
if (limitArg > 0) selected = selected.slice(0, limitArg);

if (!selected.length) {
  console.error("no tasks selected");
  process.exit(2);
}
if (new Set(selected.map((t) => t.id)).size !== selected.length) {
  console.error("duplicate task ids");
  process.exit(2);
}

fs.mkdirSync(runsDir, { recursive: true });
fs.mkdirSync(eventsRoot, { recursive: true });

function valueAfter(flag) {
  const index = args.indexOf(flag);
  return index >= 0 ? args[index + 1] : undefined;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function readModels() {
  const result = spawnSync(CLI, ["models"], { cwd: root, encoding: "utf8", timeout: 30_000 });
  if (result.status !== 0) throw new Error(`models discovery failed: ${result.stderr || result.stdout}`);
  return JSON.parse(result.stdout);
}

async function probe(endpoint) {
  try {
    const response = await fetch(`${endpoint}/v1/models`, { signal: AbortSignal.timeout(1500) });
    if (!response.ok) return { state: "occupied", ids: [] };
    const body = await response.json();
    const ids = (body.data ?? []).map((model) => model.id);
    return { state: ids.includes(MODEL) ? "ready" : "wrong", ids };
  } catch (error) {
    if (error.name === "TimeoutError" || error.name === "AbortError") return { state: "occupied", ids: [] };
    return { state: "free", ids: [] };
  }
}

async function waitForServer(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const result = await probe(URL);
    if (result.state === "ready") return true;
    if (result.state === "wrong") {
      throw new Error(`port 8080 serves ${result.ids.join(", ") || "another model"}, not ${MODEL}`);
    }
    await sleep(500);
  }
  return false;
}

function startServer(weight) {
  const serverExe = path.join(root, "tools", "llama", process.platform === "win32" ? "llama-server.exe" : "llama-server");
  if (!fs.existsSync(serverExe)) throw new Error(`missing pinned server: ${serverExe}`);
  if (!fs.existsSync(weight)) throw new Error(`missing text weight: ${weight}`);
  fs.appendFileSync(serverLog, `\n--- ${new Date().toISOString()} start ---\n`);
  const log = fs.openSync(serverLog, "a");
  const child = spawn(
    serverExe,
    ["-m", weight, "--host", "127.0.0.1", "--port", "8080", "--jinja", "-c", "4096", "-ngl", "0", "--alias", MODEL],
    { cwd: root, stdio: ["ignore", log, log], windowsHide: true },
  );
  return child;
}

function stopServer(child) {
  if (!child || child.killed) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    child.kill("SIGTERM");
  }
}

function getPath(value, dotted) {
  let current = value;
  for (const key of dotted.split(".")) {
    if (current === null || current === undefined) return undefined;
    if (Array.isArray(current)) {
      current = current[Number(key)];
      continue;
    }
    if (typeof current !== "object") return undefined;
    const actual = Object.keys(current).find((candidate) => candidate.toLowerCase() === key.toLowerCase());
    if (actual === undefined) return undefined;
    current = current[actual];
  }
  return current;
}

function containsValue(actual, expected) {
  if (Array.isArray(expected)) return expected.every((item) => containsValue(actual, item));
  if (Array.isArray(actual)) {
    return actual.some((item) => String(item).toLowerCase().includes(String(expected).toLowerCase()));
  }
  if (actual && typeof actual === "object") {
    return Object.values(actual).some((item) => containsValue(item, expected));
  }
  return String(actual ?? "").toLowerCase().includes(String(expected).toLowerCase());
}

function equalsValue(actual, expected) {
  if (typeof expected === "number") {
    const number = Number(actual);
    return Number.isFinite(number) && number === expected;
  }
  if (typeof expected === "boolean") return actual === expected;
  return String(actual ?? "").trim().toLowerCase() === String(expected).trim().toLowerCase();
}

function stripFence(content) {
  const fenced = /^\s*```(?:json)?\s*([\s\S]*?)\s*```\s*$/i.exec(content);
  return fenced ? { text: fenced[1], fenced: true } : { text: content, fenced: false };
}

function grade(task, workspace) {
  const target = path.join(workspace, task.deliverable);
  if (!fs.existsSync(target)) return { pass: false, reasons: ["deliverable missing"] };
  const content = fs.readFileSync(target, "utf8");
  if (!content.trim()) return { pass: false, reasons: ["deliverable empty"] };

  const reasons = [];
  const checks = task.checks ?? {};

  for (const pattern of checks.all ?? []) {
    if (!pattern.test(content)) reasons.push(`missing ${pattern}`);
  }
  for (const pattern of checks.none ?? []) {
    if (pattern.test(content)) reasons.push(`forbidden ${pattern}`);
  }

  const words = content.trim().split(/\s+/).filter(Boolean).length;
  if (checks.minWords && words < checks.minWords) reasons.push(`too short (${words} < ${checks.minWords} words)`);
  if (checks.maxWords && words > checks.maxWords) reasons.push(`too long (${words} > ${checks.maxWords} words)`);

  const jsonChecks = checks.json ?? [];
  if (jsonChecks.length) {
    let parsed;
    const { text, fenced } = stripFence(content);
    try {
      parsed = JSON.parse(text);
    } catch (error) {
      reasons.push(`invalid JSON: ${error.message}`);
    }
    if (parsed !== undefined) {
      for (const check of jsonChecks) {
        const actual = getPath(parsed, check.path);
        if (actual === undefined) {
          reasons.push(`json ${check.path}: missing`);
        } else if (check.equals !== undefined && !equalsValue(actual, check.equals)) {
          reasons.push(`json ${check.path}: expected ${JSON.stringify(check.equals)}, got ${JSON.stringify(actual)}`);
        } else if (check.contains !== undefined && !containsValue(actual, check.contains)) {
          reasons.push(`json ${check.path}: missing ${JSON.stringify(check.contains)}`);
        }
      }
      if (fenced) reasons.push("note: JSON was wrapped in a code fence");
    }
  }

  const hard = reasons.filter((reason) => !reason.startsWith("note:"));
  return {
    pass: hard.length === 0,
    reasons,
    words,
    excerpt: content.slice(0, 400).replace(/\s+/g, " ").trim(),
  };
}

function parseReport(stdout) {
  const start = stdout.indexOf("{");
  if (start < 0) return null;
  try {
    return JSON.parse(stdout.slice(start));
  } catch {
    return null;
  }
}

function percentile(values, fraction) {
  if (!values.length) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.ceil(fraction * sorted.length) - 1)];
}

function summarise(records) {
  const byCategory = {};
  for (const record of records) {
    const bucket = (byCategory[record.category] ??= {
      tasks: 0,
      harnessPass: 0,
      semanticPass: 0,
      turns: 0,
      milliseconds: 0,
      turnSamples: 0,
    });
    bucket.tasks += 1;
    if (record.harnessPass) bucket.harnessPass += 1;
    if (record.semanticPass) bucket.semanticPass += 1;
    if (record.turns !== null) {
      bucket.turns += record.turns;
      bucket.turnSamples += 1;
    }
    bucket.milliseconds += record.milliseconds;
  }

  const harnessPass = records.filter((r) => r.harnessPass).length;
  const semanticPass = records.filter((r) => r.semanticPass).length;
  const truePass = records.filter((r) => r.harnessPass && r.semanticPass).length;
  const durations = records.map((r) => r.milliseconds);
  const turns = records.filter((r) => r.turns !== null).map((r) => r.turns);

  return {
    tasks: records.length,
    harnessPass,
    semanticPass,
    truePass,
    falsePass: records.filter((r) => r.harnessPass && !r.semanticPass).length,
    falseFail: records.filter((r) => !r.harnessPass && r.semanticPass).length,
    bothFail: records.filter((r) => !r.harnessPass && !r.semanticPass).length,
    semanticRate: semanticPass / records.length,
    harnessRate: harnessPass / records.length,
    avgTurns: turns.length ? turns.reduce((a, b) => a + b, 0) / turns.length : 0,
    avgMs: durations.reduce((a, b) => a + b, 0) / records.length,
    medianMs: percentile(durations, 0.5),
    p95Ms: percentile(durations, 0.95),
    totalMs: durations.reduce((a, b) => a + b, 0),
    reviewed: records.filter((r) => r.phases.includes("review")).length,
    planned: records.filter((r) => r.phases[0] === "plan").length,
    byCategory,
  };
}

function renderMarkdown(records, summary, environment) {
  const pct = (value) => `${(value * 100).toFixed(1)}%`;
  const rows = records
    .map(
      (r) =>
        `| ${r.id} | ${r.category} | ${r.harnessPass ? "pass" : `fail (${r.status ?? "error"})`} | ${r.semanticPass ? "pass" : "fail"} | ${r.turns ?? "-"} | ${(r.milliseconds / 1000).toFixed(1)}s | ${r.reasons[0] ? r.reasons[0].slice(0, 80).replace(/\|/g, "\\|") : ""} |`,
    )
    .join("\n");

  const categories = Object.entries(summary.byCategory)
    .map(
      ([name, bucket]) =>
        `| ${name} | ${bucket.tasks} | ${bucket.harnessPass}/${bucket.tasks} | ${bucket.semanticPass}/${bucket.tasks} | ${(bucket.semanticPass / bucket.tasks * 100).toFixed(0)}% | ${(bucket.turns / Math.max(1, bucket.turnSamples)).toFixed(1)} | ${(bucket.milliseconds / bucket.tasks / 1000).toFixed(1)}s |`,
    )
    .join("\n");

  const falsePasses = records.filter((r) => r.harnessPass && !r.semanticPass);
  const failures = records.filter((r) => !r.semanticPass);
  const worstCategory = Object.entries(summary.byCategory).sort(
    (a, b) => a[1].semanticPass / a[1].tasks - b[1].semanticPass / b[1].tasks,
  )[0];
  const slowest = [...records].sort((a, b) => b.milliseconds - a.milliseconds)[0];

  const falsePassSection = falsePasses.length
    ? falsePasses
        .map(
          (r) =>
            `### ${r.id} (${r.category})\n\nGoal: ${r.goal}\n\nHarness: ${r.status} after ${r.turns ?? "-"} turns. Checks failed: ${r.reasons.join("; ")}\n\nOutput excerpt:\n\n> ${r.excerpt}\n`,
        )
        .join("\n")
    : "None. Every harness pass also passed the external content checks.";

  const failureSection = failures.length
    ? failures
        .map(
          (r) =>
            `- **${r.id}** (${r.category}, harness ${r.harnessPass ? "pass" : "fail"}): ${r.reasons.join("; ")}`,
        )
        .join("\n")
    : "None.";

  return `# LFM3 agent benchmark: 50 real tasks

Generated ${environment.generatedAt}. Model \`${environment.model}\` served by the pinned llama.cpp build; sampling temperature ${environment.sampling.temperature}, top-k ${environment.sampling.top_k}, repetition penalty ${environment.sampling.repetition_penalty}. Each job ran with \`--max-turns ${environment.maxTurns}\`, \`--max-calls 3\`, a five-minute deadline, and one isolated workspace under \`tmp/bench-agent50/runs/\`.

The harness pass is its own contract: a non-empty deliverable verified and re-read. The semantic pass is an external, deterministic content check (required facts, numbers, JSON fields, word bounds). A false pass means the harness accepted output whose content is wrong.

## Summary

| Measure | Result |
| --- | --- |
| Tasks | ${summary.tasks} |
| Harness pass | ${summary.harnessPass}/${summary.tasks} (${pct(summary.harnessRate)}) |
| Semantic pass | ${summary.semanticPass}/${summary.tasks} (${pct(summary.semanticRate)}) |
| True passes (both) | ${summary.truePass} |
| False passes (harness pass, content wrong) | ${summary.falsePass} |
| False failures (content right, harness failed) | ${summary.falseFail} |
| Both failed | ${summary.bothFail} |
| Opened with a plan turn | ${summary.planned}/${summary.tasks} |
| Reached a review turn | ${summary.reviewed}/${summary.tasks} |
| Average turns | ${summary.avgTurns.toFixed(1)} |
| Average / median / p95 duration | ${(summary.avgMs / 1000).toFixed(1)}s / ${(summary.medianMs / 1000).toFixed(1)}s / ${(summary.p95Ms / 1000).toFixed(1)}s |
| Total wall time | ${(summary.totalMs / 1000).toFixed(1)}s |

Weakest category by semantic pass rate: **${worstCategory[0]}** (${worstCategory[1].semanticPass}/${worstCategory[1].tasks}). Slowest task: **${slowest.id}** at ${(slowest.milliseconds / 1000).toFixed(1)}s.

## By category

| Category | Tasks | Harness | Semantic | Semantic rate | Avg turns | Avg time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
${categories}

## Per task

| Task | Category | Harness | Content | Turns | Time | First failure |
| --- | --- | --- | --- | ---: | ---: | --- |
${rows}

## False passes

${falsePassSection}

## All content failures

${failureSection}

## Scope

These are real-model runs of the stated-job loop. They do not qualify chat routing, vision, audio, or d1 classification, and they are not a semantic mission holdout. The content checks are stricter than the harness contract in some places (exact numbers and JSON fields) and looser in others (substring matches, no prose-quality judgement). Raw per-task results, event traces, stderr, and deliverables remain under \`tmp/bench-agent50/runs/\`.
`;
}

// Rebuilds the result records from the per-task artifacts of a completed run
// and re-applies the current external checks, without re-running any model
// turn. Harness results come from each task's recorded CLI stdout; durations
// come from the run log. Used after a check is corrected so the recorded
// harness results stay exactly as measured.
function regrade() {
  const logPath = path.join(eventsRoot, "run.log");
  const timings = new Map();
  if (fs.existsSync(logPath)) {
    for (const line of fs.readFileSync(logPath, "utf8").split("\n")) {
      const match = /^\[\d+\/\d+\]\s+(t\d+)\s+\S+\s+harness=\S+.*?\s+([\d.]+)s(?:\s+eta=|$)/.exec(line);
      if (match) timings.set(match[1], Math.round(Number(match[2]) * 1000));
    }
  }

  const records = [];
  for (const task of tasks) {
    const runDir = path.join(runsDir, task.id);
    const stdoutPath = path.join(runDir, "stdout.json");
    const workspace = path.join(runDir, "workspace");
    if (!fs.existsSync(stdoutPath) || !fs.existsSync(workspace)) continue;
    const stdout = fs.readFileSync(stdoutPath, "utf8");
    const report = parseReport(stdout);
    const job = report?.job;
    const turns = Array.isArray(job?.turns) ? job.turns : [];
    const phases = turns.map((turn) => turn.phase).filter(Boolean);
    const calls = turns.reduce((total, turn) => total + (turn.calls?.length ?? 0), 0);
    const graded = grade(task, workspace);
    const harnessPass = job?.status === "passed";
    records.push({
      id: task.id,
      category: task.category,
      goal: task.goal,
      deliverable: task.deliverable,
      exitCode: null,
      signal: null,
      error: null,
      status: job?.status ?? null,
      stopped: job?.stopped ?? null,
      harnessPass,
      semanticPass: graded.pass,
      reasons: graded.reasons,
      words: graded.words,
      excerpt: graded.excerpt,
      turns: job ? turns.length : null,
      calls,
      phases,
      bytes: job?.deliverable?.bytes ?? null,
      digest: job?.deliverable?.digest ?? null,
      milliseconds: timings.get(task.id) ?? 0,
      stdoutBytes: stdout.length,
      stderrTail: fs.existsSync(path.join(runDir, "stderr.log"))
        ? fs.readFileSync(path.join(runDir, "stderr.log"), "utf8").slice(-400)
        : "",
      rebuilt: true,
    });
  }
  if (!records.length) throw new Error(`no run artifacts under ${runsDir}`);

  const previous = fs.existsSync(resultsFile)
    ? JSON.parse(fs.readFileSync(resultsFile, "utf8"))
    : null;
  const environment = previous?.environment ?? {
    generatedAt: new Date().toISOString(),
    model: MODEL,
    sampling: { temperature: 0.1, top_k: 50, repetition_penalty: 1.1 },
    maxTurns: MAX_TURNS,
  };
  environment.regradedAt = new Date().toISOString();
  const summary = summarise(records);
  fs.writeFileSync(
    ndjsonFile,
    records.map((record) => `${JSON.stringify(record)}\n`).join(""),
  );
  fs.writeFileSync(resultsFile, JSON.stringify({ environment, summary, tasks: records }, null, 2));
  fs.writeFileSync(reportFile, renderMarkdown(records, summary, environment));
  console.log(
    `Regraded ${summary.tasks} recorded tasks: ${summary.semanticPass} content passes, ${summary.harnessPass} harness passes, ${summary.falsePass} false passes.`,
  );
}

async function main() {
  if (args.includes("--regrade")) {
    regrade();
    return;
  }
  if (!fs.existsSync(CLI)) throw new Error(`missing CLI: build it first: cargo build -p lfm3-cli`);
  for (const task of selected) {
    for (const source of task.sources) {
      const file = path.join(corpus, source);
      if (!fs.existsSync(file)) throw new Error(`${task.id}: missing corpus file ${source}`);
    }
  }

  const models = readModels();
  const weight = path.resolve(root, models.text_weight);
  const probeResult = await probe(URL);
  let server = null;
  let owned = false;
  const reused = probeResult.state === "ready";
  if (!reused) {
    if (probeResult.state === "wrong" || probeResult.state === "occupied") {
      throw new Error(`port 8080 is already in use (${probeResult.ids.join(", ") || "unknown server"}); stop it first`);
    }
    server = startServer(weight);
    owned = true;
    if (!(await waitForServer(180_000))) {
      stopServer(server);
      throw new Error("llama-server did not become ready within 180s; see tmp/bench-agent50/llama-server.log");
    }
  }

  const cleanup = () => stopServer(server);
  process.on("SIGINT", () => {
    cleanup();
    process.exit(130);
  });
  process.on("SIGTERM", () => {
    cleanup();
    process.exit(143);
  });

  const records = [];
  const startedAll = Date.now();
  fs.writeFileSync(ndjsonFile, "");
  try {
    for (const [index, task] of selected.entries()) {
      const health = await probe(URL);
      if (health.state !== "ready") {
        if (!owned) throw new Error(`external server stopped before ${task.id}`);
        console.log(`server not ready before ${task.id}; restarting`);
        stopServer(server);
        server = startServer(weight);
        if (!(await waitForServer(180_000))) throw new Error(`server failed to restart before ${task.id}`);
      }
      const runDir = path.join(runsDir, task.id);
      const workspace = path.join(runDir, "workspace");
      fs.rmSync(workspace, { recursive: true, force: true });
      fs.mkdirSync(path.join(workspace, "out"), { recursive: true });
      for (const source of task.sources) {
        const destination = path.join(workspace, source);
        fs.mkdirSync(path.dirname(destination), { recursive: true });
        fs.copyFileSync(path.join(corpus, source), destination);
      }
      fs.writeFileSync(path.join(runDir, "task.json"), JSON.stringify(task, null, 2));
      const eventsFile = path.join(runDir, "events.jsonl");
      fs.rmSync(eventsFile, { force: true });

      const started = Date.now();
      const result = spawnSync(
        CLI,
        [
          "job",
          "--goal",
          task.goal,
          "--workspace",
          workspace,
          "--deliverable",
          task.deliverable,
          "--minutes",
          "5",
          "--timeout",
          String(TASK_TIMEOUT_MS / 1000),
          "--max-turns",
          String(MAX_TURNS),
          "--max-calls",
          "3",
          "--external",
          "--startup-timeout",
          "30",
          "--events",
          eventsFile,
        ],
        { cwd: root, encoding: "utf8", timeout: TASK_TIMEOUT_MS + 30_000, maxBuffer: 128 * 1024 * 1024, windowsHide: true },
      );
      const milliseconds = Date.now() - started;
      fs.writeFileSync(path.join(runDir, "stdout.json"), result.stdout ?? "");
      fs.writeFileSync(path.join(runDir, "stderr.log"), result.stderr ?? "");

      const report = parseReport(result.stdout ?? "");
      const job = report?.job;
      const turns = Array.isArray(job?.turns) ? job.turns : [];
      const phases = turns.map((turn) => turn.phase).filter(Boolean);
      const calls = turns.reduce((total, turn) => total + (turn.calls?.length ?? 0), 0);
      const harnessPass = result.status === 0 && job?.status === "passed";
      const gradeResult = grade(task, workspace);

      // A killed invocation can still have left a correct file; that content
      // pass is real, but the harness did not certify it.
      const record = {
        id: task.id,
        category: task.category,
        goal: task.goal,
        deliverable: task.deliverable,
        exitCode: result.status,
        signal: result.signal ?? null,
        error: result.error ? String(result.error) : null,
        status: job?.status ?? null,
        stopped: job?.stopped ?? null,
        harnessPass,
        semanticPass: gradeResult.pass,
        reasons: gradeResult.reasons,
        words: gradeResult.words,
        excerpt: gradeResult.excerpt,
        turns: job ? turns.length : null,
        calls,
        phases,
        bytes: job?.deliverable?.bytes ?? null,
        digest: job?.deliverable?.digest ?? null,
        milliseconds,
        stdoutBytes: (result.stdout ?? "").length,
        stderrTail: (result.stderr ?? "").slice(-400),
      };
      records.push(record);
      fs.appendFileSync(ndjsonFile, `${JSON.stringify(record)}\n`);

      const elapsed = (Date.now() - startedAll) / 1000;
      const remaining = (elapsed / (index + 1)) * (selected.length - index - 1);
      console.log(
        `[${index + 1}/${selected.length}] ${task.id} ${task.category} harness=${harnessPass ? "pass" : record.status ?? "error"} content=${gradeResult.pass ? "pass" : "fail"} turns=${record.turns ?? "-"} ${(milliseconds / 1000).toFixed(1)}s eta=${(remaining / 60).toFixed(1)}m${gradeResult.pass ? "" : ` :: ${gradeResult.reasons.join("; ").slice(0, 120)}`}`,
      );
    }
  } finally {
    cleanup();
  }

  const summary = summarise(records);
  const environment = {
    generatedAt: new Date().toISOString(),
    model: MODEL,
    sampling: { temperature: 0.1, top_k: 50, repetition_penalty: 1.1 },
    maxTurns: MAX_TURNS,
    server: { reused, weight },
    cli: path.relative(root, CLI),
  };
  fs.writeFileSync(resultsFile, JSON.stringify({ environment, summary, tasks: records }, null, 2));
  fs.writeFileSync(reportFile, renderMarkdown(records, summary, environment));
  console.log(
    `\nDone: ${summary.semanticPass}/${summary.tasks} content passes, ${summary.harnessPass}/${summary.tasks} harness passes, ${summary.falsePass} false passes, ${summary.avgTurns.toFixed(1)} avg turns, ${(summary.totalMs / 1000 / 60).toFixed(1)} min.`,
  );
  console.log(`Report: ${path.relative(root, reportFile)}`);
}

main().catch((error) => {
  console.error(error.stack || String(error));
  process.exit(1);
});
