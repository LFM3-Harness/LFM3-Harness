import type {
  ChatEventListener,
  DownloadEventListener,
  EngineBridge,
  RunEventListener,
  Unsubscribe,
} from './EngineBridge';
import {
  ACCEPTANCE_HASH,
  ATTEMPT_ID,
  DIGEST,
  HEALTH,
  INCOMPLETE_EVENTS,
  INCOMPLETE_RUN,
  INCOMPLETE_SNAPSHOT,
  INTERRUPTED_EVENTS,
  INTERRUPTED_RUN,
  INTERRUPTED_SNAPSHOT,
  MISSION,
  MODELS,
  PASSED_EVENTS,
  PASSED_RUN,
  PASSED_SNAPSHOT,
  SECOND_ATTEMPT_ID,
  acceptanceFor,
  event,
} from './fixtures';
import type {
  AcceptanceSpec,
  ChatMessageRecord,
  ChatSession,
  ChatSessionView,
  HealthReport,
  JsonObject,
  JsonValue,
  MissionManifest,
  ModelCatalog,
  ModelKey,
  ModelStatus,
  RunEvent,
  RunId,
  RunRecord,
  RunSnapshot,
  JobEvent,
  JobRecord,
  JobView,
  SpeechReply,
  StartJobResult,
  StartRunRequest,
  StepSnapshot,
  AttemptSnapshot,
} from './types';

export type DemoScenario = 'workflow' | 'deny' | 'cancel' | 'gap';

interface DemoRun {
  record: RunRecord;
  steps: StepSnapshot[];
  events: RunEvent[];
  acceptance: AcceptanceSpec;
  nextSeq: number;
  pendingApproval: string | null;
  gapPending: boolean;
}

const TERMINAL = ['passed', 'incomplete', 'cancelled', 'interrupted'];

/**
 * Gap releases live at module scope so the Playwright hook reaches whichever
 * demo bridge the mounted app actually uses, including after a double mount.
 */
const gapReleases = new Set<() => void>();

interface DemoTestHooks {
  releaseGap(): void;
}

(globalThis as unknown as { __lfm3Demo?: DemoTestHooks }).__lfm3Demo = {
  releaseGap() {
    for (const release of [...gapReleases]) {
      release();
    }
  },
};

/**
 * Deterministic, read-mostly browser fixtures for component and Playwright
 * acceptance. Mutations stay in memory, every state change is a durable-looking
 * event, and no workspace or process is ever touched.
 */
export class DemoBridge implements EngineBridge {
  readonly kind = 'demo' as const;

  private readonly listeners = new Map<RunId, Set<RunEventListener>>();
  private readonly runs = new Map<RunId, DemoRun>();
  private activeId: string | null = null;
  private counter = 0;
  private readonly scenario: DemoScenario;

  constructor(scenario: string | null = null) {
    this.scenario = isScenario(scenario) ? scenario : 'workflow';
    this.seed(PASSED_RUN, PASSED_SNAPSHOT.steps, PASSED_EVENTS);
    this.seed(INCOMPLETE_RUN, INCOMPLETE_SNAPSHOT.steps, INCOMPLETE_EVENTS);
    this.seed(INTERRUPTED_RUN, INTERRUPTED_SNAPSHOT.steps, INTERRUPTED_EVENTS);
  }

  async health(): Promise<HealthReport> {
    return HEALTH;
  }

  async listMissions(): Promise<MissionManifest[]> {
    return [MISSION];
  }

  async listRuns(limit: number): Promise<RunRecord[]> {
    return [...this.runs.values()]
      .map((run) => ({ ...run.record }))
      .sort((left, right) => right.created_at - left.created_at)
      .slice(0, limit);
  }

  async listJobs(): Promise<JobRecord[]> {
    return [];
  }

  async startJob(): Promise<StartJobResult> {
    throw new Error(
      'Demo fixtures describe verified runs only; start a job in the desktop app.',
    );
  }

  async getJob(): Promise<JobView> {
    throw new Error('Demo fixtures contain no jobs.');
  }

  async getJobEvents(): Promise<JobEvent[]> {
    throw new Error('Demo fixtures contain no jobs.');
  }

  async cancelJob(): Promise<void> {
    throw new Error('Demo fixtures contain no jobs.');
  }

  async subscribeJob(): Promise<Unsubscribe> {
    return () => undefined;
  }

