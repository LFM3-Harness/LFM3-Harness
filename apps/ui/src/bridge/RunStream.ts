import type { RunReadSource, Unsubscribe } from './EngineBridge';
import { initialRunState, runReducer, type RunAction, type RunState } from './reducer';
import type { RunEvent, RunId } from './types';

/**
 * Keeps one run view in sync with the durable journal.
 *
 * Hydration reads the snapshot and the journal together. Live events are
 * applied only in sequence; a skipped sequence triggers one authoritative
 * snapshot and journal refetch before live application resumes.
 */
export class RunStream {
  private state: RunState = initialRunState;
  private unsubscribe: Unsubscribe | null = null;
  private closed = false;

  constructor(
    private readonly source: RunReadSource,
    private readonly runId: RunId,
    private readonly onChange: (state: RunState) => void,
  ) {}

  get current(): RunState {
    return this.state;
  }

  async start(): Promise<void> {
    this.dispatch({ type: 'loading' });
    try {
      const [snapshot, events] = await Promise.all([
        this.source.getRunSnapshot(this.runId),
        this.source.getRunEvents(this.runId, 0),
      ]);
      this.dispatch({ type: 'hydrate', snapshot, events });
      if (this.closed) {
        return;
      }
      this.unsubscribe = await this.source.subscribe(this.runId, (event) => {
        void this.handleEvent(event);
      });
    } catch (error) {
      this.dispatch({ type: 'error', message: messageOf(error) });
      throw error;
    }
  }

  async handleEvent(event: RunEvent): Promise<void> {
    if (this.closed) {
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
    try {
      const snapshot = await this.source.getRunSnapshot(this.runId);
      const events = await this.source.getRunEvents(this.runId, this.state.lastSeq);
      this.dispatch({ type: 'resync', snapshot, events });
    } catch (error) {
      this.dispatch({ type: 'error', message: messageOf(error) });
    }
  }

  private dispatch(action: RunAction): void {
    const next = runReducer(this.state, action);
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
