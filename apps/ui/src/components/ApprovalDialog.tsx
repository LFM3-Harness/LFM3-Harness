import type { ToolName } from '../bridge/types';

export interface ApprovalRequest {
  id: string;
  run_id: string;
  step_index: number;
  tool: ToolName;
  summary: string;
  detail: string;
  risk: 'low' | 'medium' | 'high';
  expires_at: number | null;
}

export interface ApprovalDialogProps {
  request: ApprovalRequest | null;
  busy?: boolean;
  onDecide(approvalId: string, allow: boolean): void;
}

/**
 * A single, explicit approval decision. There is no default action and no
 * keyboard shortcut that allows implicitly; denial is always available.
 */
export function ApprovalDialog({ request, busy = false, onDecide }: ApprovalDialogProps) {
  if (request === null) {
    return null;
  }

  return (
    <div
      className="approval-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="approval-title"
      aria-describedby="approval-detail"
    >
      <div className="approval-card">
        <p className="eyebrow">Step {request.step_index}</p>
        <h2 id="approval-title">Approval requested</h2>
        <p className="approval-summary">{request.summary}</p>
        <p id="approval-detail" className="muted">
          {request.detail}
        </p>
        <dl className="facts">
          <div>
            <dt>Tool</dt>
            <dd className="mono">{request.tool}</dd>
          </div>
          <div>
            <dt>Risk</dt>
            <dd>{request.risk} risk</dd>
          </div>
          <div>
            <dt>Expires</dt>
            <dd className="mono">
              {request.expires_at === null
                ? 'no deadline'
                : new Date(request.expires_at).toLocaleString()}
            </dd>
          </div>
        </dl>
        <div className="actions">
          <button
            type="button"
            className="primary"
            disabled={busy}
            onClick={() => onDecide(request.id, true)}
          >
            Allow once
          </button>
          <button
            type="button"
            className="danger"
            disabled={busy}
            onClick={() => onDecide(request.id, false)}
          >
            Deny
          </button>
        </div>
      </div>
    </div>
  );
}