  async startRun(request: StartRunRequest): Promise<RunId> {
    this.ensureNoActiveRun();
    const goal =
      typeof request.inputs['goal'] === 'string' && request.inputs['goal'].length > 0
        ? request.inputs['goal']
        : 'Demo desk task';
    const now = Date.now();
    const acceptance = acceptanceFor(goal);
    const runId = this.newRunId();
    const record: RunRecord = {
      id: runId,
      mission: MISSION.name,
      mission_version: MISSION.version,
      acceptance_hash: ACCEPTANCE_HASH,
      goal,
      status: 'awaiting_plan_approval',
      created_at: now,
      updated_at: now,
      parent_run_id: null,
    };
    const run: DemoRun = {
      record,
      steps: acceptance.steps.map((step) => ({
        index: step.index,
        title: step.title,
        criterion: step.criterion,
        status: 'pending',
        attempts: [],
      })),
      events: [],
      acceptance,
      nextSeq: 0,
      pendingApproval: null,
      gapPending: false,
    };
    this.runs.set(runId, run);
    this.activeId = runId;
    this.emit(run, 'run.created', {
      acceptance_hash: ACCEPTANCE_HASH,
      goal,
      mission: MISSION.name,
      mission_version: MISSION.version,
    });
    this.emit(run, 'run.status', {
      from: 'created',
      reason: null,
      status: 'awaiting_plan_approval',
    });
    return runId;
  }

  async getAcceptance(runId: RunId): Promise<AcceptanceSpec> {
    return this.requireRun(runId).acceptance;
  }

  async getRunSnapshot(runId: RunId): Promise<RunSnapshot> {
    const run = this.requireRun(runId);
    if (run.gapPending) {
      return new Promise<RunSnapshot>((resolve) => {
        const release = () => {
          run.gapPending = false;
          gapReleases.delete(release);
          resolve(snapshotOf(run));
        };
        gapReleases.add(release);
      });
    }
    return snapshotOf(run);
  }

  async getRunEvents(runId: RunId, after: number): Promise<RunEvent[]> {
    const run = this.requireRun(runId);
    return run.events.filter((entry) => entry.seq > after);
  }

  async approvePlan(runId: RunId, acceptanceHash: string): Promise<void> {
    const run = this.requireRun(runId);
    if (run.record.status !== 'awaiting_plan_approval') {
      throw new Error(`run ${runId} is not awaiting plan approval`);
    }
    if (acceptanceHash !== run.record.acceptance_hash) {
      throw new Error('acceptance hash does not match this run');
    }

    this.transition(run, 'running');
    this.emit(run, 'run.status', { from: 'awaiting_plan_approval', reason: null, status: 'running' });
    this.startAttempt(run, ATTEMPT_ID, 1);
    this.emit(run, 'tool.result', {
      artifacts: [DIGEST],
      attempt: ATTEMPT_ID,
      call_id: 'demo-call-1',
      exit_code: null,
      status: 'succeeded',
      tool: 'write_file',
    });
    const failure = 'out/summary.md does not contain "owner"';
    this.finishAttempt(run, ATTEMPT_ID, 'failed', failure);
    this.emit(run, 'check.result', {
      attempt: ATTEMPT_ID,
      evidence_count: 1,
      kind: 'content_matches',
      message: failure,
      sources: ['workspace:out/summary.md'],
      status: 'fail',
      step_index: 1,
    });
    this.emit(run, 'step.failed', {
      attempt: ATTEMPT_ID,
      error: failure,
      status: 'failed',
      step_index: 1,
    });
    this.emit(run, 'recovery.decision', {
      step_index: 1,
      strategy: 1,
      decision: { action: 'retry', reason: 'check_failed' },
    });

    this.startAttempt(run, SECOND_ATTEMPT_ID, 2);

    if (this.scenario === 'cancel') {
      return;
    }
    if (this.scenario === 'gap') {
      this.completeWithGap(run);
      return;
    }
    this.requestApproval(run);
  }

  async decideApproval(approvalId: string, allow: boolean): Promise<void> {
    const run = [...this.runs.values()].find((entry) => entry.pendingApproval === approvalId);
    if (!run) {
      throw new Error(`approval ${approvalId} is not pending`);
    }
    run.pendingApproval = null;
    this.emit(run, 'approval.resolved', { allow, approval_id: approvalId });
    if (!allow) {
      this.finishAttempt(run, SECOND_ATTEMPT_ID, 'aborted', 'approval denied');
      this.emit(run, 'step.blocked', {
        attempt: SECOND_ATTEMPT_ID,
        error: 'approval denied',
        status: 'aborted',
        step_index: 1,
      });
      this.finish(run, 'incomplete', 'approval_denied');
      return;
    }
    this.transition(run, 'running');
    this.emit(run, 'run.status', { from: 'awaiting_user', reason: null, status: 'running' });
    this.completePass(run);
  }

