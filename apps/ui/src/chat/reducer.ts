import type {
  ChatActionRecord,
  ChatEvent,
  ChatMessageRecord,
  ChatRoute,
  ChatSessionView,
} from '../bridge/types';

export interface ChatUiState {
  view: ChatSessionView | null;
  streaming: Record<string, string>;
  status: { state: string; detail: string | null } | null;
  route: ChatRoute | null;
  running: boolean;
  error: string | null;
}

export const initialChatState: ChatUiState = {
  view: null,
  streaming: {},
  status: null,
  route: null,
  running: false,
  error: null,
};

function upsertMessage(
  messages: ChatMessageRecord[],
  message: ChatMessageRecord,
): ChatMessageRecord[] {
  const existing = messages.findIndex((entry) => entry.id === message.id);
  if (existing >= 0) {
    const next = messages.slice();
    next[existing] = message;
    return next;
  }
  return [...messages, message].sort((left, right) => left.seq - right.seq);
}

function upsertAction(
  actions: ChatActionRecord[],
  action: ChatActionRecord,
): ChatActionRecord[] {
  const existing = actions.findIndex((entry) => entry.id === action.id);
  if (existing >= 0) {
    const next = actions.slice();
    next[existing] = action;
    return next;
  }
  return [...actions, action].sort((left, right) => left.created_at - right.created_at);
}

/** Applies one live event. The durable refetch after `done` is authoritative. */
export function applyChatEvent(state: ChatUiState, event: ChatEvent): ChatUiState {
  if (!state.view) {
    return state;
  }
  const view = state.view;
  switch (event.kind) {
    case 'user_message':
      return {
        ...state,
        view: { ...view, messages: upsertMessage(view.messages, event.message) },
        running: true,
        error: null,
      };
    case 'session_updated':
      return {
        ...state,
        view:
          view.session.id === event.session.id
            ? { ...view, session: event.session }
            : view,
      };
    case 'assistant_start':
      return {
        ...state,
        view: { ...view, messages: upsertMessage(view.messages, event.message) },
        streaming: { ...state.streaming, [event.message.id]: event.message.content },
      };
    case 'assistant_delta':
      return {
        ...state,
        streaming: {
          ...state.streaming,
          [event.message_id]: (state.streaming[event.message_id] ?? '') + event.delta,
        },
      };
    case 'assistant_done': {
      const streaming = { ...state.streaming };
      delete streaming[event.message.id];
      return {
        ...state,
        view: { ...view, messages: upsertMessage(view.messages, event.message) },
        streaming,
      };
    }
    case 'action_requested':
    case 'action_resolved':
      return {
        ...state,
        view: { ...view, actions: upsertAction(view.actions, event.action) },
      };
    case 'tool_result':
      return {
        ...state,
        view: { ...view, messages: upsertMessage(view.messages, event.message) },
      };
    case 'status':
      return {
        ...state,
        status: { state: event.state, detail: event.detail },
        running: event.state !== 'idle',
      };
    case 'route':
      return { ...state, route: event.route };
    case 'error':
      return { ...state, error: event.message, running: false, status: null };
    case 'done':
      return { ...state, running: false, status: null, route: null };
    default:
      return state;
  }
}

/** Replaces the transcript with the canonical durable view. */
export function mergeView(state: ChatUiState, view: ChatSessionView): ChatUiState {
  return {
    ...state,
    view,
    streaming: {},
    error: state.error,
  };
}
