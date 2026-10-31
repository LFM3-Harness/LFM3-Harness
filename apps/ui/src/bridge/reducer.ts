import {
  ATTEMPT_STATUSES,
  RUN_STATUSES,
  type AttemptSnapshot,
  type AttemptStatus,
  type RunEvent,
  type RunSnapshot,
  type RunStatus,
  type StepStatus,
} from './types';

export type RunPhase = 'idle' | 'loading' | 'live' | 'recovering' | 'error';

export interface RunState {
  runId: string | null;
  snapshot: RunSnapshot | null;
  events: readonly RunEvent[];
  /** Highest applied journal sequence; the next event must be `lastSeq + 1`. */
  lastSeq: number;
  phase: RunPhase;
  gap: boolean;
  error: string | null;
}

export const initialRunState: RunState = {
  runId: null,
  snapshot: null,
  events: [],
  lastSeq: 0,
  phase: 'idle',
  gap: false,
  error: null,
};

export type RunAction =
  | { type: 'loading' }
  | { type: 'hydrate'; snapshot: RunSnapshot; events: RunEvent[] }
  | { type: 'resync'; snapshot: RunSnapshot; events: RunEvent[] }
  | { type: 'event'; event: RunEvent }
  | { type: 'error'; message: string }
  | { type: 'clear' };

/**
 * Applies one durable event at a time. An event that skips a sequence is not
 * applied: the state becomes `recovering` and the caller refetches the
 * authoritative snapshot plus the missing journal entries.
 */
export function runReducer(state: RunState, action: RunAction): RunState {
  switch (action.type) {
    case 'loading':
      return { ...state, phase: 'loading', error: null };
    case 'hydrate':
    case 'resync':
      return adopt(state, action.snapshot, action.events);
    case 'event':
      return apply(state, action.event);
    case 'error':
      return { ...state, phase: 'error', error: action.message };
    case 'clear':
      return initialRunState;
  }
}

function adopt(state: RunState, snapshot: RunSnapshot, events: RunEvent[]): RunState {
  const runId = snapshot.run.id;
  const merged = mergeEvents(state.events, events, runId);
  return {
    runId,
    snapshot,
    events: merged,
    lastSeq: merged.at(-1)?.seq ?? 0,
    phase: 'live',
    gap: false,
    error: null,
  };
}

function apply(state: RunState, event: RunEvent): RunState {
  if (state.snapshot === null || event.run_id !== state.snapshot.run.id) {
    return state;
  }
  if (event.seq <= state.lastSeq) {
    return state;
  }
  if (event.seq !== state.lastSeq + 1) {
    return { ...state, phase: 'recovering', gap: true };
  }
  return {
    ...state,
    snapshot: evolve(state.snapshot, event),
    events: [...state.events, event],
    lastSeq: event.seq,
    phase: 'live',
    gap: false,
    error: null,
  };
}

function mergeEvents(
  previous: readonly RunEvent[],
  incoming: readonly RunEvent[],
  runId: string,
): RunEvent[] {
  const bySeq = new Map<number, RunEvent>();
  for (const entry of [...previous, ...incoming]) {
    if (entry.run_id === runId) {
      bySeq.set(entry.seq, entry);
    }
  }
  return [...bySeq.values()].sort((left, right) => left.seq - right.seq);
}

function evolve(snapshot: RunSnapshot, event: RunEvent): RunSnapshot {
  switch (event.kind) {
    case 'run.created':
    case 'run.started':
    case 'run.status':
    case 'run.cancelling':
    case 'run.finished':
    case 'run.final_verifying':
    case 'run.interrupted': {
      const status = event.payload['status'];
      if (typeof status === 'string' && isRunStatus(status)) {
        return {
          ...snapshot,
          run: { ...snapshot.run, status, updated_at: event.at },
        };
      }
      return snapshot;
    }
    case 'attempt.started':
      return startAttempt(snapshot, event);
    case 'step.started':
    case 'step.replanned':
    case 'step.passed':
    case 'step.failed':
    case 'step.inconclusive':
    case 'step.blocked': {
      const index = event.payload['step_index'];
      if (typeof index !== 'number') {
        return snapshot;
      }
      const status: StepStatus =
        event.kind === 'step.passed'
          ? 'passed'
          : event.kind === 'step.started'
            ? 'running'
            : event.kind === 'step.replanned' || event.kind === 'step.inconclusive'
              ? 'replanning'
              : event.kind === 'step.blocked'
                ? 'blocked'
                : 'retrying';
      const steps = snapshot.steps.map((step) =>
        step.index === index
          ? {
              ...step,
              status,
              attempts: finishAttempt(step.attempts, event),
            }
          : step,
      );
      return { ...snapshot, steps };
    }
    default:
      return snapshot;
  }
}

function startAttempt(snapshot: RunSnapshot, event: RunEvent): RunSnapshot {
  const id = event.payload['attempt'];
  const stepIndex = event.payload['step_index'];
  if (typeof id !== 'string' || typeof stepIndex !== 'number') {
    return snapshot;
  }
  const strategy = event.payload['strategy'];
  const attempt: AttemptSnapshot = {
    id,
    strategy: typeof strategy === 'number' ? strategy : 1,
    status: 'executing',
    action: event.payload['action'] ?? null,
    model: '',
    started_at: event.at,
    finished_at: null,
    error: null,
  };
  return {
    ...snapshot,
    steps: snapshot.steps.map((step) =>
      step.index === stepIndex && !step.attempts.some((entry) => entry.id === id)
        ? { ...step, status: 'running', attempts: [...step.attempts, attempt] }
        : step,
    ),
  };
}

function finishAttempt(attempts: AttemptSnapshot[], event: RunEvent): AttemptSnapshot[] {
  const id = event.payload['attempt'];
  const raw = event.payload['status'];
  if (typeof id !== 'string' || typeof raw !== 'string' || !isAttemptStatus(raw)) {
    return attempts;
  }
  const error = event.payload['error'];
  return attempts.map((attempt) =>
    attempt.id === id
      ? {
          ...attempt,
          status: raw,
          finished_at: event.at,
          error: typeof error === 'string' ? error : null,
        }
      : attempt,
  );
}

function isRunStatus(value: string): value is RunStatus {
  return (RUN_STATUSES as readonly string[]).includes(value);
}

function isAttemptStatus(value: string): value is AttemptStatus {
  return (ATTEMPT_STATUSES as readonly string[]).includes(value);
}