  async cancelRun(runId: RunId): Promise<void> {
    const run = this.requireRun(runId);
    if (isTerminal(run.record.status)) {
      return;
    }
    const executing = run.steps[0]?.attempts.find((attempt) => attempt.status === 'executing');
    if (executing) {
      this.finishAttempt(run, executing.id, 'interrupted', 'cancelled by user');
      this.emit(run, 'step.blocked', {
        attempt: executing.id,
        error: 'cancelled by user',
        status: 'interrupted',
        step_index: 1,
      });
    }
    this.finish(run, 'cancelled', 'cancelled');
  }

  async retryRun(runId: RunId): Promise<RunId> {
    const parent = this.requireRun(runId);
    if (!isTerminal(parent.record.status)) {
      throw new Error(`run ${runId} is still active`);
    }
    this.ensureNoActiveRun();
    const now = Date.now();
    const acceptance = acceptanceFor(parent.record.goal);
    const childId = this.newRunId();
    const record: RunRecord = {
      id: childId,
      mission: parent.record.mission,
      mission_version: parent.record.mission_version,
      acceptance_hash: parent.record.acceptance_hash,
      goal: parent.record.goal,
      status: 'awaiting_plan_approval',
      created_at: now,
      updated_at: now,
      parent_run_id: parent.record.id,
    };
    const child: DemoRun = {
      record,
      steps: acceptance.steps.map((step) => ({
        index: step.index,
        title: step.title,
        criterion: step.criterion,
        status: 'pending',
        attempts: [],
      })),
      events: [],
      acceptance,
      nextSeq: 0,
      pendingApproval: null,
      gapPending: false,
    };
    this.runs.set(childId, child);
    this.activeId = childId;
    this.emit(child, 'run.created', {
      acceptance_hash: ACCEPTANCE_HASH,
      goal: record.goal,
      mission: record.mission,
      mission_version: record.mission_version,
      parent_run_id: record.parent_run_id,
    });
    this.emit(child, 'run.status', {
      from: 'created',
      reason: null,
      status: 'awaiting_plan_approval',
    });
    return childId;
  }

  async listModels(): Promise<ModelStatus[]> {
    return MODELS;
  }

  async releaseModel(_modelKey: ModelKey): Promise<void> {
    return undefined;
  }

  async chooseWorkspace(): Promise<string | null> {
    throw new Error('Folder selection needs the desktop app; demo data keeps its fixed folder.');
  }

  async modelCatalog(): Promise<ModelCatalog> {
    return demoCatalog();
  }

  async downloadModel(_modelKey: ModelKey): Promise<void> {
    throw new Error('Demo data cannot download models; run the desktop app.');
  }

  async cancelDownload(_modelKey: ModelKey): Promise<void> {
    throw new Error('Demo data has no downloads.');
  }

  async reloadModels(): Promise<ModelStatus[]> {
    return MODELS;
  }

  async subscribeDownloads(_listener: DownloadEventListener): Promise<Unsubscribe> {
    return () => undefined;
  }

  async chooseInputFiles(_mission: string, _inputKey: string): Promise<string[]> {
    return [];
  }

  async openArtifact(_artifactId: string): Promise<void> {
    throw new Error('DemoBridge has no real artifact to open.');
  }

  async subscribe(runId: RunId, listener: RunEventListener): Promise<Unsubscribe> {
    this.requireRun(runId);
    const listeners = this.listeners.get(runId) ?? new Set<RunEventListener>();
    listeners.add(listener);
    this.listeners.set(runId, listeners);
    return () => {
      listeners.delete(listener);
    };
  }

  // Chat needs a real local model and workspace. Demo mode shows the verified
  // run fixtures only, and refuses chat instead of simulating an answer.

  async listChatSessions(_limit: number): Promise<ChatSession[]> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  async createChatSession(_title?: string): Promise<ChatSession> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  async getChatSession(_sessionId: string): Promise<ChatSessionView> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  async renameChatSession(_sessionId: string, _title: string): Promise<void> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  async deleteChatSession(_sessionId: string): Promise<void> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  async sendChatMessage(_sessionId: string, _content: string): Promise<ChatMessageRecord> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  async cancelChat(_sessionId: string): Promise<void> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  async decideChatAction(_actionId: string, _allow: boolean): Promise<void> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  async transcribeVoice(_audioBase64: string, _format: string): Promise<string> {
    throw new Error('Voice input is unavailable in demo data; run the desktop app.');
  }

  async synthesizeSpeech(_text: string): Promise<SpeechReply> {
    throw new Error('Voice output is unavailable in demo data; run the desktop app.');
  }

