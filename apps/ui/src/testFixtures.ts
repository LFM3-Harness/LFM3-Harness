/**
 * Shared component fixtures. These are presentation inputs only: every
 * component still renders whatever the engine reports and never upgrades a
 * terminal state.
 */

import type {
  AcceptanceSpec,
  CheckSpec,
  HealthReport,
  MissionManifest,
  ModelStatus,
  RunEvent,
  RunRecord,
  RunSnapshot,
} from './bridge/types';

export const ACCEPTANCE_HASH =
  'f7ed5f17bda3c957797569028aab3e375b42998cbf65d489bdc7a9e8e7b6aba5';
export const RUN_ID = 'd1e2f3a4b5c6d7e8f90123456789abcd';
export const ATTEMPT_ID = '0123456789abcdef0123456789abcdef';
export const RETRY_RUN_ID = 'aa11bb22cc33dd44ee55ff6677889900';
export const INTERRUPTED_RUN_ID = 'bb22cc33dd44ee55ff6677889900aa11';

export const DESK_MISSION: MissionManifest = {
  schema_version: 1,
  name: 'desk_task',
  version: 1,
  title: 'Workspace task',
  description: 'Change approved workspace files and prove every change with a check.',
  inputs: {
    goal: { kind: 'string', required: true, schema: null },
    source_note: { kind: 'file', required: false, schema: null },
  },
  tools: ['list_files', 'read_file', 'write_file', 'edit_file'],
  checks: ['file_exists', 'content_matches', 'content_absent'],
  command_templates: [],
  output: {
    schema: null,
    required_artifacts: [{ path: 'out/summary.md', non_empty: true }],
  },
  limits: { max_steps: 8, max_attempts: 3 },
};

export const SCREEN_MISSION: MissionManifest = {
  schema_version: 1,
  name: 'screen_qa',
  version: 1,
  title: 'Screen question',
  description: 'Answer one question about a supplied screen capture.',
  inputs: {
    screenshot: { kind: 'image', required: true, schema: null },
    question: { kind: 'string', required: true, schema: null },
  },
  tools: ['look', 'write_file'],
  checks: ['vl_criterion', 'file_exists', 'content_matches'],
  command_templates: [],
  output: {
    schema: null,
    required_artifacts: [{ path: 'answer.md', non_empty: true }],
  },
  limits: { max_steps: 2, max_attempts: 2 },
  semantic: {
    mode: 'human_review_only',
    min_confidence_bps: 8000,
    qualification: null,
    reason:
      'No packaged holdout qualification has been recorded, so a visual judgement can never pass a run.',
  },
};

const SUMMARY_CHECKS: CheckSpec[] = [
  { kind: 'file_exists', path: 'out/summary.md' },
  {
    kind: 'content_matches',
    path: 'out/summary.md',
    contains: ['handover', 'owner'],
  },
];

export const ACCEPTANCE: AcceptanceSpec = {
  schema_version: 1,
  mission: 'desk_task',
  mission_version: 1,
  goal: 'Read notes/handover.md and write a correct summary to out/summary.md.',
  inputs: {
    source_note: { id: 'notes-1', sha256: 'a'.repeat(64) },
  },
  steps: [
    {
      index: 1,
      title: 'Write the summary',
      criterion: 'out/summary.md exists and contains every required handover fact.',
      checks: SUMMARY_CHECKS,
      tool: 'write_file',
    },
    {
      index: 2,
      title: 'Confirm the summary',
      criterion: 'The summary contains no invented owner or deadline.',
      checks: [{ kind: 'content_absent', path: 'out/summary.md', terms: ['unicorn'] }],
      tool: null,
    },
  ],
  output: {
    schema: null,
    required_artifacts: [{ path: 'out/summary.md', non_empty: true }],
  },
  allowed_tools: ['list_files', 'read_file', 'write_file', 'edit_file'],
  allowed_checks: ['file_exists', 'content_matches', 'content_absent'],
  max_steps: 8,
  max_attempts: 3,
};

const RUN: RunRecord = {
  id: RUN_ID,
  mission: 'desk_task',
  mission_version: 1,
  acceptance_hash: ACCEPTANCE_HASH,
  goal: ACCEPTANCE.goal,
  status: 'passed',
  created_at: 1_762_000_000_000,
  updated_at: 1_762_000_001_861,
  parent_run_id: null,
};

