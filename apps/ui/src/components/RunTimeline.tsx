import type {
  AttemptSnapshot,
  RunEvent,
  RunSnapshot,
  StepSnapshot,
} from '../bridge/types';
import { isTerminal, statusLabel } from './format';

export interface RunTimelineProps {
  snapshot: RunSnapshot;
  events: readonly RunEvent[];
  onCancel(runId: string): void;
  onRetry?(runId: string): void;
  cancelling?: boolean;
  retrying?: boolean;
}

/**
 * Steps, attempts, and the recovery trail. An attempt is only ever shown as
 * passed when its durable status says so; retries and interrupted attempts
 * remain visible with their evidence.
 */
export function RunTimeline({
  snapshot,
  events,
  onCancel,
  onRetry,
  cancelling = false,
  retrying = false,
}: RunTimelineProps) {
  const status = snapshot.run.status;
  const terminal = isTerminal(status);
  const interrupted = status === 'interrupted';
  const stopReason = lastFinishReason(events);
  const interruption = events.find((event) => event.kind === 'run.interrupted');
  const decisions = events.filter((event) => event.kind === 'recovery.decision');

  return (
    <section className="panel" aria-labelledby="timeline-title">
      <div className="panel-heading">
        <div>
          <h2 id="timeline-title">Run timeline</h2>
          <p className="mono muted">
            {snapshot.run.mission} v{snapshot.run.mission_version} · {snapshot.run.id}
          </p>
        </div>
        <span className={`status status-${status}`}>{statusLabel(status)}</span>
      </div>

      {interrupted ? (
        <p className="notice notice-bad">
          Interrupted — not a success. The engine did not silently resume this run; review the
          attempts and start a new linked run.
        </p>
      ) : null}

      {interrupted && interruption ? (
        <p className="mono muted">{stringField(interruption.payload, 'reason')}</p>
      ) : null}

      {terminal && stopReason ? (
        <p className="mono muted">Stop reason: {stopReason}</p>
      ) : null}

      <ol className="timeline">
        {snapshot.steps.map((step) => (
          <StepRow key={step.index} step={step} />
        ))}
      </ol>

      {decisions.length > 0 ? (
        <div className="recovery">
          <h3 className="subhead">Recovery decisions</h3>
          <ul>
            {decisions.map((event) => (
              <li key={event.seq}>
                Recovery: {decisionLabel(event.payload)} at step{' '}
                {numberField(event.payload, 'step_index') ?? '?'} (strategy{' '}
                {numberField(event.payload, 'strategy') ?? '?'})
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      <div className="actions">
        <button
          type="button"
          className="danger"
          disabled={terminal || cancelling}
          onClick={() => onCancel(snapshot.run.id)}
        >
          {cancelling ? 'Cancelling…' : 'Cancel run'}
        </button>
        {terminal && onRetry && status !== 'passed' ? (
          <button
            type="button"
            className="primary"
            disabled={retrying}
            onClick={() => onRetry(snapshot.run.id)}
          >
            {retrying ? 'Starting retry…' : 'Retry run'}
          </button>
        ) : null}
      </div>
    </section>
  );
}

function StepRow({ step }: { step: StepSnapshot }) {
  return (
    <li className="step" aria-label={`Step ${step.index}: ${step.title}`}>
      <header>
        <span className="step-index">Step {step.index}</span>
        <h4>{step.title}</h4>
        <span className={`status status-step status-${step.status}`}>
          {statusLabel(step.status)}
        </span>
      </header>
      <p className="criterion">{step.criterion}</p>
      {step.attempts.length === 0 ? (
        <p className="muted">No attempts yet.</p>
      ) : (
        <ul className="attempts">
          {step.attempts.map((attempt) => (
            <AttemptRow key={attempt.id} attempt={attempt} />
          ))}
        </ul>
      )}
    </li>
  );
}

function AttemptRow({ attempt }: { attempt: AttemptSnapshot }) {
  return (
    <li className={`attempt attempt-${attempt.status}`}>
      <div className="attempt-head">
        <span className="mono">
          Strategy {attempt.strategy} · attempt {attempt.id.slice(0, 8)}
        </span>
        <span className={`status status-${attempt.status}`}>
          Attempt {statusLabel(attempt.status)}
        </span>
      </div>
      <p className="mono muted">
        {attempt.model} · {attemptTool(attempt)}
      </p>
      {attempt.error ? <p className="attempt-error">{attempt.error}</p> : null}
    </li>
  );
}

function attemptTool(attempt: AttemptSnapshot): string {
  const action = attempt.action;
  if (action !== null && typeof action === 'object' && !Array.isArray(action)) {
    const tool = action['tool'];
    if (typeof tool === 'string') {
      return tool;
    }
  }
  return 'no tool';
}

function lastFinishReason(events: readonly RunEvent[]): string | null {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index];
    if (event.kind === 'run.finished' || event.kind === 'step.failed') {
      const reason = stringField(event.payload, 'reason');
      if (reason) {
        return reason;
      }
    }
  }
  return null;
}

function decisionLabel(payload: RunEvent['payload']): string {
  const decision = payload['decision'];
  if (decision !== null && typeof decision === 'object' && !Array.isArray(decision)) {
    const action = decision['action'];
    if (typeof action === 'string') {
      return action;
    }
  }
  if (typeof decision === 'string') {
    return decision;
  }
  return 'recorded';
}

function stringField(payload: RunEvent['payload'], key: string): string | null {
  const value = payload[key];
  return typeof value === 'string' ? value : null;
}

function numberField(payload: RunEvent['payload'], key: string): number | null {
  const value = payload[key];
  return typeof value === 'number' ? value : null;
}
