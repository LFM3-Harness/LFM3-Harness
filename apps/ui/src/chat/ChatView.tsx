import { useEffect, useRef, useState } from 'react';

import { createBridge } from '../bridge/createBridge';
import type { EngineBridge, Unsubscribe } from '../bridge/EngineBridge';
import type { ChatEvent, ChatSession, HealthReport } from '../bridge/types';
import { ChatSidebar } from './ChatSidebar';
import { Composer } from './Composer';
import { MessageBubble } from './MessageBubble';
import {
  applyChatEvent,
  initialChatState,
  mergeView,
  type ChatUiState,
} from './reducer';

const SUGGESTIONS = [
  'Analyse this folder, find the important files, and tell me what this project does.',
  'Find files related to a file I name and explain how they connect.',
  'Read the key files in a folder and write a Markdown report to reports/overview.md.',
  'Look at a screenshot in this workspace and describe what it shows.',
];

export function ChatView({ onOpenTasks }: { onOpenTasks: () => void }) {
  const [bridge, setBridge] = useState<EngineBridge | null>(null);
  const [health, setHealth] = useState<HealthReport | null>(null);
  const [sessions, setSessions] = useState<ChatSession[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [state, setState] = useState<ChatUiState>(initialChatState);
  const [loadingView, setLoadingView] = useState(false);
  const [sending, setSending] = useState(false);
  const [deciding, setDeciding] = useState<string | null>(null);
  const [turnSession, setTurnSession] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [autoVoice, setAutoVoice] = useState(false);
  const [speakingId, setSpeakingId] = useState<string | null>(null);
  const activeRef = useRef<string | null>(null);
  const loadToken = useRef(0);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const autoVoiceRef = useRef(false);
  const speechToken = useRef(0);

  useEffect(() => {
    const stored = window.localStorage.getItem('lfm3.voiceReplies') === '1';
    setAutoVoice(stored);
    autoVoiceRef.current = stored;
  }, []);

  useEffect(() => {
    return () => {
      speechToken.current += 1;
      const audio = audioRef.current;
      if (audio) {
        audio.pause();
        audio.src = '';
        audioRef.current = null;
      }
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const engine = await createBridge();
        if (cancelled) {
          return;
        }
        setBridge(engine);
        try {
          const report = await engine.health();
          if (!cancelled) {
            setHealth(report);
          }
        } catch (cause) {
          if (!cancelled) {
            setError(messageOf(cause));
          }
        }
        try {
          const list = await engine.listChatSessions(50);
          if (cancelled) {
            return;
          }
          setSessions(list);
          if (list.length > 0) {
            await loadSession(engine, list[0].id);
          }
        } catch (cause) {
          if (!cancelled) {
            setError(messageOf(cause));
          }
        }
      } catch (cause) {
        if (!cancelled) {
          setError(messageOf(cause));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
    // The engine is created once per mount; loadSession only touches setters and refs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!bridge) {
      return;
    }
    let cancelled = false;
    let unsubscribe: Unsubscribe | null = null;
    bridge
      .subscribeChat((event) => {
        if (cancelled) {
          return;
        }
        handleEvent(bridge, event);
      })
      .then((off) => {
        if (cancelled) {
          off();
        } else {
          unsubscribe = off;
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(`Live updates unavailable: ${messageOf(cause)}`);
        }
      });
    return () => {
      cancelled = true;
      unsubscribe?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bridge]);

  useEffect(() => {
    const node = scrollRef.current;
    if (node) {
      node.scrollTop = node.scrollHeight;
    }
  }, [state.view?.messages, state.streaming, state.status]);

  async function loadSession(engine: EngineBridge, sessionId: string) {
    stopSpeaking();
    const token = ++loadToken.current;
    activeRef.current = sessionId;
    setActiveId(sessionId);
    setState(initialChatState);
    setLoadingView(true);
    setError(null);
    try {
      const view = await engine.getChatSession(sessionId);
      if (token === loadToken.current && activeRef.current === sessionId) {
        setState((current) => mergeView(current, view));
      }
    } catch (cause) {
      if (token === loadToken.current) {
        setError(messageOf(cause));
      }
    } finally {
      if (token === loadToken.current) {
        setLoadingView(false);
      }
    }
  }

  async function refreshView(engine: EngineBridge, sessionId: string) {
    if (activeRef.current !== sessionId) {
      return;
    }
    const token = ++loadToken.current;
    try {
      const view = await engine.getChatSession(sessionId);
      if (token === loadToken.current && activeRef.current === sessionId) {
        setState((current) => mergeView(current, view));
      }
    } catch {
      // The live transcript stays visible; the next turn refetches.
    }
  }

  async function refreshSessions(engine: EngineBridge) {
    try {
      const list = await engine.listChatSessions(50);
      setSessions(list);
    } catch {
      // Keep the last known list.
    }
  }

  function handleEvent(engine: EngineBridge, event: ChatEvent) {
    if (event.kind === 'session_updated') {
      setSessions((current) => {
        const others = current.filter((session) => session.id !== event.session.id);
        return [event.session, ...others].sort((left, right) => right.updated_at - left.updated_at);
      });
    }
    if (event.session_id !== activeRef.current) {
      return;
    }
    setState((current) => applyChatEvent(current, event));
    if (event.kind === 'done') {
      setTurnSession((current) => (current === event.session_id ? null : current));
      void refreshSessions(engine);
      void refreshView(engine, event.session_id);
    } else if (event.kind === 'error') {
      setTurnSession((current) => (current === event.session_id ? null : current));
    }
    if (
      event.kind === 'assistant_done' &&
      autoVoiceRef.current &&
      event.message.status === 'complete' &&
      event.message.content.trim().length > 0 &&
      !Array.isArray(event.message.meta.tool_calls)
    ) {
      void speakText(event.message.id, event.message.content);
    }
  }

  async function ensureSession(engine: EngineBridge): Promise<string> {
    if (activeRef.current) {
      return activeRef.current;
    }
    const session = await engine.createChatSession();
    activeRef.current = session.id;
    setActiveId(session.id);
    setSessions((current) => [session, ...current]);
    setState({
      ...initialChatState,
      view: { session, messages: [], actions: [] },
    });
    return session.id;
  }

  async function send(text: string) {
    if (!bridge) {
      setError('The local engine is not connected.');
      return;
    }
    stopSpeaking();
    setSending(true);
    setError(null);
    let sessionId: string | null = null;
    try {
      sessionId = await ensureSession(bridge);
      setTurnSession(sessionId);
      await bridge.sendChatMessage(sessionId, text);
    } catch (cause) {
      setError(messageOf(cause));
      if (sessionId) {
        setTurnSession((current) => (current === sessionId ? null : current));
      }
    } finally {
      setSending(false);
    }
  }

  async function stop() {
    const sessionId = turnSession ?? activeId;
    if (!bridge || !sessionId) {
      return;
    }
    try {
      await bridge.cancelChat(sessionId);
    } catch (cause) {
      setError(messageOf(cause));
    }
  }

  async function decide(actionId: string, allow: boolean) {
    if (!bridge) {
      return;
    }
    setDeciding(actionId);
    setError(null);
    try {
      await bridge.decideChatAction(actionId, allow);
    } catch (cause) {
      setError(messageOf(cause));
    } finally {
      setDeciding(null);
    }
  }

  function stopSpeaking() {
    speechToken.current += 1;
    const audio = audioRef.current;
    if (audio) {
      audio.pause();
      audio.src = '';
      audioRef.current = null;
    }
    setSpeakingId(null);
  }

  async function speakText(messageId: string, text: string) {
    if (!bridge) {
      setError('The local engine is not connected.');
      return;
    }
    if (audioRef.current !== null && speakingId === messageId) {
      stopSpeaking();
      return;
    }
    stopSpeaking();
    const token = speechToken.current;
    try {
      const reply = await bridge.synthesizeSpeech(text);
      if (token !== speechToken.current) {
        return;
      }
      if (reply.audio_base64.length === 0) {
        throw new Error('The voice backend returned no audio.');
      }
      const audio = new Audio(
        `data:audio/${reply.format || 'wav'};base64,${reply.audio_base64}`,
      );
      audioRef.current = audio;
      setSpeakingId(messageId);
      audio.onended = () => {
        if (audioRef.current === audio) {
          audioRef.current = null;
          setSpeakingId(null);
        }
      };
      audio.onerror = () => {
        if (audioRef.current === audio) {
          audioRef.current = null;
          setSpeakingId(null);
          setError('The spoken reply could not be played.');
        }
      };
      await audio.play();
    } catch (cause) {
      if (token === speechToken.current) {
        stopSpeaking();
        setError(`Spoken reply failed: ${messageOf(cause)}`);
      }
    }
  }

  function toggleAutoVoice() {
    setAutoVoice((current) => {
      const next = !current;
      autoVoiceRef.current = next;
      window.localStorage.setItem('lfm3.voiceReplies', next ? '1' : '0');
      if (!next) {
        stopSpeaking();
      }
      return next;
    });
  }

  async function attachImage(file: File): Promise<string> {
    if (!bridge) {
      throw new Error('The local engine is not connected.');
    }
    const base64 = await readBase64(file);
    return bridge.attachImage(file.name, base64);
  }

  function newChat() {
    stopSpeaking();
    loadToken.current += 1;
    activeRef.current = null;
    setActiveId(null);
    setState(initialChatState);
    setError(null);
  }

  async function removeSession(sessionId: string) {
    if (!bridge) {
      return;
    }
    setError(null);
    try {
      await bridge.deleteChatSession(sessionId);
      const remaining = sessions.filter((session) => session.id !== sessionId);
      setSessions(remaining);
      if (activeRef.current === sessionId) {
        if (remaining.length > 0) {
          await loadSession(bridge, remaining[0].id);
        } else {
          newChat();
        }
      }
      if (turnSession === sessionId) {
        setTurnSession(null);
      }
    } catch (cause) {
      setError(messageOf(cause));
    }
  }

  async function renameSession(sessionId: string, title: string) {
    if (!bridge) {
      return;
    }
    setError(null);
    try {
      await bridge.renameChatSession(sessionId, title);
      setSessions((current) =>
        current.map((session) =>
          session.id === sessionId ? { ...session, title } : session,
        ),
      );
      if (activeId === sessionId && state.view) {
        setState((current) =>
          current.view
            ? { ...current, view: { ...current.view, session: { ...current.view.session, title } } }
            : current,
        );
      }
    } catch (cause) {
      setError(messageOf(cause));
    }
  }

  const view = state.view;
  const messages = view ? view.messages.filter((message) => message.role !== 'system') : [];
  const elsewhere = turnSession !== null && turnSession !== activeId;
  const running =
    state.running || turnSession !== null || messages.some((message) => message.status === 'streaming');
  const showEmpty = view === null && !loadingView;
  const hasTextModel = health === null || health.models.some((model) => model.key === 'text');
  const voiceOutput = health?.voice_output ?? null;
  const voiceAvailable = voiceOutput?.available ?? false;
  const shownError = error ?? state.error;

  return (
    <div className="chat-shell">
      <ChatSidebar
        sessions={sessions}
        activeId={activeId}
        health={health}
        onSelect={(sessionId) => {
          if (bridge) {
            void loadSession(bridge, sessionId);
          }
        }}
        onNew={newChat}
        onRename={(sessionId, title) => void renameSession(sessionId, title)}
        onDelete={(sessionId) => void removeSession(sessionId)}
        onOpenTasks={onOpenTasks}
      />

      <main className="chat-main">
        <header className="chat-header">
          <div>
            <p className="eyebrow">Local cowork harness</p>
            <h1>{view?.session.title ?? 'New conversation'}</h1>
          </div>
          <div className="badges">
            <span className="badge badge-local">Local only</span>
            <span className="badge badge-chat">Chat · not verified</span>
            {voiceAvailable ? (
              <button
                type="button"
                className={`badge badge-voice ${autoVoice ? 'badge-voice-on' : ''}`}
                aria-pressed={autoVoice}
                title={voiceOutput?.note ?? 'Read answers aloud with the local voice'}
                onClick={toggleAutoVoice}
              >
                {autoVoice ? 'Voice replies on' : 'Voice replies off'}
              </button>
            ) : null}
          </div>
        </header>

        {autoVoice && voiceOutput ? (
          <p className="voice-note" role="note">
            {voiceOutput.note}
          </p>
        ) : null}

        {shownError ? (
          <section className="chat-error" role="alert">
            <span>{shownError}</span>
            <button
              type="button"
              className="small"
              onClick={() => {
                setError(null);
                setState((current) => ({ ...current, error: null }));
              }}
            >
              Dismiss
            </button>
          </section>
        ) : null}

        {!hasTextModel ? (
          <p className="chat-warning" role="status">
            No text model is configured. Place the LFM3 GGUF in the models folder and restart the
            app before chatting.
          </p>
        ) : null}

        <div className="chat-scroll" ref={scrollRef}>
          {showEmpty ? (
            <div className="chat-empty">
              <p className="eyebrow">Chat, ask, or speak</p>
              <h2>What should we work on?</h2>
              <p className="muted">
                Route with the local decision model, search this folder, read and summarise files,
                trace related files, look at attached images with the vision model, and draft
                reports. Nothing is written until you approve the exact change, and nothing leaves
                this machine.
              </p>
              <div className="suggestions">
                {SUGGESTIONS.map((suggestion) => (
                  <button
                    key={suggestion}
                    type="button"
                    className="suggestion"
                    disabled={!bridge || sending}
                    onClick={() => void send(suggestion)}
                  >
                    {suggestion}
                  </button>
                ))}
              </div>
            </div>
          ) : null}

          {loadingView ? (
            <div className="chat-loading" role="status">
              Reading the conversation…
            </div>
          ) : null}

          {messages.map((message) => (
            <MessageBubble
              key={message.id}
              message={message}
              streaming={state.streaming[message.id]}
              actions={view?.actions ?? []}
              deciding={deciding}
              voiceAvailable={voiceAvailable}
              speaking={speakingId === message.id}
              onDecide={(actionId, allow) => void decide(actionId, allow)}
              onSpeak={(messageId, text) => void speakText(messageId, text)}
            />
          ))}

          {running && state.route ? (
            <div className="route-live" role="status">
              <span className={`route-intent route-${state.route.intent}`}>
                {state.route.intent}
              </span>
              <span className="route-source">
                {state.route.source === 'decision'
                  ? `decision model · ${(state.route.confidence_bps / 100).toFixed(0)}%`
                  : 'keyword route'}
              </span>
              <span className="route-roles">
                {state.route.roles
                  .filter((role) => role.used)
                  .map((role) => `${role.role}:${role.model}`)
                  .join(' · ')}
              </span>
            </div>
          ) : null}

          {running ? (
            <div className="chat-status" role="status">
              <span className="pulse" aria-hidden="true" />
              {statusLabel(state.status, elsewhere)}
            </div>
          ) : null}
        </div>

        <Composer
          disabled={!bridge || sending || loadingView || elsewhere}
          running={running}
          onSend={(text) => void send(text)}
          onStop={() => void stop()}
          onTranscribe={async (base64, format) => {
            if (!bridge) {
              throw new Error('The local engine is not connected.');
            }
            return bridge.transcribeVoice(base64, format);
          }}
          onAttachImage={attachImage}
          onError={setError}
        />
      </main>
    </div>
  );
}

function statusLabel(
  status: ChatUiState['status'],
  elsewhere: boolean,
): string {
  if (elsewhere) {
    return 'Another conversation is still replying. Stop it to send here.';
  }
  if (!status) {
    return 'Thinking locally…';
  }
  if (status.state === 'waiting_for_approval') {
    return status.detail ? `Waiting for your approval: ${status.detail}` : 'Waiting for your approval.';
  }
  if (status.state === 'routing') {
    return status.detail ? `Choosing a route: ${status.detail}` : 'Consulting the decision model…';
  }
  if (status.state === 'tool' || status.state === 'applying') {
    return status.detail ?? 'Working in the workspace…';
  }
  return status.detail ? `${humanize(status.state)}: ${status.detail}` : humanize(status.state);
}

function humanize(value: string): string {
  const clean = value.replace(/_/g, ' ');
  return clean.charAt(0).toUpperCase() + clean.slice(1);
}

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

function readBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = typeof reader.result === 'string' ? reader.result : '';
      const comma = result.indexOf(',');
      if (comma < 0) {
        reject(new Error('The image could not be read.'));
        return;
      }
      resolve(result.slice(comma + 1));
    };
    reader.onerror = () => {
      reject(reader.error ?? new Error('The image could not be read.'));
    };
    reader.readAsDataURL(file);
  });
}
