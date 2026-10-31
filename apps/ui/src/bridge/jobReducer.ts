import type {
  JobDeliverable,
  JobEvent,
  JobRecord,
  JobTodo,
  JobTodoCheck,
  JobTurn,
  JobView,
} from './types';

export type JobPhase = 'idle' | 'loading' | 'live' | 'recovering' | 'error';

export interface JobState {
  jobId: string | null;
  job: JobRecord | null;
  todos: readonly JobTodo[];
  turns: readonly JobTurn[];
  events: readonly JobEvent[];
  /** Highest applied journal sequence; the next event must be `lastSeq + 1`. */
  lastSeq: number;
  phase: JobPhase;
  gap: boolean;
  error: string | null;
}

export const initialJobState: JobState = {
  jobId: null,
  job: null,
  todos: [],
  turns: [],
  events: [],
  lastSeq: 0,
  phase: 'idle',
  gap: false,
  error: null,
};

export type JobAction =
  | { type: 'loading' }
  | { type: 'hydrate'; view: JobView; events: JobEvent[] }
  | { type: 'resync'; view: JobView; events: JobEvent[] }
  | { type: 'event'; event: JobEvent }
  | { type: 'error'; message: string }
  | { type: 'cancelling' }
  | { type: 'clear' };

/**
 * Applies one durable job event at a time. The turn list and todo list come
 * from the journal, never from assistant prose, and an event that skips a
 * sequence is not applied: the caller refetches the authoritative view.
 */
export function jobReducer(state: JobState, action: JobAction): JobState {
  switch (action.type) {
    case 'loading':
      return { ...state, phase: 'loading', error: null };
    case 'hydrate':
    case 'resync':
      return adopt(state, action.view, action.events);
    case 'event':
      return apply(state, action.event);
    case 'error':
      return { ...state, phase: 'error', error: action.message };
    case 'cancelling':
      return state.job === null
        ? state
        : { ...state, job: { ...state.job, stopped: 'Cancellation requested' } };
    case 'clear':
      return initialJobState;
  }
}

function adopt(state: JobState, view: JobView, events: JobEvent[]): JobState {
  const jobId = view.job.id;
  const merged = mergeEvents(state.events, events, jobId);
  return {
    jobId,
    job: view.job,
    todos: view.todos,
    turns: view.turns,
    events: merged,
    lastSeq: merged.at(-1)?.seq ?? 0,
    phase: 'live',
    gap: false,
    error: null,
  };
}

function apply(state: JobState, event: JobEvent): JobState {
  if (state.job === null || event.job_id !== state.job.id) {
    return state;
  }
  if (event.seq <= state.lastSeq) {
    return state;
  }
  if (event.seq !== state.lastSeq + 1) {
    return { ...state, phase: 'recovering', gap: true };
  }
  const next: JobState = {
    ...state,
    job: state.job,
    events: [...state.events, event],
    lastSeq: event.seq,
    phase: 'live',
    gap: false,
    error: null,
  };
  switch (event.kind) {
    case 'job.started':
      return { ...next, job: { ...state.job, status: 'running' } };
    case 'loop.turn': {
      const turn = readTurn(event.payload);
      if (turn === null) {
        return next;
      }
      const todos = readTodos(event.payload['todos']);
      return {
        ...next,
        turns: [...state.turns, turn],
        todos: todos ?? state.todos,
      };
    }
    case 'loop.todos': {
      const todos = readTodos(event.payload);
      return todos === null ? next : { ...next, todos };
    }
    case 'job.finished': {
      const status = event.payload['status'];
      if (status === 'passed' || status === 'failed' || status === 'cancelled') {
        // The proof and stop reason arrive with the terminal event, so the panel
        // can show what was measured without waiting for a refetch.
        const stopped = event.payload['stopped'];
        const model = event.payload['model'];
        return {
          ...next,
          job: {
            ...state.job,
            status,
            stopped: typeof stopped === 'string' ? stopped : state.job.stopped,
            model: typeof model === 'string' && model.length > 0 ? model : state.job.model,
            deliverable_proof:
              readProof(event.payload['proof']) ?? state.job.deliverable_proof,
            finished_at: event.at,
          },
        };
      }
      return next;
    }
    case 'job.interrupted': {
      const stopped = event.payload['stopped'];
      return {
        ...next,
        job: {
          ...state.job,
          status: 'interrupted',
          stopped: typeof stopped === 'string' ? stopped : state.job.stopped,
          finished_at: event.at,
        },
      };
    }
    default:
      return next;
  }
}

function mergeEvents(
  previous: readonly JobEvent[],
  incoming: readonly JobEvent[],
  jobId: string,
): JobEvent[] {
  const bySeq = new Map<number, JobEvent>();
  for (const entry of [...previous, ...incoming]) {
    if (entry.job_id === jobId) {
      bySeq.set(entry.seq, entry);
    }
  }
  return [...bySeq.values()].sort((left, right) => left.seq - right.seq);
}

/** A malformed turn is dropped rather than rendered as an empty one. */
function readTurn(payload: JobEvent['payload']): JobTurn | null {
  const index = payload['index'];
  if (typeof index !== 'number') {
    return null;
  }
  const note = typeof payload['note'] === 'string' ? payload['note'] : '';
  const phase = typeof payload['phase'] === 'string' ? payload['phase'] : undefined;
  const rawCalls = payload['calls'];
  const calls = Array.isArray(rawCalls)
    ? rawCalls.flatMap((entry) => {
        if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
          return [];
        }
        const tool = entry['tool'];
        if (typeof tool !== 'string') {
          return [];
        }
        return [
          {
            tool,
            arguments: entry['arguments'] ?? null,
            ok: entry['ok'] === true,
            output: typeof entry['output'] === 'string' ? entry['output'] : '',
          },
        ];
      })
    : [];
  return { index, phase, note, calls };
}

function readProof(value: unknown): JobDeliverable | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return null;
  }
  const record = value as Record<string, unknown>;
  const path = record['path'];
  const bytes = record['bytes'];
  const digest = record['digest'];
  if (typeof path !== 'string' || typeof bytes !== 'number' || typeof digest !== 'string') {
    return null;
  }
  return { path, bytes, digest };
}

function readTodos(value: unknown): JobTodo[] | null {
  if (!Array.isArray(value)) {
    return null;
  }
  return value.flatMap((entry) => {
    if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
      return [];
    }
    const text = entry['text'];
    if (typeof text !== 'string' || text.trim().length === 0) {
      return [];
    }
    return [{ text, done: entry['done'] === true, check: readTodoCheck(entry['check']) }];
  });
}

function readTodoCheck(value: unknown): JobTodoCheck | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return null;
  }
  const record = value as Record<string, unknown>;
  const kind = record['kind'];
  const path = record['path'];
  if (
    (kind !== 'file_exists' && kind !== 'dir_exists' && kind !== 'content_matches') ||
    typeof path !== 'string' ||
    path.length === 0
  ) {
    return null;
  }
  const contains = typeof record['contains'] === 'string' ? record['contains'] : null;
  return { kind, path, contains };
}
