/**
 * Browser fixtures. These never touch a real workspace and cannot be selected
 * in the packaged application; every scenario is deterministic and labelled.
 */

import type {
  AcceptanceSpec,
  HealthReport,
  MissionManifest,
  ModelStatus,
  RunEvent,
  RunRecord,
  RunSnapshot,
} from '../types';

export const ACCEPTANCE_HASH =
  'f7ed5f17bda3c957797569028aab3e375b42998cbf65d489bdc7a9e8e7b6aba5';
export const PASSED_RUN_ID = 'd1e2f3a4b5c6d7e8f90123456789abcd';
export const INCOMPLETE_RUN_ID = 'aa11bb22cc33dd44ee55ff6677889900';
export const INTERRUPTED_RUN_ID = 'bb22cc33dd44ee55ff6677889900aa11';
export const ATTEMPT_ID = '0123456789abcdef0123456789abcdef';
export const SECOND_ATTEMPT_ID = 'fedcba9876543210fedcba9876543210';
export const DIGEST = '3b7c1d9e5f0a4c2b8d6e1f3a5c7b9d0e2f4a6c8b1d3e5f7a9c0b2d4e6f8a1c3b';

export const MISSION: MissionManifest = {
  schema_version: 1,
  name: 'desk_task',
  version: 1,
  title: 'Workspace task',
  description: 'Change approved workspace files and prove every change with a check.',
  inputs: {
    goal: { kind: 'string', required: true, schema: null },
  },
  tools: ['list_files', 'read_file', 'write_file', 'edit_file'],
  checks: [
    'file_exists',
    'dir_exists',
    'content_matches',
    'content_absent',
    'json_schema',
    'numeric_consistency',
    'report_complete',
  ],
  command_templates: [],
  output: {
    schema: null,
    required_artifacts: [{ path: 'out/summary.md', non_empty: true }],
  },
  limits: { max_steps: 8, max_attempts: 3 },
};

export function acceptanceFor(goal: string): AcceptanceSpec {
  return {
    schema_version: 1,
    mission: MISSION.name,
    mission_version: MISSION.version,
    goal,
    inputs: {},
    steps: [
      {
        index: 1,
        title: 'Write the summary',
        criterion: 'out/summary.md exists and contains every required handover fact.',
        checks: [
          { kind: 'file_exists', path: 'out/summary.md' },
          {
            kind: 'content_matches',
            path: 'out/summary.md',
            contains: ['handover', 'owner'],
          },
        ],
        tool: 'write_file',
      },
    ],
    output: MISSION.output,
    allowed_tools: MISSION.tools,
    allowed_checks: MISSION.checks,
    max_steps: MISSION.limits.max_steps,
    max_attempts: MISSION.limits.max_attempts,
  };
}

export const ACCEPTANCE = acceptanceFor(
  'Read notes/handover.md and write a correct summary to out/summary.md.',
);

function record(
  id: string,
  status: RunRecord['status'],
  created_at: number,
  goal: string,
  parent_run_id: string | null = null,
): RunRecord {
  return {
    id,
    mission: MISSION.name,
    mission_version: MISSION.version,
    acceptance_hash: ACCEPTANCE_HASH,
    goal,
    status,
    created_at,
    updated_at: created_at + 1_861,
    parent_run_id,
  };
}

export const PASSED_RUN = record(
  PASSED_RUN_ID,
  'passed',
  1_762_000_000_000,
  ACCEPTANCE.goal,
);

export const INCOMPLETE_RUN = record(
  INCOMPLETE_RUN_ID,
  'incomplete',
  1_762_000_100_000,
  'Write the summary after a denied command approval.',
);

export const INTERRUPTED_RUN = record(
  INTERRUPTED_RUN_ID,
  'interrupted',
  1_762_000_200_000,
  'Write the summary before the engine was stopped.',
);

export function event(
  runId: string,
  seq: number,
  kind: RunEvent['kind'],
  payload: RunEvent['payload'],
  at: number,
): RunEvent {
  return { run_id: runId, seq, kind, payload, at };
}

export const PASSED_SNAPSHOT: RunSnapshot = {
  run: PASSED_RUN,
  steps: [
    {
      index: 1,
      title: 'Write the summary',
      criterion: ACCEPTANCE.steps[0].criterion,
      status: 'passed',
      attempts: [
        {
          id: ATTEMPT_ID,
          strategy: 2,
          status: 'passed',
          action: {
            tool: 'write_file',
            arguments: { path: 'out/summary.md' },
            call_id: 'demo-call-2',
          },
          model: 'lfm3-1.7b',
          started_at: PASSED_RUN.created_at + 900,
          finished_at: PASSED_RUN.updated_at - 4,
          error: null,
        },
      ],
    },
  ],
};