  async attachImage(_name: string, _dataBase64: string): Promise<string> {
    throw new Error('Image attachments are unavailable in demo data; run the desktop app.');
  }

  async subscribeChat(_listener: ChatEventListener): Promise<Unsubscribe> {
    throw new Error('Chat is unavailable in demo data; run the desktop app.');
  }

  // -------------------------------------------------------------- internals

  private seed(record: RunRecord, steps: StepSnapshot[], events: RunEvent[]): void {
    this.runs.set(record.id, {
      record: { ...record },
      steps: steps.map((step) => ({
        ...step,
        attempts: step.attempts.map((attempt) => ({ ...attempt })),
      })),
      events: events.map((entry) => ({ ...entry })),
      acceptance: acceptanceFor(record.goal),
      nextSeq: events.at(-1)?.seq ?? 0,
      pendingApproval: null,
      gapPending: false,
    });
  }

  private ensureNoActiveRun(): void {
    if (this.activeId === null) {
      return;
    }
    const active = this.runs.get(this.activeId);
    if (active && !isTerminal(active.record.status)) {
      throw new Error('Another run is active; finish or cancel it before starting a new run.');
    }
  }

  private newRunId(): string {
    this.counter += 1;
    return `demo${String(this.counter).padStart(4, '0')}${'0'.repeat(24)}`;
  }

  private requireRun(runId: RunId): DemoRun {
    const run = this.runs.get(runId);
    if (!run) {
      throw new Error(`DemoBridge has no run ${runId}`);
    }
    return run;
  }

  private transition(run: DemoRun, status: RunRecord['status']): void {
    run.record = { ...run.record, status, updated_at: Date.now() };
  }

  private emit(
    run: DemoRun,
    kind: RunEvent['kind'],
    payload: JsonObject,
    deliver = true,
  ): number {
    run.nextSeq += 1;
    const entry = event(run.record.id, run.nextSeq, kind, payload, Date.now());
    run.events.push(entry);
    if (deliver) {
      for (const listener of this.listeners.get(run.record.id) ?? []) {
        listener(entry);
      }
    }
    return entry.seq;
  }

  private startAttempt(run: DemoRun, attemptId: string, strategy: number): void {
    const step = run.steps[0];
    step.status = 'running';
    const action: JsonValue = {
      tool: 'write_file',
      arguments: { path: 'out/summary.md' },
      call_id: `demo-call-${strategy}`,
    };
    const attempt: AttemptSnapshot = {
      id: attemptId,
      strategy,
      status: 'executing',
      action,
      model: 'lfm3-1.7b',
      started_at: Date.now(),
      finished_at: null,
      error: null,
    };
    step.attempts.push(attempt);
    this.emit(run, 'attempt.started', {
      action: { attempt: strategy, phase: 'proposed', step: 1 },
      attempt: attemptId,
      step_index: 1,
      strategy,
    });
  }

  private finishAttempt(
    run: DemoRun,
    attemptId: string,
    status: AttemptSnapshot['status'],
    error: string | null,
  ): void {
    const attempt = run.steps[0]?.attempts.find((entry) => entry.id === attemptId);
    if (attempt) {
      attempt.status = status;
      attempt.finished_at = Date.now();
      attempt.error = error;
    }
  }

  private requestApproval(run: DemoRun): void {
    this.transition(run, 'awaiting_user');
    this.emit(run, 'run.status', { from: 'running', reason: null, status: 'awaiting_user' });
    this.counter += 1;
    const approvalId = `approval-${this.counter}`;
    run.pendingApproval = approvalId;
    this.emit(run, 'approval.requested', {
      approval_id: approvalId,
      run_id: run.record.id,
      step_index: 1,
      tool: 'write_file',
      summary: 'Overwrite out/summary.md',
      detail: 'The retry wants to replace the summary with the corrected handover facts.',
      risk: 'medium',
      expires_at: null,
    });
  }

  private completePass(run: DemoRun): void {
    this.finishAttempt(run, SECOND_ATTEMPT_ID, 'passed', null);
    this.emit(run, 'tool.result', {
      artifacts: [DIGEST],
      attempt: SECOND_ATTEMPT_ID,
      call_id: 'demo-call-2',
      exit_code: null,
      status: 'succeeded',
      tool: 'write_file',
    });
    this.emit(run, 'check.result', {
      attempt: SECOND_ATTEMPT_ID,
      evidence_count: 1,
      kind: 'content_matches',
      message: 'out/summary.md matches every required condition',
      sources: ['workspace:out/summary.md'],
      status: 'pass',
      step_index: 1,
    });
    run.steps[0].status = 'passed';
    this.emit(run, 'step.passed', {
      attempt: SECOND_ATTEMPT_ID,
      error: null,
      status: 'passed',
      step_index: 1,
    });
    this.transition(run, 'final_verifying');
    this.emit(run, 'run.final_verifying', {
      from: 'running',
      reason: null,
      status: 'final_verifying',
    });
    this.finish(run, 'passed', null);
  }