export function runRecord(overrides: Partial<RunRecord> = {}): RunRecord {
  return { ...RUN, ...overrides };
}

export const PASSED_SNAPSHOT: RunSnapshot = {
  run: RUN,
  steps: [
    {
      index: 1,
      title: 'Write the summary',
      criterion: ACCEPTANCE.steps[0].criterion,
      status: 'passed',
      attempts: [
        {
          id: ATTEMPT_ID,
          strategy: 1,
          status: 'passed',
          action: {
            tool: 'write_file',
            arguments: { path: 'out/summary.md' },
            call_id: 'call-1',
          },
          model: 'lfm3-1.7b',
          started_at: RUN.created_at + 3,
          finished_at: RUN.updated_at - 4,
          error: null,
        },
      ],
    },
    {
      index: 2,
      title: 'Confirm the summary',
      criterion: ACCEPTANCE.steps[1].criterion,
      status: 'passed',
      attempts: [],
    },
  ],
};

export const RETRIED_SNAPSHOT: RunSnapshot = {
  run: {
    ...RUN,
    id: RETRY_RUN_ID,
    status: 'incomplete',
    parent_run_id: 'ffffffff000000001111111122222222',
  },
  steps: [
    {
      index: 1,
      title: 'Write the summary',
      criterion: ACCEPTANCE.steps[0].criterion,
      status: 'blocked',
      attempts: [
        {
          id: 'aaaabbbbccccddddeeeeffff00001111',
          strategy: 1,
          status: 'failed',
          action: { tool: 'write_file', arguments: { path: 'out/summary.md' } },
          model: 'lfm3-1.7b',
          started_at: RUN.created_at + 10,
          finished_at: RUN.created_at + 900,
          error: 'out/summary.md does not contain "owner"',
        },
        {
          id: '22223333444455556666777788889999',
          strategy: 2,
          status: 'failed',
          action: { tool: 'read_file', arguments: { path: 'notes/handover.md' } },
          model: 'lfm3-1.7b',
          started_at: RUN.created_at + 1_000,
          finished_at: RUN.created_at + 1_800,
          error: 'step budget exhausted',
        },
      ],
    },
  ],
};

export const INTERRUPTED_SNAPSHOT: RunSnapshot = {
  run: { ...RUN, id: INTERRUPTED_RUN_ID, status: 'interrupted' },
  steps: [
    {
      index: 1,
      title: 'Write the summary',
      criterion: ACCEPTANCE.steps[0].criterion,
      status: 'blocked',
      attempts: [
        {
          id: '99998888777766665555444433332222',
          strategy: 1,
          status: 'interrupted',
          action: { tool: 'write_file', arguments: { path: 'out/summary.md' } },
          model: 'lfm3-1.7b',
          started_at: RUN.created_at + 20,
          finished_at: null,
          error: 'engine stopped while the attempt was executing',
        },
      ],
    },
  ],
};

function event(
  runId: string,
  seq: number,
  kind: RunEvent['kind'],
  payload: RunEvent['payload'],
  at = RUN.created_at + seq,
): RunEvent {
  return { run_id: runId, seq, kind, payload, at };
}

export const PASSED_EVENTS: RunEvent[] = [
  event(RUN_ID, 1, 'run.created', {
    acceptance_hash: ACCEPTANCE_HASH,
    goal: ACCEPTANCE.goal,
    mission: 'desk_task',
    mission_version: 1,
  }),
  event(RUN_ID, 2, 'run.status', { from: 'created', reason: null, status: 'running' }),
  event(RUN_ID, 3, 'step.started', { step_index: 1, title: 'Write the summary' }),
  event(RUN_ID, 4, 'attempt.started', {
    action: { attempt: 1, phase: 'proposed', step: 1 },
    attempt: ATTEMPT_ID,
    step_index: 1,
    strategy: 1,
  }),
  event(RUN_ID, 5, 'tool.result', {
    artifacts: ['3b7c1d9e5f0a4c2b8d6e1f3a5c7b9d0e2f4a6c8b1d3e5f7a9c0b2d4e6f8a1c3b'],
    attempt: ATTEMPT_ID,
    call_id: 'call-1',
    exit_code: null,
    status: 'succeeded',
    tool: 'write_file',
  }),
  event(RUN_ID, 6, 'check.result', {
    attempt: ATTEMPT_ID,
    evidence_count: 1,
    kind: 'content_matches',
    message: 'out/summary.md matches every required condition',
    sources: ['workspace:out/summary.md'],
    status: 'pass',
    step_index: 1,
  }),
  event(RUN_ID, 7, 'step.passed', {
    attempt: ATTEMPT_ID,
    error: null,
    status: 'passed',
    step_index: 1,
  }),
  event(RUN_ID, 8, 'run.final_verifying', {
    from: 'running',
    reason: null,
    status: 'final_verifying',
  }),
  event(RUN_ID, 9, 'run.finished', { from: 'final_verifying', reason: null, status: 'passed' }),
];

