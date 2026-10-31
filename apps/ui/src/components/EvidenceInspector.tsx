import type { JsonObject, RunEvent, RunSnapshot } from '../bridge/types';
import { statusLabel } from './format';

export interface EvidenceInspectorProps {
  snapshot: RunSnapshot;
  events: readonly RunEvent[];
  /** Highlights the run's own final verification evidence. */
  final?: boolean;
}

/**
 * Dense, typed evidence. The inspector never turns a message into a verdict:
 * `pass`, `fail`, and `error` come from the engine, and semantic results are
 * explicitly advisory until their mission is qualified for automatic pass.
 */
export function EvidenceInspector({ snapshot, events, final }: EvidenceInspectorProps) {
  const checks = events.filter((event) => event.kind === 'check.result');
  const tools = events.filter((event) => event.kind === 'tool.result');
  // A pass is judged on the attempt that settled each step. Earlier failed
  // attempts stay visible as history but cannot veto the deciding evidence.
  const deciding = new Set(
    snapshot.steps.flatMap((step) => {
      const last = step.attempts[step.attempts.length - 1];
      return last ? [last.id] : [];
    }),
  );
  const hasFailure = checks.some((event) => {
    const status = stringField(event.payload, 'status');
    const attempt = stringField(event.payload, 'attempt');
    const decidingCheck = deciding.size === 0 || (attempt !== null && deciding.has(attempt));
    return decidingCheck && (status === 'fail' || status === 'error');
  });
  const showFinal = (final ?? snapshot.run.status === 'passed') && !hasFailure;

  return (
    <section className="panel" aria-labelledby="evidence-title">
      <h2 id="evidence-title">{showFinal ? 'Final evidence' : 'Evidence inspector'}</h2>
      <p className="muted">
        Check results and artifact digests as reported by the engine. A message alone is never
        a pass; only the typed status and its same-attempt evidence count.
      </p>

      {checks.length === 0 ? (
        <p className="muted">No check results recorded yet.</p>
      ) : (
        <table className="evidence">
          <thead>
            <tr>
              <th>Check</th>
              <th>Status</th>
              <th>Message</th>
              <th>Evidence</th>
            </tr>
          </thead>
          <tbody>
            {checks.map((event) => (
              <CheckRow key={event.seq} event={event} />
            ))}
          </tbody>
        </table>
      )}

      {tools.length > 0 ? (
        <>
          <h3 className="subhead">Tool results</h3>
          <ul className="tool-results">
            {tools.map((event) => (
              <ToolResultRow key={event.seq} event={event} />
            ))}
          </ul>
        </>
      ) : null}
    </section>
  );
}

function CheckRow({ event }: { event: RunEvent }) {
  const payload = event.payload;
  const status = stringField(payload, 'status') ?? 'error';
  const sources = stringArray(payload, 'sources');
  const confidenceBps = numberField(payload, 'confidence_bps');
  const confidence =
    confidenceBps === null ? numberField(payload, 'confidence') : confidenceBps / 10_000;
  const semantic =
    payload['semantic'] === true || (stringField(payload, 'kind') ?? '').includes('semantic');

  return (
    <tr data-testid="check-row" data-status={status}>
      <td>
        <span className="mono">{stringField(payload, 'kind') ?? 'unknown'}</span>
        <span className="block muted mono">
          step {numberField(payload, 'step_index') ?? '?'} · attempt{' '}
          {short(stringField(payload, 'attempt'))}
        </span>
      </td>
      <td>
        <span className={`status status-check status-${status}`}>{statusLabel(status)}</span>
      </td>
      <td>
        <span>{stringField(payload, 'message') ?? 'no message'}</span>
        {semantic ? (
          <span className="semantic-label block">
            {semanticLabel(
              confidence,
              stringField(payload, 'semantic_chain'),
              stringField(payload, 'semantic_label'),
            )}
          </span>
        ) : null}
      </td>
      <td>
        <span className="mono muted">
          {numberField(payload, 'evidence_count') ?? sources.length} source(s)
        </span>
        <ul className="sources">
          {sources.map((source) => (
            <li key={source} className="mono">
              {source}
            </li>
          ))}
        </ul>
      </td>
    </tr>
  );
}

function ToolResultRow({ event }: { event: RunEvent }) {
  const payload = event.payload;
  const artifacts = stringArray(payload, 'artifacts');
  return (
    <li>
      <div className="actions">
        <span className="mono">{stringField(payload, 'tool') ?? 'tool'}</span>
        <span className={`status status-${stringField(payload, 'status') ?? 'failed'}`}>
          {statusLabel(stringField(payload, 'status') ?? 'failed')}
        </span>
        <span className="mono muted">attempt {short(stringField(payload, 'attempt'))}</span>
      </div>
      {artifacts.length === 0 ? (
        <p className="muted">No artifacts reported.</p>
      ) : (
        <ul className="artifacts">
          {artifacts.map((digest) => (
            <li key={digest}>
              <code className="mono">{digest}</code>
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}

function semanticLabel(
  confidence: number | null,
  chain: string | null,
  label: string | null,
): string {
  const suffix = `${chain ? ` · chain ${chain}` : ''}${label ? ` · ${label}` : ''}`;
  if (confidence === null) {
    return `Semantic verdict — advisory, confidence not reported${suffix}`;
  }
  const percent = Math.round(confidence * 100);
  const low = confidence < 0.6 ? ', low' : '';
  return `Semantic confidence ${percent}% (advisory${low})${suffix}`;
}

function short(value: string | null): string {
  return value ? value.slice(0, 8) : '?';
}

function stringField(payload: JsonObject, key: string): string | null {
  const value = payload[key];
  return typeof value === 'string' ? value : null;
}

function numberField(payload: JsonObject, key: string): number | null {
  const value = payload[key];
  return typeof value === 'number' ? value : null;
}

function stringArray(payload: JsonObject, key: string): string[] {
  const value = payload[key];
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : [];
}
