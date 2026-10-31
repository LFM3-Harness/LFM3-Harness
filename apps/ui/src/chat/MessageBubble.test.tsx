import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import type { ChatMessageRecord } from '../bridge/types';
import { MessageBubble } from './MessageBubble';

function message(overrides: Partial<ChatMessageRecord> = {}): ChatMessageRecord {
  return {
    id: 'message-1',
    session_id: 'session-1',
    seq: 1,
    role: 'assistant',
    content: 'The report is in reports/overview.md.',
    status: 'complete',
    meta: {},
    created_at: 1_000,
    ...overrides,
  };
}

describe('MessageBubble', () => {
  it('shows the decision route and reads the answer aloud only when asked', async () => {
    const user = userEvent.setup();
    const onSpeak = vi.fn();
    render(
      <MessageBubble
        message={message({
          meta: {
            route: {
              intent: 'report',
              source: 'decision',
              confidence_bps: 8_200,
              reason: 'The person asked for a written report.',
              model: 'd1-2b',
              roles: [],
            },
          },
        })}
        streaming={undefined}
        actions={[]}
        deciding={null}
        voiceAvailable
        speaking={false}
        onDecide={vi.fn()}
        onSpeak={onSpeak}
      />,
    );

    expect(screen.getByText(/the report is in/i)).toBeInTheDocument();
    expect(screen.getByText('report')).toBeInTheDocument();
    expect(screen.getByText('d1 82%')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: /speak/i }));
    expect(onSpeak).toHaveBeenCalledWith('message-1', 'The report is in reports/overview.md.');
  });

  it('hides the empty body of a tool-call-only message but keeps its route', () => {
    render(
      <MessageBubble
        message={message({
          content: '',
          meta: {
            tool_calls: [{ id: 'call-1' }],
            route: {
              intent: 'explore',
              source: 'heuristic',
              confidence_bps: 0,
              reason: 'The person asked to search a folder.',
              model: null,
              roles: [],
            },
          },
        })}
        streaming={undefined}
        actions={[]}
        deciding={null}
        voiceAvailable
        speaking={false}
        onDecide={vi.fn()}
        onSpeak={vi.fn()}
      />,
    );

    expect(screen.getByText('explore')).toBeInTheDocument();
    expect(screen.getByText('keyword route')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /speak/i })).not.toBeInTheDocument();
  });
});
