import { describe, expect, it } from 'vitest';

import { initialJobState, jobReducer } from './jobReducer';
import type { JobEvent, JobRecord, JobView } from './types';

const JOB: JobRecord = {
  id: 'job-1',
  goal: 'Summarise the notes',
  deliverable: 'out/report.md',
  status: 'running',
  stopped: '',
  model: 'lfm3-1.7b',
  max_turns: 12,
  minutes: 30,
  deliverable_proof: null,
  created_at: 1,
  updated_at: 1,
  finished_at: null,
};

const VIEW: JobView = { job: JOB, todos: [], turns: [] };

function event(seq: number, kind: JobEvent['kind'], payload: JobEvent['payload']): JobEvent {
  return { job_id: JOB.id, seq, kind, payload, at: seq };
}

describe('jobReducer', () => {
  it('hydrates the job, todos and turns from the authoritative view', () => {
    const state = jobReducer(initialJobState, {
      type: 'hydrate',
      view: {
        job: JOB,
        todos: [{ text: 'read the notes', done: false }],
        turns: [{ index: 1, note: '', calls: [] }],
      },
      events: [event(1, 'job.started', { goal: JOB.goal })],
    });

    expect(state.phase).toBe('live');
    expect(state.job?.id).toBe(JOB.id);
    expect(state.todos).toHaveLength(1);
    expect(state.turns).toHaveLength(1);
    expect(state.lastSeq).toBe(1);
  });

  it('appends turns and adopts the model todo list they carry', () => {
    const hydrated = jobReducer(initialJobState, {
      type: 'hydrate',
      view: VIEW,
      events: [event(1, 'job.started', {})],
    });

    const next = jobReducer(hydrated, {
      type: 'event',
      event: event(2, 'loop.turn', {
        index: 1,
        note: '',
        calls: [{ tool: 'read_file', arguments: { path: 'notes/a.md' }, ok: true, output: 'hi' }],
        todos: [{ text: 'read the notes', done: true }],
      }),
    });

    expect(next.turns).toHaveLength(1);
    expect(next.turns[0].calls[0].tool).toBe('read_file');
    expect(next.todos).toEqual([{ text: 'read the notes', done: true }]);
    expect(next.lastSeq).toBe(2);
  });

  it('records a finished status without inventing a pass', () => {
    const hydrated = jobReducer(initialJobState, {
      type: 'hydrate',
      view: VIEW,
      events: [event(1, 'job.started', {})],
    });

    const next = jobReducer(hydrated, {
      type: 'event',
      event: event(2, 'job.finished', { status: 'failed', stopped: 'turn budget' }),
    });

    expect(next.job?.status).toBe('failed');
  });

  it('adopts the stop reason and measured proof from the terminal event', () => {
    const hydrated = jobReducer(initialJobState, {
      type: 'hydrate',
      view: VIEW,
      events: [event(1, 'job.started', {})],
    });

    const next = jobReducer(hydrated, {
      type: 'event',
      event: event(2, 'job.finished', {
        status: 'passed',
        stopped: 'the required output verified',
        model: 'lfm3-1.7b',
        proof: { path: 'out/report.md', bytes: 42, digest: 'b'.repeat(64) },
      }),
    });

    expect(next.job?.status).toBe('passed');
    expect(next.job?.stopped).toBe('the required output verified');
    expect(next.job?.deliverable_proof?.bytes).toBe(42);
    expect(next.job?.finished_at).toBe(2);
  });

  it('stops applying events that skip a sequence and asks for a refetch', () => {
    const hydrated = jobReducer(initialJobState, {
      type: 'hydrate',
      view: VIEW,
      events: [event(1, 'job.started', {})],
    });

    const next = jobReducer(hydrated, {
      type: 'event',
      event: event(4, 'loop.turn', { index: 2, note: '', calls: [] }),
    });

    expect(next.phase).toBe('recovering');
    expect(next.gap).toBe(true);
    expect(next.turns).toHaveLength(0);
    expect(next.lastSeq).toBe(1);
  });

  it('ignores events for another job', () => {
    const hydrated = jobReducer(initialJobState, {
      type: 'hydrate',
      view: VIEW,
      events: [],
    });

    const next = jobReducer(hydrated, {
      type: 'event',
      event: { ...event(1, 'job.started', {}), job_id: 'other' },
    });

    expect(next).toBe(hydrated);
  });
});
