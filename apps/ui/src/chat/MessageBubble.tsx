import type { ChatActionRecord, ChatMessageRecord, ChatRoute } from '../bridge/types';
import { ActionCard } from './ActionCard';
import { RichText } from './RichText';

export function MessageBubble({
  message,
  streaming,
  actions,
  deciding,
  voiceAvailable,
  speaking,
  onDecide,
  onSpeak,
}: {
  message: ChatMessageRecord;
  streaming: string | undefined;
  actions: ChatActionRecord[];
  deciding: string | null;
  voiceAvailable: boolean;
  speaking: boolean;
  onDecide: (actionId: string, allow: boolean) => void;
  onSpeak: (messageId: string, text: string) => void;
}) {
  if (message.role === 'system') {
    return null;
  }

  const related = actions.filter((action) => action.message_id === message.id);
  const content = streaming ?? message.content;
  const toolCalls = Array.isArray(message.meta.tool_calls) ? message.meta.tool_calls : [];
  const streamingNow = streaming !== undefined;
  const route = routeOf(message);

  if (message.role === 'tool') {
    const toolName =
      typeof message.meta.tool === 'string' ? message.meta.tool : 'tool';
    return (
      <details className="tool-note">
        <summary>
          <span className="tool-dot" aria-hidden="true" />
          {toolName} output
        </summary>
        <pre>{message.content}</pre>
      </details>
    );
  }

  const isAssistant = message.role === 'assistant';
  const hidden = isAssistant && content.trim().length === 0 && toolCalls.length > 0;

  return (
    <div className={`message message-${message.role}`}>
      <div className="message-meta">
        <span className="message-author">{isAssistant ? 'LFM3' : 'You'}</span>
        <time dateTime={new Date(message.created_at).toISOString()}>
          {new Date(message.created_at).toLocaleTimeString([], {
            hour: '2-digit',
            minute: '2-digit',
          })}
        </time>
        {message.status === 'cancelled' ? <span className="message-flag">stopped</span> : null}
        {message.status === 'error' ? <span className="message-flag">error</span> : null}
        {isAssistant && voiceAvailable && content.trim().length > 0 ? (
          <button
            type="button"
            className="speak-button"
            title="Read this answer aloud with the local voice"
            aria-pressed={speaking}
            onClick={() => onSpeak(message.id, content)}
          >
            {speaking ? 'Stop' : 'Speak'}
          </button>
        ) : null}
      </div>
      {route ? (
        <div className={`route-badge route-${route.intent}`} title={route.reason}>
          <span className="route-intent">{route.intent}</span>
          <span className="route-source">
            {route.source === 'decision'
              ? `d1 ${(route.confidence_bps / 100).toFixed(0)}%`
              : 'keyword route'}
          </span>
        </div>
      ) : null}
      {hidden ? null : (
        <div className={`message-body ${message.status === 'error' ? 'message-error' : ''}`}>
          {content.trim().length === 0 && streamingNow ? (
            <span className="typing" aria-label="The model is writing">
              <span />
              <span />
              <span />
            </span>
          ) : (
            <RichText text={content} />
          )}
        </div>
      )}
      {related.map((action) => (
        <ActionCard
          key={action.id}
          action={action}
          busy={deciding === action.id}
          onDecide={onDecide}
        />
      ))}
    </div>
  );
}

function routeOf(message: ChatMessageRecord): ChatRoute | null {
  const value = message.meta.route;
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return null;
  }
  const intent = (value as Record<string, unknown>).intent;
  const source = (value as Record<string, unknown>).source;
  if (typeof intent !== 'string' || (source !== 'decision' && source !== 'heuristic')) {
    return null;
  }
  return value as unknown as ChatRoute;
}
