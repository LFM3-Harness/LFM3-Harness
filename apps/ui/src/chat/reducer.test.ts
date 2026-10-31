import { describe, expect, it } from 'vitest';

import type {
  ChatActionRecord,
  ChatMessageRecord,
  ChatSession,
  ChatSessionView,
} from '../bridge/types';
import {
  applyChatEvent,
  initialChatState,
  mergeView,
  type ChatUiState,
} from './reducer';

const session: ChatSession = {
  id: 'session-1',
  title: 'New chat',
  created_at: 1_000,
  updated_at: 1_000,
};

function message(overrides: Partial<ChatMessageRecord> = {}): ChatMessageRecord {
  return {
    id: 'message-1',
    session_id: session.id,
    seq: 1,
    role: 'assistant',
    content: '',
    status: 'streaming',
    meta: {},
    created_at: 1_001,
    ...overrides,
  };
}

function action(overrides: Partial<ChatActionRecord> = {}): ChatActionRecord {
  return {
    id: 'action-1',
    session_id: session.id,
    message_id: 'message-1',
    tool: 'write_file',
    arguments: { path: 'notes.md', content: 'hello' },
    summary: 'Create notes.md',
    status: 'pending',
    created_at: 1_002,
    decided_at: null,
    ...overrides,
  };
}

function stateWithView(): ChatUiState {
  return {
    ...initialChatState,
    view: { session, messages: [], actions: [] },
  };
}

