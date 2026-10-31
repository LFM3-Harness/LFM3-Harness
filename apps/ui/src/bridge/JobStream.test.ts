import { describe, expect, it } from 'vitest';

import { JobStream } from './JobStream';
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

function event(seq: number, kind: JobEvent['kind'], payload: JobEvent['payload']): JobEvent {
  return { job_id: JOB.id, seq, kind, payload, at: seq };
}

const FINISHED = event(4, 'job.finished', {
  status: 'passed',
  stopped: 'the required output verified',
  model: 'lfm3-1.7b',
  proof: { path: 'out/report.md', bytes: 42, digest: 'c'.repeat(64) },
});

/** The view as the journal described it when the read began. */
function viewOf(events: readonly JobEvent[]): JobView {
  const finished = [...events].reverse().find((entry) => entry.kind === 'job.finished');
  const status = finished?.payload['status'];
  const stopped = finished?.payload['stopped'];
  const proof = finished?.payload['proof'];
  return {
    job: {
      ...JOB,
      status: status === 'passed' || status === 'failed' || status === 'cancelled' ? status : 'running',
      stopped: typeof stopped === 'string' ? stopped : '',
      deliverable_proof:
        proof !== null && typeof proof === 'object'
          ? (proof as unknown as JobRecord['deliverable_proof'])
          : null,
    },
    todos: [],
    turns: events
      .filter((entry) => entry.kind === 'loop.turn')
      .map((entry) => ({
        index: Number(entry.payload['index'] ?? 0),
        note: '',
        calls: [],
      })),
  };
}

function deferred(): { promise: Promise<void>; release: () => void } {
  let release: () => void = () => undefined;
  const promise = new Promise<void>((resolve) => {
    release = () => {
      resolve();
    };
  });
  return { promise, release };
}

describe('JobStream', () => {
  it('applies an event that lands while the refill is still reading', async () => {
    const journal: JobEvent[] = [event(1, 'job.started', {})];
    let snapshot: JobEvent[] = [...journal];
    const gate = deferred();
    let gated = false;

    async function waitForGate(): Promise<void> {
      if (gated) {
        await gate.promise;
      }
    }

    const source = {
      getJob: async () => {
        await waitForGate();
        return viewOf(snapshot);
      },
      getJobEvents: async (_jobId: string, after: number) => {
        await waitForGate();
        return snapshot.filter((entry) => entry.seq > after);
      },
      subscribeJob: async () => () => undefined,
    };

    const stream = new JobStream(source, JOB.id, () => undefined);
    await stream.start();
    expect(stream.current.lastSeq).toBe(1);

    // A skipped turn starts the refill, which reads a snapshot that cannot yet
    // contain what is about to arrive on the live channel.
    gated = true;
    journal.push(event(2, 'loop.turn', { index: 1, note: '', calls: [] }));
    journal.push(event(3, 'loop.turn', { index: 2, note: '', calls: [] }));
    snapshot = [...journal];
    const refilling = stream.handleEvent(event(3, 'loop.turn', { index: 2, note: '', calls: [] }));

    journal.push(FINISHED);
    await stream.handleEvent(FINISHED);

    gate.release();
    await refilling;

    expect(stream.current.lastSeq).toBe(4);
    expect(stream.current.phase).toBe('live');
    expect(stream.current.turns).toHaveLength(2);
    expect(stream.current.job?.status).toBe('passed');
    expect(stream.current.job?.deliverable_proof?.bytes).toBe(42);
    stream.close();
  });
});