export const RETRIED_EVENTS: RunEvent[] = [
  event(RETRY_RUN_ID, 1, 'run.created', {
    acceptance_hash: ACCEPTANCE_HASH,
    goal: ACCEPTANCE.goal,
    mission: 'desk_task',
    mission_version: 1,
  }),
  event(RETRY_RUN_ID, 2, 'step.failed', {
    attempt: 'aaaabbbbccccddddeeeeffff00001111',
    error: 'out/summary.md does not contain "owner"',
    status: 'retrying',
    step_index: 1,
  }),
  event(RETRY_RUN_ID, 3, 'recovery.decision', {
    step_index: 1,
    strategy: 1,
    decision: { action: 'replan', reason: 'check_failed' },
  }),
  event(RETRY_RUN_ID, 4, 'step.failed', {
    attempt: '22223333444455556666777788889999',
    error: 'step budget exhausted',
    status: 'blocked',
    step_index: 1,
  }),
  event(RETRY_RUN_ID, 5, 'run.finished', {
    from: 'running',
    reason: 'budget_exhausted',
    status: 'incomplete',
  }),
];

export const INTERRUPTED_EVENTS: RunEvent[] = [
  event(INTERRUPTED_RUN_ID, 1, 'run.created', {
    acceptance_hash: ACCEPTANCE_HASH,
    goal: ACCEPTANCE.goal,
    mission: 'desk_task',
    mission_version: 1,
  }),
  event(INTERRUPTED_RUN_ID, 2, 'run.interrupted', {
    status: 'interrupted',
    reason: 'engine exited during attempt execution',
  }),
];

export const SEMANTIC_EVENTS: RunEvent[] = [
  event(RUN_ID, 1, 'check.result', {
    attempt: ATTEMPT_ID,
    confidence: 0.82,
    evidence_count: 2,
    kind: 'semantic_support',
    message: 'The answer names the handover owner and deadline.',
    semantic: true,
    sources: ['workspace:answer.md', 'model:d1-2b'],
    status: 'pass',
    step_index: 1,
  }),
  event(RUN_ID, 2, 'check.result', {
    attempt: ATTEMPT_ID,
    confidence: 0.44,
    evidence_count: 1,
    kind: 'semantic_support',
    message: 'The deadline is only partially supported.',
    semantic: true,
    sources: ['workspace:answer.md'],
    status: 'fail',
    step_index: 1,
  }),
];

/** Engine-shaped semantic event: the store emits basis points and the chain. */
export const SEMANTIC_ENGINE_EVENTS: RunEvent[] = [
  event(RUN_ID, 1, 'check.result', {
    attempt: ATTEMPT_ID,
    confidence_bps: 9300,
    evidence_count: 2,
    kind: 'd1_grounded',
    message: 'Every judged value is supported by the transcript.',
    semantic: true,
    semantic_chain: 'vl->d1',
    semantic_label: 'supported',
    sources: ['input:transcript', 'workspace:out/report.json'],
    status: 'pass',
    step_index: 1,
    threshold_bps: 8000,
  }),
];

export const MODELS: ModelStatus[] = [
  {
    key: 'text',
    model: 'lfm3-1.7b',
    url: 'http://127.0.0.1:8080/v1',
    state: 'ready',
    memory_bytes: 1_603_404 * 1024,
    error: null,
  },
  {
    key: 'vision',
    model: 'lfm3-vl-2b',
    url: 'http://127.0.0.1:8081/v1',
    state: 'absent',
    memory_bytes: 0,
    error: null,
  },
];

export const HEALTH: HealthReport = {
  version: '0.1.0-demo',
  local_only: true,
  data_dir: 'demo://local',
  llama: {
    executable: 'tools/llama/llama-server.exe',
    build: 11960,
    release: '0.8.0-dev',
    pinned: true,
  },
  models: MODELS,
};