describe('applyChatEvent', () => {
  it('streams assistant text and clears it when the durable message arrives', () => {
    let state = stateWithView();
    const start = message();
    state = applyChatEvent(state, { session_id: session.id, kind: 'assistant_start', message: start });
    expect(state.streaming[start.id]).toBe('');

    state = applyChatEvent(state, {
      session_id: session.id,
      kind: 'assistant_delta',
      message_id: start.id,
      delta: 'Hello ',
    });
    state = applyChatEvent(state, {
      session_id: session.id,
      kind: 'assistant_delta',
      message_id: start.id,
      delta: 'world',
    });
    expect(state.streaming[start.id]).toBe('Hello world');
    expect(state.view?.messages[0].content).toBe('');

    const finished = message({ content: 'Hello world', status: 'complete' });
    state = applyChatEvent(state, {
      session_id: session.id,
      kind: 'assistant_done',
      message: finished,
    });
    expect(state.streaming).toEqual({});
    expect(state.view?.messages).toHaveLength(1);
    expect(state.view?.messages[0].content).toBe('Hello world');
    expect(state.view?.messages[0].status).toBe('complete');
  });

  it('tracks a running turn from user message through status to done', () => {
    let state = stateWithView();
    const user = message({ id: 'user-1', seq: 1, role: 'user', content: 'hi', status: 'complete' });
    state = applyChatEvent(state, { session_id: session.id, kind: 'user_message', message: user });
    expect(state.running).toBe(true);

    state = applyChatEvent(state, {
      session_id: session.id,
      kind: 'status',
      state: 'thinking',
      detail: null,
    });
    expect(state.status).toEqual({ state: 'thinking', detail: null });
    expect(state.running).toBe(true);

    state = applyChatEvent(state, { session_id: session.id, kind: 'done' });
    expect(state.running).toBe(false);
    expect(state.status).toBeNull();
  });

  it('updates an approval card in place when it is resolved', () => {
    let state = stateWithView();
    state = applyChatEvent(state, {
      session_id: session.id,
      kind: 'action_requested',
      action: action(),
    });
    expect(state.view?.actions[0].status).toBe('pending');

    state = applyChatEvent(state, {
      session_id: session.id,
      kind: 'action_resolved',
      action: action({ status: 'applied', decided_at: 1_010 }),
    });
    expect(state.view?.actions).toHaveLength(1);
    expect(state.view?.actions[0].status).toBe('applied');
  });

  it('keeps tool output ordered by sequence', () => {
    let state = stateWithView();
    const tool = message({
      id: 'tool-2',
      seq: 3,
      role: 'tool',
      content: 'file contents',
      status: 'complete',
      meta: { tool: 'read_file' },
    });
    const assistant = message({ id: 'assistant-1', seq: 2, content: 'Reading…', status: 'complete' });
    state = applyChatEvent(state, { session_id: session.id, kind: 'tool_result', message: tool });
    state = applyChatEvent(state, {
      session_id: session.id,
      kind: 'assistant_start',
      message: assistant,
    });
    const settled = message({ id: 'assistant-1', seq: 2, content: 'Reading…', status: 'complete' });
    state = applyChatEvent(state, {
      session_id: session.id,
      kind: 'assistant_done',
      message: settled,
    });
    expect(state.view?.messages.map((entry) => entry.id)).toEqual(['assistant-1', 'tool-2']);
  });

  it('records errors and ignores events before a conversation is open', () => {
    expect(applyChatEvent(initialChatState, { session_id: session.id, kind: 'done' })).toBe(
      initialChatState,
    );
    const failed = applyChatEvent(stateWithView(), {
      session_id: session.id,
      kind: 'error',
      message: 'model unavailable',
    });
    expect(failed.error).toBe('model unavailable');
    expect(failed.running).toBe(false);
  });

  it('renames the session from a session update', () => {
    const renamed = { ...session, title: 'Workspace survey', updated_at: 2_000 };
    const state = applyChatEvent(stateWithView(), {
      session_id: session.id,
      kind: 'session_updated',
      session: renamed,
    });
    expect(state.view?.session.title).toBe('Workspace survey');
  });

  it('shows the decision route during a turn and clears it when done', () => {
    let state = applyChatEvent(stateWithView(), {
      session_id: session.id,
      kind: 'route',
      route: {
        intent: 'report',
        source: 'decision',
        confidence_bps: 8_200,
        reason: 'Ask for a written report.',
        model: 'lfm3-d1',
        roles: [
          { role: 'route', model: 'd1', capability: 'decide', used: true },
          { role: 'answer', model: 'text', capability: 'write', used: true },
        ],
      },
    });
    expect(state.route?.intent).toBe('report');
    expect(state.route?.source).toBe('decision');

    state = applyChatEvent(state, { session_id: session.id, kind: 'done' });
    expect(state.route).toBeNull();
  });

  it('keeps the latest gate decision and drops it when the turn ends', () => {
    let state = applyChatEvent(stateWithView(), {
      session_id: session.id,
      kind: 'decision',
      decision: {
        point: 'write_gate',
        label: 'contradicts',
        confidence_bps: 9_100,
        floor_bps: 7_500,
        accepted: true,
        outcome: 'hold',
        model: 'lfm3-d1',
        reason: 'The request asked for a summary, not a rewrite.',
        target: 'notes.md',
      },
    });
    expect(state.decision?.outcome).toBe('hold');
    expect(state.decision?.target).toBe('notes.md');

    state = applyChatEvent(state, { session_id: session.id, kind: 'done' });
    expect(state.decision).toBeNull();
  });

  it('clears a stale decision when the next user turn starts', () => {
    const base: ChatUiState = {
      ...stateWithView(),
      decision: {
        point: 'evidence_gate',
        label: 'insufficient',
        confidence_bps: 7_000,
        floor_bps: 6_500,
        accepted: true,
        outcome: 'advise',
        model: 'lfm3-d1',
        reason: 'Only one file was read.',
        target: null,
      },
    };
    const user = message({ id: 'user-1', seq: 1, role: 'user', content: 'next', status: 'complete' });
    const state = applyChatEvent(base, {
      session_id: session.id,
      kind: 'user_message',
      message: user,
    });
    expect(state.decision).toBeNull();
  });
});

describe('mergeView', () => {
  it('replaces the transcript and drops streaming text while keeping the last error', () => {
    const before: ChatUiState = {
      ...stateWithView(),
      streaming: { 'message-1': 'partial' },
      error: 'network hiccup',
    };
    const view: ChatSessionView = {
      session,
      messages: [message({ content: 'final', status: 'complete' })],
      actions: [action({ status: 'applied' })],
    };
    const after = mergeView(before, view);
    expect(after.view).toEqual(view);
    expect(after.streaming).toEqual({});
    expect(after.error).toBe('network hiccup');
  });
});
