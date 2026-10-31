import type { JobCall, JobRecord, JobTodo, JobTurn } from '../bridge/types';
import { statusLabel } from './format';

export interface TodoTimelineProps {
  job: JobRecord;
  todos: readonly JobTodo[];
  turns: readonly JobTurn[];
  jobs?: readonly JobRecord[];
  cancelling?: boolean;
  recovering?: boolean;
  onCancel?(jobId: string): void;
  onSelect?(jobId: string): void;
}

/**
 * The live job surface: the model's own todo list, every turn it took, and the
 * measured deliverable proof.
 *
 * A job is not verified work, so this panel never shows an acceptance hash. It
 * shows what actually happened: the calls, their real outputs, and whether the
 * required file exists with bytes and a digest.
 */
export function TodoTimeline({
  job,
  todos,
  turns,
  jobs = [],
  cancelling = false,
  recovering = false,
  onCancel,
  onSelect,
}: TodoTimelineProps) {
  const running = job.status === 'running';
  const done = todos.filter((todo) => todo.done).length;

  return (
    <section className="panel" aria-labelledby="job-timeline-title">
      <div className="panel-heading">
        <div>
          <h2 id="job-timeline-title">Job timeline</h2>
          <p className="mono muted">
            {job.model || 'text model'} · {job.id.slice(0, 8)} · {job.minutes} min ·{' '}
            {job.max_turns} turns max
          </p>
        </div>
        <span className={`status status-${job.status}`} data-testid="job-status">
          {statusLabel(job.status)}
        </span>
      </div>

      <p className="job-goal">{job.goal}</p>
      {job.deliverable ? (
        <p className="mono muted">Required output: {job.deliverable}</p>
      ) : (
        <p className="muted">No required output; the job ends when the model finishes.</p>
      )}
      {job.deliverable_proof ? (
        <p className="mono muted">
          Proven output: {job.deliverable_proof.path} · {job.deliverable_proof.bytes} bytes ·
          sha256 {job.deliverable_proof.digest.slice(0, 16)}…
        </p>
      ) : null}
      {job.stopped ? <p className="mono muted">Stopped: {job.stopped}</p> : null}

      {recovering ? (
        <p className="notice" role="status">
          Recovering journal after a skipped turn. Re-reading the authoritative job before
          applying more turns.
        </p>
      ) : null}

      {job.status === 'interrupted' ? (
        <p className="notice notice-bad">
          Interrupted — not a success. The engine stopped before this job finished; it was not
          resumed, so start a new job to continue the work.
        </p>
      ) : null}

      {job.status === 'failed' ? (
        <p className="notice notice-bad">
          This job stopped without a verified output. The turns below show what it tried.
        </p>
      ) : null}

      <h3 className="subhead">Todo list</h3>
      {todos.length === 0 ? (
        <p className="muted">The model has not written a todo list yet.</p>
      ) : (
        <>
          <p className="muted">
            {done} of {todos.length} marked done
          </p>
          <ul className="todo-list" aria-label="Job todos">
            {todos.map((todo, index) => (
              <li
                key={`${index}-${todo.text}`}
                className={todo.done ? 'todo todo-done' : 'todo'}
              >
                <span className="todo-box" aria-hidden="true">
                  {todo.done ? '✓' : '○'}
                </span>
                <span>{todo.text}</span>
                <span className="muted">{todo.done ? 'done' : 'open'}</span>
              </li>
            ))}
          </ul>
        </>
      )}

      <h3 className="subhead">Turns</h3>
      {turns.length === 0 ? (
        <p className="muted">Waiting for the first turn.</p>
      ) : (
        <ol className="turns">
          {turns.map((turn) => (
            <li key={turn.index} className="turn" aria-label={`Turn ${turn.index}`}>
              <header>
                <span className="turn-index">Turn {turn.index}</span>
                <span className="muted">
                  {turn.calls.length === 0
                    ? 'no tool call'
                    : `${turn.calls.length} call${turn.calls.length === 1 ? '' : 's'}`}
                </span>
              </header>
              {turn.note ? <p className="turn-note">{turn.note}</p> : null}
              {turn.calls.length > 0 ? (
                <ul className="turn-calls">
                  {turn.calls.map((call, index) => (
                    <CallRow key={`${turn.index}-${index}`} call={call} />
                  ))}
                </ul>
              ) : null}
            </li>
          ))}
        </ol>
      )}

      {jobs.length > 1 ? (
        <div className="job-history">
          <h3 className="subhead">Recent jobs</h3>
          <ul>
            {jobs.map((entry) => (
              <li key={entry.id}>
                <button
                  type="button"
                  className="mission-button"
                  aria-pressed={entry.id === job.id}
                  onClick={() => onSelect?.(entry.id)}
                >
                  <span className={`status status-${entry.status}`}>
                    {statusLabel(entry.status)}
                  </span>{' '}
                  {entry.goal}
                </button>
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      <div className="actions">
        {running ? (
          <button
            type="button"
            className="danger"
            disabled={cancelling}
            onClick={() => onCancel?.(job.id)}
          >
            {cancelling ? 'Stopping…' : 'Stop job'}
          </button>
        ) : null}
      </div>
    </section>
  );
}

function CallRow({ call }: { call: JobCall }) {
  const detail = callDetail(call);
  return (
    <li className={call.ok ? 'call call-ok' : 'call call-failed'}>
      <div className="call-head">
        <span className="mono">{call.tool}</span>
        <span
          className={`status status-check ${call.ok ? 'status-pass' : 'status-fail'}`}
        >
          {call.ok ? 'ok' : 'failed'}
        </span>
      </div>
      {detail ? <p className="mono muted">{detail}</p> : null}
      {call.output ? <pre className="call-output">{clip(call.output, 900)}</pre> : null}
    </li>
  );
}

/** Paths and short arguments are what a reader needs; never the whole file. */
function callDetail(call: JobCall): string | null {
  const args = call.arguments;
  if (args === null || typeof args !== 'object' || Array.isArray(args)) {
    return null;
  }
  const path = args['path'];
  if (typeof path === 'string') {
    const content = args['content'];
    const bytes =
      typeof content === 'string' ? ` · ${content.length} chars written` : '';
    return `${path}${bytes}`;
  }
  const items = args['items'];
  if (Array.isArray(items)) {
    return `${items.length} todo item${items.length === 1 ? '' : 's'}`;
  }
  const summary = JSON.stringify(args);
  return summary && summary !== '{}' ? clip(summary, 160) : null;
}

function clip(text: string, limit: number): string {
  const characters = [...text];
  if (characters.length <= limit) {
    return text;
  }
  return `${characters.slice(0, limit).join('')}…`;
}
