import { describe, expect, it } from 'vitest';

import type { RunEvent, RunEventKind, RunId, RunSnapshot, StepStatus } from './types';
import { initialRunState, runReducer } from './reducer';
import { RunStream } from './RunStream';
import type { RunReadSource } from './EngineBridge';

const RUN_ID: RunId = 'a'.repeat(32);

function snapshot(
  status: RunSnapshot['run']['status'] = 'running',
  stepStatus: StepStatus = 'running',
): RunSnapshot {
  return {
    run: {
      id: RUN_ID,
      mission: 'desk_task',
      mission_version: 1,
      acceptance_hash: 'f'.repeat(64),
      goal: 'Write a verified summary.',
      status,
      created_at: 1,
      updated_at: 1,
      parent_run_id: null,
    },
    steps: [
      {
        index: 1,
        title: 'Write the summary',
        criterion: 'out/summary.md contains the required facts',
        status: stepStatus,
        attempts: [],
      },
    ],
  };
}

function event(
  seq: number,
  kind: RunEventKind,
  payload: RunEvent['payload'] = {},
  runId: RunId = RUN_ID,
): RunEvent {
  return { run_id: runId, seq, kind, payload, at: 1_000 + seq };
}

function hydrate(events: RunEvent[] = []) {
  return runReducer(initialRunState, {
    type: 'hydrate',
    snapshot: snapshot(),
    events,
  });
}

describe('run reducer', () => {
  it('hydrates a snapshot with its existing ordered journal', () => {
    const first = event(1, 'run.created');
    const second = event(2, 'run.status', { status: 'running' });

    const state = hydrate([second, first]);

    expect(state.phase).toBe('live');
    expect(state.snapshot?.run.id).toBe(RUN_ID);
    expect(state.events.map((entry) => entry.seq)).toEqual([1, 2]);
    expect(state.lastSeq).toBe(2);
    expect(state.gap).toBe(false);
  });

  it('applies only the next sequence and updates run and step state', () => {
    let state = hydrate([event(1, 'run.created')]);

    state = runReducer(state, {
      type: 'event',
      event: event(2, 'run.status', { status: 'running' }),
    });
    state = runReducer(state, {
      type: 'event',
      event: event(3, 'step.passed', { step_index: 1 }),
    });
    state = runReducer(state, {
      type: 'event',
      event: event(4, 'run.finished', { status: 'passed' }),
    });

    expect(state.events.map((entry) => entry.seq)).toEqual([1, 2, 3, 4]);
    expect(state.lastSeq).toBe(4);
    expect(state.snapshot?.run.status).toBe('passed');
    expect(state.snapshot?.steps[0]?.status).toBe('passed');
    expect(state.phase).toBe('live');
  });

  it('suppresses duplicate and already-applied sequences', () => {
    const state = hydrate([event(1, 'run.created'), event(2, 'run.status')]);

    const unchanged = runReducer(state, {
      type: 'event',
      event: event(2, 'run.status', { status: 'running' }),
    });

    expect(unchanged).toBe(state);
    expect(unchanged.events).toHaveLength(2);
  });

  it('ignores events from a different run', () => {
    const state = hydrate([event(1, 'run.created')]);

    const unchanged = runReducer(state, {
      type: 'event',
      event: event(2, 'run.status', { status: 'passed' }, 'b'.repeat(32)),
    });

    expect(unchanged).toBe(state);
  });

  it('detects a missing sequence without applying later events', () => {
    const state = hydrate([event(1, 'run.created'), event(2, 'run.status')]);

    const gapped = runReducer(state, {
      type: 'event',
      event: event(4, 'run.finished', { status: 'passed' }),
    });

    expect(gapped.gap).toBe(true);
    expect(gapped.phase).toBe('recovering');
    expect(gapped.lastSeq).toBe(2);
    expect(gapped.events.map((entry) => entry.seq)).toEqual([1, 2]);
    expect(gapped.snapshot?.run.status).toBe('running');
  });

  it('resynchronizes missing events after a gap', () => {
    const gapped = runReducer(
      hydrate([event(1, 'run.created'), event(2, 'run.status')]),
      { type: 'event', event: event(4, 'run.finished', { status: 'passed' }) },
    );

    const resynced = runReducer(gapped, {
      type: 'resync',
      snapshot: snapshot('passed', 'passed'),
      events: [event(4, 'run.finished', { status: 'passed' }), event(3, 'step.passed')],
    });

    expect(resynced.gap).toBe(false);
    expect(resynced.phase).toBe('live');
    expect(resynced.lastSeq).toBe(4);
    expect(resynced.events.map((entry) => entry.seq)).toEqual([1, 2, 3, 4]);
    expect(resynced.snapshot?.run.status).toBe('passed');
  });
});

describe('run stream', () => {
  it('refetches the snapshot and missing events when a sequence is skipped', async () => {
    const calls: Array<{ snapshot: number; after: number[] }> = [];
    let listener: ((entry: RunEvent) => void) | null = null;
    let snapshotReads = 0;
    const source: RunReadSource = {
      async getRunSnapshot() {
        calls.push({ snapshot: 1, after: [] });
        snapshotReads += 1;
        return snapshotReads === 1 ? snapshot() : snapshot('passed', 'passed');
      },
      async getRunEvents(_runId, from) {
        calls.push({ snapshot: 0, after: [from] });
        if (from === 0) {
          return [event(1, 'run.created'), event(2, 'run.status')];
        }
        return [event(3, 'step.passed'), event(4, 'run.finished', { status: 'passed' })];
      },
      async subscribe(_runId, next) {
        listener = next;
        return () => {
          listener = null;
        };
      },
    };
    const states: number[] = [];
    const stream = new RunStream(source, RUN_ID, (state) => states.push(state.lastSeq));

    await stream.start();
    expect(stream.current.lastSeq).toBe(2);

    await stream.handleEvent(event(4, 'run.finished', { status: 'passed' }));

    expect(calls.filter((call) => call.snapshot === 1)).toHaveLength(2);
    expect(calls.at(-1)?.after).toEqual([2]);
    expect(stream.current.lastSeq).toBe(4);
    expect(stream.current.gap).toBe(false);
    expect(stream.current.snapshot?.run.status).toBe('passed');
    expect(states.at(-1)).toBe(4);
    stream.close();
    expect(listener).toBeNull();
  });
});
