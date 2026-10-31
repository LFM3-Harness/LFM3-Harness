import type { JobReadSource, Unsubscribe } from './EngineBridge';
import { initialJobState, jobReducer, type JobAction, type JobState } from './jobReducer';
import type { JobEvent } from './types';

/**
 * Keeps one job's turn timeline in sync with its durable journal.
 *
 * Hydration reads the job view and the journal together. Live events are
 * applied only in sequence; a skipped sequence triggers one authoritative
 * refetch before live application resumes, so a missed turn is never lost.
 */
export class JobStream {
  private state: JobState = initialJobState;
  private unsubscribe: Unsubscribe | null = null;
  private closed = false;
  /** Events that arrived while the journal was being re-read. */
  private pending: JobEvent[] = [];
  private refilling = false;

  constructor(
    private readonly source: JobReadSource,
    private readonly jobId: string,
    private readonly onChange: (state: JobState) => void,
  ) {}

  get current(): JobState {
    return this.state;
  }

  async start(): Promise<void> {
    this.dispatch({ type: 'loading' });
    try {
      const [view, events] = await Promise.all([
        this.source.getJob(this.jobId),
        this.source.getJobEvents(this.jobId, 0),
      ]);
      this.dispatch({ type: 'hydrate', view, events });
      if (this.closed) {
        return;
      }
      this.unsubscribe = await this.source.subscribeJob((event) => {
        void this.handleEvent(event);
      });
    } catch (error) {
      this.dispatch({ type: 'error', message: messageOf(error) });
      throw error;
    }
  }

  async handleEvent(event: JobEvent): Promise<void> {
    if (this.closed) {
      return;
    }
    if (this.refilling) {
      // The refill snapshot may predate this event, so it is applied after the
      // snapshot is adopted rather than being lost to a gap check.
      this.pending.push(event);
      return;
    }
    const before = this.state;
    this.dispatch({ type: 'event', event });
    if (this.state.gap && !before.gap) {
      await this.refill();
    }
  }

  close(): void {
    this.closed = true;
    this.unsubscribe?.();
    this.unsubscribe = null;
  }

  private async refill(): Promise<void> {
    this.refilling = true;
    try {
      const view = await this.source.getJob(this.jobId);
      const events = await this.source.getJobEvents(this.jobId, this.state.lastSeq);
      if (this.closed) {
        return;
      }
      this.dispatch({ type: 'resync', view, events });
    } catch (error) {
      this.dispatch({ type: 'error', message: messageOf(error) });
    } finally {
      this.refilling = false;
    }
    await this.drain();
  }

  /** Applies events held during the refill, refetching again if one still skips. */
  private async drain(): Promise<void> {
    while (this.pending.length > 0 && !this.closed) {
      const next = this.pending
        .filter((event) => event.seq > this.state.lastSeq)
        .sort((left, right) => left.seq - right.seq);
      this.pending = [];
      if (next.length === 0) {
        return;
      }
      for (const event of next) {
        if (this.closed) {
          return;
        }
        const before = this.state;
        this.dispatch({ type: 'event', event });
        if (this.state.gap && !before.gap) {
          await this.refill();
          return;
        }
      }
    }
  }

  private dispatch(action: JobAction): void {
    const next = jobReducer(this.state, action);
    if (next === this.state) {
      return;
    }
    this.state = next;
    this.onChange(next);
  }
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