  /** Appends the complete final journal, delivering one later event out of order. */
  private completeWithGap(run: DemoRun): void {
    this.finishAttempt(run, SECOND_ATTEMPT_ID, 'passed', null);
    run.steps[0].status = 'passed';
    this.transition(run, 'passed');
    this.emit(
      run,
      'tool.result',
      {
        artifacts: [DIGEST],
        attempt: SECOND_ATTEMPT_ID,
        call_id: 'demo-call-2',
        exit_code: null,
        status: 'succeeded',
        tool: 'write_file',
      },
      false,
    );
    run.gapPending = true;
    this.emit(run, 'check.result', {
      attempt: SECOND_ATTEMPT_ID,
      evidence_count: 1,
      kind: 'content_matches',
      message: 'out/summary.md matches every required condition',
      sources: ['workspace:out/summary.md'],
      status: 'pass',
      step_index: 1,
    });
    this.emit(run, 'step.passed', {
      attempt: SECOND_ATTEMPT_ID,
      error: null,
      status: 'passed',
      step_index: 1,
    }, false);
    this.emit(run, 'run.final_verifying', {
      from: 'running',
      reason: null,
      status: 'final_verifying',
    }, false);
    this.emit(run, 'run.finished', {
      from: 'final_verifying',
      reason: null,
      status: 'passed',
    }, false);
  }

  private finish(run: DemoRun, status: RunRecord['status'], reason: string | null): void {
    const previous = run.record.status;
    this.transition(run, status);
    this.emit(run, 'run.finished', {
      from: previous,
      reason,
      status,
    });
  }

}

function snapshotOf(run: DemoRun): RunSnapshot {
  return {
    run: { ...run.record },
    steps: run.steps.map((step) => ({
      ...step,
      attempts: step.attempts.map((attempt) => ({ ...attempt })),
    })),
  };
}

function isTerminal(status: RunRecord['status']): boolean {
  return TERMINAL.includes(status);
}

function isScenario(value: string | null): value is DemoScenario {
  return value === 'workflow' || value === 'deny' || value === 'cancel' || value === 'gap';
}

/** Deterministic download catalog for the browser demo, derived from MODELS. */
function demoCatalog(): ModelCatalog {
  const roles: Array<{ key: ModelKey; title: string; repo: string; files: [string, number][] }> = [
    {
      key: 'text',
      title: 'Text and agent',
      repo: 'LFM3-1.7B-GGUF',
      files: [['LFM3-1.7B-Q4_K_M.gguf', 1_099_627_808]],
    },
    {
      key: 'vision',
      title: 'Vision',
      repo: 'LFM3-VL-2B-GGUF',
      files: [
        ['LFM3-VL-2B-QAD-Q4_0.gguf', 1_049_615_648],
        ['mmproj-LFM3-VL-2B-Q8_0.gguf', 203_031_872],
      ],
    },
    {
      key: 'audio',
      title: 'Voice in and out',
      repo: 'LFM3-Audio-2B-GGUF',
      files: [
        ['LFM3-Audio-2B-Q4_K_M.gguf', 1_099_626_016],
        ['mmproj-LFM3-Audio-2B-Q8_0.gguf', 303_811_840],
      ],
    },
    {
      key: 'd1',
      title: 'Decision',
      repo: 'd1-2B-GGUF',
      files: [['d1-2B-Q4_K_M.gguf', 1_099_629_888]],
    },
  ];
  return {
    models_dir: 'demo://local/models',
    download: null,
    pending_reload: false,
    runtime_ready: true,
    runtime_note: null,
    models: roles.map((role) => {
      const status = MODELS.find((model) => model.key === role.key);
      const loaded = status?.state === 'ready';
      const installed = loaded || status?.state === 'installed';
      const files = role.files.map(([name, bytes]) => ({ name, bytes, sha256: '0'.repeat(64) }));
      return {
        key: role.key,
        title: role.title,
        repo: role.repo,
        source: `LiquidAI/${role.repo}`,
        license: 'LFM Open License (see the model card)',
        total_bytes: files.reduce((total, file) => total + file.bytes, 0),
        files,
        installed,
        loaded,
        state: loaded ? 'loaded' : installed ? 'installed' : 'absent',
      };
    }),
  };
}
