import type { RunRecord } from '../bridge/types';
import { isTerminal, shortId, statusLabel } from './format';

export interface RunHistoryProps {
  runs: RunRecord[];
  activeRunId: string | null;
  busyRunId?: string | null;
  onSelect(runId: string): void;
  onRetry(runId: string): void;
}

/**
 * Durable run history. Terminal states stay visibly distinct, a passed run
 * opens its final evidence, and retries remain linked to their parent run.
 */
export function RunHistory({
  runs,
  activeRunId,
  busyRunId = null,
  onSelect,
  onRetry,
}: RunHistoryProps) {
  if (runs.length === 0) {
    return (
      <section className="panel" aria-labelledby="history-title">
        <h2 id="history-title">History</h2>
        <p className="muted">No runs in this journal.</p>
      </section>
    );
  }

  return (
    <section className="panel" aria-labelledby="history-title">
      <h2 id="history-title">History</h2>
      <table className="runs">
        <thead>
          <tr>
            <th>Status</th>
            <th>Mission</th>
            <th>Goal</th>
            <th>Acceptance</th>
            <th>Started</th>
            <th>Evidence</th>
          </tr>
        </thead>
        <tbody>
          {runs.map((run) => {
            const terminal = isTerminal(run.status);
            return (
              <tr
                key={run.id}
                className={run.id === activeRunId ? 'run-active' : undefined}
                aria-current={run.id === activeRunId ? 'true' : undefined}
              >
                <td>
                  <span className={`status status-${run.status}`}>{statusLabel(run.status)}</span>
                </td>
                <td>
                  {run.mission}
                  {run.parent_run_id ? (
                    <span className="block muted mono">
                      Retry of {shortId(run.parent_run_id)}
                    </span>
                  ) : null}
                </td>
                <td className="goal">{run.goal.split('\n')[0]}</td>
                <td className="mono">{shortId(run.acceptance_hash)}</td>
                <td>{new Date(run.created_at).toLocaleString()}</td>
                <td>
                  <div className="actions">
                    <button
                      type="button"
                      className="small"
                      onClick={() => onSelect(run.id)}
                      aria-label={
                        run.status === 'passed'
                          ? `Final evidence for ${shortId(run.id)}`
                          : `Open run ${shortId(run.id)}`
                      }
                    >
                      {run.status === 'passed' ? 'Final evidence' : 'Open run'}
                    </button>
                    {terminal && run.status !== 'passed' ? (
                      <button
                        type="button"
                        className="small"
                        onClick={() => onRetry(run.id)}
                        disabled={busyRunId === run.id}
                        aria-label={`Retry run ${shortId(run.id)}`}
                      >
                        {busyRunId === run.id ? 'Retrying…' : 'Retry run'}
                      </button>
                    ) : null}
                  </div>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}
