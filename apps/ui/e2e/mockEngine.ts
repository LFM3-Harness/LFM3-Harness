import type {
  JobCall,
  JobDeliverable,
  JobEvent,
  JobRecord,
  JobTodo,
  JobTurn,
  TaskRoute,
} from '../src/bridge/types';

export type MockScenario = 'pass' | 'stop' | 'gap';

export interface MockEngineOptions {
  scenario: MockScenario;
  health: unknown;
  mission: unknown;
  interrupted: { job: JobRecord; events: JobEvent[] };
}

type JsonRecord = Record<string, unknown>;

interface MockJob {
  job: JobRecord;
  events: JobEvent[];
  todos: JobTodo[];
  turns: JobTurn[];
}

/**
 * Installed before the app loads, this stands in for the Tauri IPC layer only:
 * `invoke` answers the engine commands and `plugin:event|listen` delivers job
 * events through the same channel the real shell emits on. The React surface,
 * the bridge, and the journal reducer all run unmodified, so a Playwright test
 * sees exactly what the desktop app renders.
 */
export function installMockJobEngine(options: MockEngineOptions): void {
  const scope = globalThis as unknown as {
    __TAURI_INTERNALS__?: {
      invoke: (cmd: string, args?: JsonRecord) => unknown;
      transformCallback: (callback: (value: unknown) => void, once?: boolean) => number;
    };
  };

  // Everything the engine answers with lives inside this function: Playwright
  // serialises it into the page, so module-scope constants would not exist there.
  const ROUTE: TaskRoute = {
    roles: [
      { role: 'plan', model: 'lfm3-1.7b', capability: 'text', used: true, reason: null },
      { role: 'act', model: 'lfm3-1.7b', capability: 'text', used: true, reason: null },
      { role: 'observe', model: 'lfm3-vl-2b', capability: 'vision', used: false, reason: null },
    ],
    images: [],
    audio: [],
    tools: ['list_files', 'read_file', 'write_file', 'edit_file'],
  };

  const OPEN_TODOS: JobTodo[] = [
    { text: 'read both notes', done: false },
    { text: 'write the report', done: false },
  ];

  const READ_TODOS: JobTodo[] = [
    { text: 'read both notes', done: true },
    { text: 'write the report', done: false },
  ];

  const DONE_TODOS: JobTodo[] = [
    { text: 'read both notes', done: true },
    { text: 'write the report', done: true },
  ];

  const PROOF_BYTES = 512;
  const PROOF_DIGEST = 'a'.repeat(64);

  const callbacks = new Map<number, (value: unknown) => void>();
  const handlers = new Map<number, string>();
  const listening = new Map<number, number>();
  const jobs = new Map<string, MockJob>();
  const timers: number[] = [];
  let callbackId = 1;
  let listenId = 1;
  let eventId = 1;
  let slowReads = 0;

  jobs.set(options.interrupted.job.id, {
    job: options.interrupted.job,
    events: [...options.interrupted.events],
    todos: [],
    turns: [],
  });

  function later(delay: number, run: () => void): void {
    timers.push(
      globalThis.setTimeout(() => {
        run();
      }, delay),
    );
  }

  function broadcast(event: JobEvent): void {
    for (const [id, name] of handlers) {
      if (name !== 'lfm3:job-event') {
        continue;
      }
      callbacks.get(id)?.({ event: name, id: eventId++, payload: event });
    }
  }

  function apply(entry: MockJob, event: JobEvent): void {
    switch (event.kind) {
      case 'job.started':
        entry.job = { ...entry.job, status: 'running', updated_at: event.at };
        break;
      case 'loop.turn': {
        const payload = event.payload;
        const calls = Array.isArray(payload['calls']) ? (payload['calls'] as JobCall[]) : [];
        entry.turns.push({
          index: Number(payload['index'] ?? entry.turns.length + 1),
          note: typeof payload['note'] === 'string' ? payload['note'] : '',
          calls,
        });
        const todos = payload['todos'];
        if (Array.isArray(todos)) {
          entry.todos = todos as JobTodo[];
        }
        break;
      }
      case 'job.finished': {
        const payload = event.payload;
        entry.job = {
          ...entry.job,
          status: payload['status'] as JobRecord['status'],
          stopped: typeof payload['stopped'] === 'string' ? payload['stopped'] : '',
          model: typeof payload['model'] === 'string' ? payload['model'] : entry.job.model,
          deliverable_proof:
            (payload['proof'] as JobDeliverable | null | undefined) ??
            entry.job.deliverable_proof,
          updated_at: event.at,
          finished_at: event.at,
        };
        break;
      }
      case 'job.interrupted':
        entry.job = { ...entry.job, status: 'interrupted', updated_at: event.at };
        break;
      default:
        break;
    }
  }

  function record(jobId: string, kind: string, payload: JsonRecord, live = true): void {
    const entry = jobs.get(jobId);
    if (entry === undefined) {
      return;
    }
    const event: JobEvent = {
      job_id: jobId,
      seq: entry.events.length + 1,
      kind,
      payload,
      at: Date.now(),
    };
    entry.events.push(event);
    apply(entry, event);
    if (live) {
      broadcast(event);
    }
  }

  function view(jobId: string): { job: JobRecord; todos: JobTodo[]; turns: JobTurn[] } {
    const entry = jobs.get(jobId);
    if (entry === undefined) {
      throw new Error(`unknown job ${jobId}`);
    }
    return {
      job: { ...entry.job },
      todos: entry.todos.map((todo) => ({ ...todo })),
      turns: entry.turns.map((turn) => ({ ...turn, calls: turn.calls.map((call) => ({ ...call })) })),
    };
  }

  function finishPassed(jobId: string, output: string): void {
    record(jobId, 'job.finished', {
      status: 'passed',
      stopped: 'the required output verified',
      model: 'lfm3-1.7b',
      proof: { path: output, bytes: PROOF_BYTES, digest: PROOF_DIGEST },
    });
  }

  function schedule(jobId: string, output: string): void {
    const scenario = options.scenario;
    later(80, () => {
      record(jobId, 'job.started', { goal: jobs.get(jobId)?.job.goal ?? '' });
    });
    later(200, () => {
      record(jobId, 'loop.turn', {
        index: 1,
        note: '',
        calls: [
          { tool: 'list_files', arguments: {}, ok: true, output: 'notes/a.md\nnotes/b.md' },
          {
            tool: 'todo_write',
            arguments: { items: OPEN_TODOS },
            ok: true,
            output: '2 todos written',
          },
        ],
        todos: OPEN_TODOS,
      });
    });
    later(340, () => {
      record(jobId, 'loop.turn', {
        index: 2,
        note: '',
        calls: [
          { tool: 'read_file', arguments: { path: 'notes/a.md' }, ok: true, output: 'Revenue: 42' },
          { tool: 'read_file', arguments: { path: 'notes/b.md' }, ok: true, output: 'Costs: 17' },
        ],
        todos: READ_TODOS,
      });
    });

    if (scenario === 'stop') {
      return;
    }

    if (scenario === 'gap') {
      // The second turn lands in the journal but never reaches the live channel,
      // so the next sequence number is skipped. The UI must refetch, not drop it.
      later(420, () => {
        record(
          jobId,
          'loop.turn',
          {
            index: 3,
            note: 'recording the intermediate read without a live frame',
            calls: [{ tool: 'read_file', arguments: { path: 'notes/a.md' }, ok: true, output: 'again' }],
            todos: READ_TODOS,
          },
          false,
        );
      });
      later(480, () => {
        slowReads = 2;
        record(jobId, 'loop.turn', {
          index: 4,
          note: '',
          calls: [
            {
              tool: 'write_file',
              arguments: { path: output, content: '# Report\nRevenue: 42\nCosts: 17' },
              ok: true,
              output: `wrote 31 bytes to ${output}`,
            },
          ],
          todos: DONE_TODOS,
        });
      });
      later(1400, () => finishPassed(jobId, output));
      return;
    }

    later(460, () => {
      record(jobId, 'loop.turn', {
        index: 3,
        note: '',
        calls: [
          {
            tool: 'write_file',
            arguments: { path: output, content: '# Report\nRevenue: 42\nCosts: 17' },
            ok: true,
            output: `wrote 31 bytes to ${output}`,
          },
        ],
        todos: DONE_TODOS,
      });
    });
    later(640, () => finishPassed(jobId, output));
  }

  function startJob(args: JsonRecord): JsonRecord {
    const goal = String(args['goal'] ?? '');
    const deliverable = typeof args['deliverable'] === 'string' ? args['deliverable'] : null;
    const minutes = Number(args['minutes'] ?? 30);
    const maxTurns = Number(args['maxTurns'] ?? 24);
    const id = `job-${String(jobs.size + 1).padStart(2, '0')}`;
    const at = Date.now();
    const job: JobRecord = {
      id,
      goal,
      deliverable,
      status: 'running',
      stopped: '',
      model: 'lfm3-1.7b',
      max_turns: maxTurns,
      minutes,
      deliverable_proof: null,
      created_at: at,
      updated_at: at,
      finished_at: null,
    };
    jobs.set(id, { job, events: [], todos: [], turns: [] });
    schedule(id, deliverable ?? 'out/report.md');
    return { job_id: id, job, route: ROUTE };
  }

  function cancelJob(jobId: string): void {
    const entry = jobs.get(jobId);
    if (entry === undefined || entry.job.status !== 'running') {
      return;
    }
    for (const timer of timers.splice(0)) {
      globalThis.clearTimeout(timer);
    }
    record(jobId, 'job.finished', {
      status: 'cancelled',
      stopped: 'the operator stopped the job',
      model: 'lfm3-1.7b',
      proof: null,
    });
  }

  async function settle<T>(value: T): Promise<T> {
    if (slowReads > 0) {
      slowReads -= 1;
      await new Promise((resolve) => globalThis.setTimeout(resolve, 600));
    }
    return value;
  }

  async function invoke(command: string, args: JsonRecord = {}): Promise<unknown> {
    switch (command) {
      case 'health':
        return options.health;
      case 'list_missions':
        return [options.mission];
      case 'list_runs':
      case 'list_chat_sessions':
        return [];
      case 'list_models':
        return (options.health as { models: unknown }).models;
      case 'list_jobs':
        return [...jobs.values()]
          .map((entry) => ({ ...entry.job }))
          .sort((left, right) => right.created_at - left.created_at);
      case 'get_job':
        return settle(view(String(args['jobId'])));
      case 'get_job_events': {
        const after = Number(args['after'] ?? 0);
        const events = jobs
          .get(String(args['jobId']))
          ?.events.filter((event) => event.seq > after);
        return settle(events?.map((event) => ({ ...event })) ?? []);
      }
      case 'start_job':
        return startJob(args);
      case 'cancel_job':
        cancelJob(String(args['jobId']));
        return null;
      case 'plugin:event|listen': {
        const handler = Number(args['handler']);
        const name = String(args['event']);
        handlers.set(handler, name);
        const id = listenId++;
        listening.set(id, handler);
        return id;
      }
      case 'plugin:event|unlisten': {
        const id = Number(args['eventId']);
        const handler = listening.get(id);
        if (handler !== undefined) {
          handlers.delete(handler);
          listening.delete(id);
        }
        return null;
      }
      default:
        throw new Error(`the mock engine does not implement ${command}`);
    }
  }

  scope.__TAURI_INTERNALS__ = {
    invoke: (command: string, args?: JsonRecord) => invoke(command, args),
    transformCallback: (callback, once = false) => {
      const id = callbackId++;
      callbacks.set(id, once ? (value) => {
        callbacks.delete(id);
        callback(value);
      } : callback);
      return id;
    },
  };
}