export const PASSED_EVENTS: RunEvent[] = [
  event(PASSED_RUN_ID, 1, 'run.created', {
    acceptance_hash: ACCEPTANCE_HASH,
    goal: PASSED_RUN.goal,
    mission: MISSION.name,
    mission_version: MISSION.version,
  }, PASSED_RUN.created_at),
  event(PASSED_RUN_ID, 2, 'run.status', {
    from: 'created',
    reason: null,
    status: 'running',
  }, PASSED_RUN.created_at + 1),
  event(PASSED_RUN_ID, 3, 'check.result', {
    attempt: ATTEMPT_ID,
    evidence_count: 1,
    kind: 'content_matches',
    message: 'out/summary.md matches every required condition',
    sources: ['workspace:out/summary.md'],
    status: 'pass',
    step_index: 1,
  }, PASSED_RUN.created_at + 900),
  event(PASSED_RUN_ID, 4, 'tool.result', {
    artifacts: [DIGEST],
    attempt: ATTEMPT_ID,
    call_id: 'demo-call-2',
    exit_code: null,
    status: 'succeeded',
    tool: 'write_file',
  }, PASSED_RUN.created_at + 901),
  event(PASSED_RUN_ID, 5, 'step.passed', {
    attempt: ATTEMPT_ID,
    error: null,
    status: 'passed',
    step_index: 1,
  }, PASSED_RUN.created_at + 902),
  event(PASSED_RUN_ID, 6, 'run.final_verifying', {
    from: 'running',
    reason: null,
    status: 'final_verifying',
  }, PASSED_RUN.created_at + 1_800),
  event(PASSED_RUN_ID, 7, 'run.finished', {
    from: 'final_verifying',
    reason: null,
    status: 'passed',
  }, PASSED_RUN.updated_at),
];

export const INCOMPLETE_SNAPSHOT: RunSnapshot = {
  run: INCOMPLETE_RUN,
  steps: [
    {
      index: 1,
      title: 'Write the summary',
      criterion: ACCEPTANCE.steps[0].criterion,
      status: 'blocked',
      attempts: [
        {
          id: SECOND_ATTEMPT_ID,
          strategy: 2,
          status: 'aborted',
          action: {
            tool: 'write_file',
            arguments: { path: 'out/summary.md' },
            call_id: 'demo-call-3',
          },
          model: 'lfm3-1.7b',
          started_at: INCOMPLETE_RUN.created_at + 900,
          finished_at: INCOMPLETE_RUN.created_at + 1_000,
          error: 'approval denied',
        },
      ],
    },
  ],
};

export const INCOMPLETE_EVENTS: RunEvent[] = [
  event(INCOMPLETE_RUN_ID, 1, 'run.created', {
    acceptance_hash: ACCEPTANCE_HASH,
    goal: INCOMPLETE_RUN.goal,
    mission: MISSION.name,
    mission_version: MISSION.version,
  }, INCOMPLETE_RUN.created_at),
  event(INCOMPLETE_RUN_ID, 2, 'step.failed', {
    attempt: SECOND_ATTEMPT_ID,
    error: 'approval denied',
    status: 'aborted',
    step_index: 1,
  }, INCOMPLETE_RUN.created_at + 1_000),
  event(INCOMPLETE_RUN_ID, 3, 'run.finished', {
    from: 'running',
    reason: 'approval_denied',
    status: 'incomplete',
  }, INCOMPLETE_RUN.updated_at),
];

export const INTERRUPTED_SNAPSHOT: RunSnapshot = {
  run: INTERRUPTED_RUN,
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
          action: {
            tool: 'write_file',
            arguments: { path: 'out/summary.md' },
            call_id: 'demo-call-4',
          },
          model: 'lfm3-1.7b',
          started_at: INTERRUPTED_RUN.created_at + 20,
          finished_at: null,
          error: 'engine stopped while the attempt was executing',
        },
      ],
    },
  ],
};

export const INTERRUPTED_EVENTS: RunEvent[] = [
  event(INTERRUPTED_RUN_ID, 1, 'run.created', {
    acceptance_hash: ACCEPTANCE_HASH,
    goal: INTERRUPTED_RUN.goal,
    mission: MISSION.name,
    mission_version: MISSION.version,
  }, INTERRUPTED_RUN.created_at),
  event(INTERRUPTED_RUN_ID, 2, 'run.interrupted', {
    status: 'interrupted',
    reason: 'engine exited during attempt execution',
  }, INTERRUPTED_RUN.updated_at),
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
  {
    key: 'audio',
    model: 'lfm3-audio-2b',
    url: 'http://127.0.0.1:8082/v1',
    state: 'absent',
    memory_bytes: 0,
    error: null,
  },
  {
    key: 'd1',
    model: 'd1-2b',
    url: 'http://127.0.0.1:8083/v1',
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
