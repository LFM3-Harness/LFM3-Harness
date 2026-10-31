import type { AcceptanceSpec, CheckSpec } from '../bridge/types';

export interface AcceptanceReviewProps {
  acceptance: AcceptanceSpec;
  acceptanceHash: string;
  expectedHash?: string;
  busy?: boolean;
  onApprove(hash: string): void;
  onReject(): void;
}

/**
 * The exact frozen contract. Approval is bound to the displayed hash, so a
 * run whose acceptance changed can never be approved through a stale review.
 */
export function AcceptanceReview({
  acceptance,
  acceptanceHash,
  expectedHash,
  busy = false,
  onApprove,
  onReject,
}: AcceptanceReviewProps) {
  const mismatch = expectedHash !== undefined && expectedHash !== acceptanceHash;

  return (
    <section className="panel" aria-labelledby="acceptance-title">
      <h2 id="acceptance-title">Frozen acceptance</h2>
      <p className="muted">
        Nothing in this contract may weaken during retries or replanning. Approval freezes
        these exact steps, checks, and budgets for the run.
      </p>

      <dl className="facts">
        <div className="span-2">
          <dt>Acceptance hash</dt>
          <dd>
            <code className="mono wrap" data-testid="acceptance-hash">
              {acceptanceHash}
            </code>
          </dd>
        </div>
        <div>
          <dt>Mission</dt>
          <dd>
            {acceptance.mission} v{acceptance.mission_version}
          </dd>
        </div>
        <div>
          <dt>Budget</dt>
          <dd>
            max {acceptance.max_steps} steps · max attempts {acceptance.max_attempts}
          </dd>
        </div>
        <div className="span-2">
          <dt>Goal</dt>
          <dd className="goal-full">{acceptance.goal}</dd>
        </div>
        <div className="span-2">
          <dt>Allowed tools</dt>
          <dd className="mono">{acceptance.allowed_tools.join(', ')}</dd>
        </div>
      </dl>

      {mismatch ? (
        <p className="notice notice-bad" role="alert">
          Acceptance hash does not match this run. Refusing to approve a stale contract.
        </p>
      ) : null}

      <h3 className="subhead">Steps</h3>
      <ol className="steps">
        {acceptance.steps.map((step) => (
          <li key={step.index} className="step">
            <header>
              <span className="step-index">Step {step.index}</span>
              <h4>{step.title}</h4>
              {step.tool ? <span className="chip">{step.tool}</span> : null}
            </header>
            <p className="criterion">{step.criterion}</p>
            <ul className="checks">
              {step.checks.map((check, index) => (
                <li key={`${check.kind}-${index}`}>
                  <span className="mono">{check.kind}</span>
                  <span className="mono muted">{describeCheck(check)}</span>
                </li>
              ))}
            </ul>
          </li>
        ))}
      </ol>

      <h3 className="subhead">Required output</h3>
      {acceptance.output.required_artifacts.length === 0 ? (
        <p className="muted">No required artifacts.</p>
      ) : (
        <ul className="artifacts">
          {acceptance.output.required_artifacts.map((artifact) => (
            <li key={artifact.path}>
              <code>{artifact.path}</code>
              {artifact.non_empty ? <span className="muted"> non-empty</span> : null}
            </li>
          ))}
        </ul>
      )}

      <div className="actions">
        <button
          type="button"
          className="primary"
          disabled={busy || mismatch}
          onClick={() => onApprove(acceptanceHash)}
        >
          Approve plan
        </button>
        <button type="button" className="danger" disabled={busy} onClick={onReject}>
          Reject plan
        </button>
      </div>
    </section>
  );
}

function describeCheck(check: CheckSpec): string {
  const args = Object.entries(check)
    .filter(([key]) => key !== 'kind')
    .map(([key, value]) => `${key}: ${JSON.stringify(value)}`)
    .join(', ');
  return args.length > 0 ? args : 'no arguments';
}
