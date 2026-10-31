import type { ChatActionRecord, JsonValue } from '../bridge/types';

const STATUS_LABEL: Record<ChatActionRecord['status'], string> = {
  pending: 'Needs your approval',
  approved: 'Approved',
  denied: 'Denied',
  applied: 'Applied',
  failed: 'Failed',
  expired: 'Expired',
};

export function ActionCard({
  action,
  busy,
  onDecide,
}: {
  action: ChatActionRecord;
  busy: boolean;
  onDecide: (actionId: string, allow: boolean) => void;
}) {
  const pending = action.status === 'pending';
  const content = textField(action.arguments, 'content');
  const oldText = textField(action.arguments, 'old_text');
  const newText = textField(action.arguments, 'new_text');
  const path = textField(action.arguments, 'path');

  return (
    <article
      className={`action-card action-${action.status}`}
      aria-label={`Tool approval: ${action.summary}`}
    >
      <header className="action-head">
        <span className="label">{action.tool.replace('_', ' ')}</span>
        <span className={`status status-action-${action.status}`}>
          {STATUS_LABEL[action.status]}
        </span>
      </header>
      <h4>{action.summary}</h4>
      {path ? <p className="action-path">{path}</p> : null}
      {content !== null || oldText !== null ? (
        <details className="action-detail">
          <summary>Review the exact change</summary>
          {oldText !== null ? (
            <div className="diff-grid">
              <div>
                <span className="label">Remove</span>
                <pre>{oldText}</pre>
              </div>
              <div>
                <span className="label">Insert</span>
                <pre>{newText ?? ''}</pre>
              </div>
            </div>
          ) : (
            <pre>{content ?? ''}</pre>
          )}
        </details>
      ) : null}
      {pending ? (
        <div className="actions">
          <button
            type="button"
            className="primary"
            disabled={busy}
            onClick={() => onDecide(action.id, true)}
          >
            Allow once
          </button>
          <button
            type="button"
            className="danger"
            disabled={busy}
            onClick={() => onDecide(action.id, false)}
          >
            Deny
          </button>
        </div>
      ) : null}
    </article>
  );
}

function textField(arguments_: Record<string, JsonValue> | undefined, key: string): string | null {
  const value = arguments_?.[key];
  return typeof value === 'string' ? value : null;
}
