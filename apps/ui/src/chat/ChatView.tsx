import { useEffect, useRef, useState } from 'react';

import { createBridge } from '../bridge/createBridge';
import type { EngineBridge, Unsubscribe } from '../bridge/EngineBridge';
import type {
  ChatEvent,
  ChatSession,
  DownloadEvent,
  HealthReport,
  ModelCatalog,
  ModelKey,
} from '../bridge/types';
import { ChatModelMenu, type DownloadProgress } from './ChatModelMenu';
import { ChatSidebar } from './ChatSidebar';
import { Composer } from './Composer';
import { MessageBubble } from './MessageBubble';
import { WorkspaceSwitcher } from './WorkspaceSwitcher';
import {
  applyChatEvent,
  initialChatState,
  mergeView,
  type ChatUiState,
} from './reducer';

/** Exact logical viewport of a phone-sized chat screen (iPhone 13/14). */
const PHONE_WIDTH = 390;
const PHONE_HEIGHT = 844;
const PHONE_STAGE_BREAKPOINT = 720;
const PHONE_STORAGE_KEY = 'lfm3.phoneView';

const SUGGESTIONS = [
  'Summarise what this project does and where I should start.',
  'Find the file that handles a feature and explain how it works.',
  'Turn my notes into a clear report I can share.',
  'Look at an image in this folder and describe what it shows.',
];

/** Short labels for the two-column mobile starter cards. */
const PHONE_SUGGESTIONS = [
  { label: 'Summarise the project', prompt: SUGGESTIONS[0] },
  { label: 'Explain a file', prompt: SUGGESTIONS[1] },
  { label: 'Draft a report', prompt: SUGGESTIONS[2] },
  { label: 'Describe an image', prompt: SUGGESTIONS[3] },
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
  const [menuOpen, setMenuOpen] = useState(false);
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [downloading, setDownloading] = useState<ModelKey | null>(null);
  const [progress, setProgress] = useState<DownloadProgress | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [switchingFolder, setSwitchingFolder] = useState(false);
  const [phoneView, setPhoneView] = useState(
    () => window.localStorage.getItem(PHONE_STORAGE_KEY) === '1',
  );
  const [phoneScale, setPhoneScale] = useState(phoneFitScale);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const activeRef = useRef<string | null>(null);
  const loadToken = useRef(0);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const autoVoiceRef = useRef(false);
  const speechToken = useRef(0);
  /** Guards a slow catalog response from clearing a newer download. */
  const downloadEpoch = useRef(0);

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
    if (!bridge) {
      return;
    }
    let cancelled = false;
    let unsubscribe: Unsubscribe | null = null;
    bridge
      .subscribeDownloads((event) => {
        if (!cancelled) {
          handleDownloadEvent(bridge, event);
        }
      })
      .then((off) => {
        if (cancelled) {
          off();
        } else {
          unsubscribe = off;
        }
      })
      .catch(() => {
        // Downloads still work; the catalog shows the final state on refresh.
      });
    return () => {
      cancelled = true;
      unsubscribe?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bridge]);

  useEffect(() => {
    if (!menuOpen && !phoneView && !drawerOpen) {
      return;
    }
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') {
        return;
      }
      if (drawerOpen) {
        setDrawerOpen(false);
      } else if (menuOpen) {
        setMenuOpen(false);
      } else {
        exitPhoneView();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
    // exitPhoneView only touches setters and localStorage.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [menuOpen, phoneView, drawerOpen]);

  useEffect(() => {
    if (!phoneView) {
      setPhoneScale(1);
      return;
    }
    const updateScale = () => setPhoneScale(phoneFitScale());
    updateScale();
    window.addEventListener('resize', updateScale);
    return () => window.removeEventListener('resize', updateScale);
  }, [phoneView]);

  useEffect(() => {
    const query = window.matchMedia('(max-width: 900px)');
    const closeDrawer = () => {
      if (!query.matches) {
        setDrawerOpen(false);
      }
    };
    closeDrawer();
    query.addEventListener('change', closeDrawer);
    return () => query.removeEventListener('change', closeDrawer);
  }, []);

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

  async function refreshHealth(engine: EngineBridge) {
    try {
      setHealth(await engine.health());
    } catch {
      // Keep the last known report.
    }
  }

  async function refreshCatalog(engine: EngineBridge) {
    const epoch = downloadEpoch.current;
    setRefreshing(true);
    try {
      const next = await engine.modelCatalog();
      setCatalog(next);
      if (next.download !== null) {
        // The menu may open in the middle of a download started earlier.
        setDownloading((current) => current ?? next.download);
      } else if (downloadEpoch.current === epoch) {
        setDownloading(null);
        setProgress(null);
      }
    } catch (cause) {
      setCatalogError(messageOf(cause));
    } finally {
      setRefreshing(false);
    }
  }

  function toggleMenu() {
    const next = !menuOpen;
    setMenuOpen(next);
    setCatalogError(null);
    if (next && bridge) {
      void refreshCatalog(bridge);
    }
  }

  function closeMenu() {
    setMenuOpen(false);
  }

  function togglePhoneView() {
    const next = !phoneView;
    setPhoneView(next);
    setDrawerOpen(false);
    window.localStorage.setItem(PHONE_STORAGE_KEY, next ? '1' : '0');
    if (next) {
      setMenuOpen(false);
    }
  }

  function exitPhoneView() {
    setPhoneView(false);
    setDrawerOpen(false);
    window.localStorage.setItem(PHONE_STORAGE_KEY, '0');
  }

  function handleDownloadEvent(engine: EngineBridge, event: DownloadEvent) {
    if (event.kind === 'started') {
      downloadEpoch.current += 1;
      setDownloading(event.key);
      setProgress({
        key: event.key,
        file: '',
        received: 0,
        fileTotal: 0,
        overallReceived: 0,
        overallTotal: event.total_bytes,
      });
      setCatalogError(null);
      return;
    }
    if (event.kind === 'progress') {
      setProgress({
        key: event.key,
        file: event.file,
        received: event.received,
        fileTotal: event.file_total,
        overallReceived: event.overall_received,
        overallTotal: event.overall_total,
      });
      return;
    }
    if (event.kind === 'done') {
      downloadEpoch.current += 1;
      setDownloading(null);
      setProgress(null);
      void refreshCatalog(engine);
      void refreshHealth(engine);
      return;
    }
    if (event.kind === 'failed') {
      downloadEpoch.current += 1;
      setDownloading(null);
      setProgress(null);
      setCatalogError(event.message);
      void refreshCatalog(engine);
      return;
    }
    if (event.kind === 'cancelled') {
      downloadEpoch.current += 1;
      setDownloading(null);
      setProgress(null);
      void refreshCatalog(engine);
    }
  }

  async function startDownload(key: ModelKey) {
    if (!bridge) {
      return;
    }
    setCatalogError(null);
    try {
      await bridge.downloadModel(key);
    } catch (cause) {
      setCatalogError(messageOf(cause));
    }
  }

  async function stopDownload(key: ModelKey) {
    if (!bridge) {
      return;
    }
    try {
      await bridge.cancelDownload(key);
    } catch (cause) {
      setCatalogError(messageOf(cause));
    }
  }

  async function reloadInstalled() {
    if (!bridge) {
      return;
    }
    setCatalogError(null);
    try {
      const models = await bridge.reloadModels();
      setHealth((current) => (current ? { ...current, models } : current));
      await refreshCatalog(bridge);
    } catch (cause) {
      setCatalogError(messageOf(cause));
    }
  }

  async function pickWorkspace() {
    if (!bridge) {
      return;
    }
    setSwitchingFolder(true);
    setError(null);
    try {
      const path = await bridge.chooseWorkspace();
      if (path) {
        setHealth((current) => (current ? { ...current, workspace: path } : current));
        await refreshCatalog(bridge);
      }
    } catch (cause) {
      setError(messageOf(cause));
    } finally {
      setSwitchingFolder(false);
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
  const textModel = health?.models.find((model) => model.key === 'text') ?? null;
  const hasTextModel =
    health === null || textModel?.installed === true || textModel?.state === 'ready';
  const workspaceLabel = health?.workspace ? folderName(health.workspace) : 'Not set';
  const missingModels =
    catalog !== null
      ? catalog.models.filter((model) => !model.installed).length
      : (health?.models.filter((model) => model.state === 'absent').length ?? 0);
  const voiceOutput = health?.voice_output ?? null;
  const voiceAvailable = voiceOutput?.available ?? false;
  const shownError = error ?? state.error;
  const workspaceTitle = running
    ? 'Stop the current reply before changing folders.'
    : (health?.workspace ?? 'Choose the folder chat and jobs work in');

  const modelPicker = (
    <div className="models-anchor">
      <button
        type="button"
        className={`model-picker ${menuOpen ? 'model-picker-open' : ''}`}
        aria-haspopup="dialog"
        aria-expanded={menuOpen}
        title="Choose or download the local model"
        onClick={toggleMenu}
      >
        <span
          className={`model-picker-dot model-${textModel?.state ?? 'absent'}`}
          aria-hidden="true"
        />
        <span className="model-picker-name">{textModel?.model ?? 'Local model'}</span>
        <span className="model-picker-caret" aria-hidden="true">
          ▾
        </span>
        {missingModels > 0 ? (
          <span className="models-count" aria-label={`${missingModels} not installed`}>
            {missingModels}
          </span>
        ) : null}
      </button>
      {menuOpen ? (
        <>
          <button
            type="button"
            className="model-menu-backdrop"
            aria-label="Close models"
            tabIndex={-1}
            onClick={closeMenu}
          />
          <ChatModelMenu
            catalog={catalog}
            downloading={downloading}
            progress={progress}
            error={catalogError}
            refreshing={refreshing}
            busy={running}
            busyReason="Stop the current reply before downloading or loading models."
            onRefresh={() => {
              if (bridge) {
                void refreshCatalog(bridge);
              }
            }}
            onDownload={(key) => void startDownload(key)}
            onCancel={(key) => void stopDownload(key)}
            onReload={() => void reloadInstalled()}
            onClose={closeMenu}
          />
        </>
      ) : null}
    </div>
  );

  const renderSidebar = () => (
    <ChatSidebar
      sessions={sessions}
      activeId={activeId}
      health={health}
      workspaceLabel={switchingFolder ? 'Choosing…' : workspaceLabel}
      workspaceTitle={workspaceTitle}
      workspaceDisabled={!bridge || switchingFolder || running}
      workspaceBusy={switchingFolder}
      onChooseWorkspace={() => void pickWorkspace()}
      onSelect={(sessionId) => {
        if (bridge) {
          void loadSession(bridge, sessionId);
        }
        setDrawerOpen(false);
      }}
      onNew={() => {
        newChat();
        setDrawerOpen(false);
      }}
      onRename={(sessionId, title) => void renameSession(sessionId, title)}
      onDelete={(sessionId) => void removeSession(sessionId)}
      onOpenTasks={() => {
        if (phoneView) {
          exitPhoneView();
        }
        onOpenTasks();
      }}
    />
  );

  const shell = (
    <div className="chat-shell">
      {phoneView ? null : renderSidebar()}

      <main className="chat-main">
        {drawerOpen ? (
          <>
            <button
              type="button"
              className="phone-drawer-scrim"
              aria-label="Close conversations"
              tabIndex={-1}
              onClick={() => setDrawerOpen(false)}
            />
            <aside className="phone-drawer" aria-label="Conversations">
              {renderSidebar()}
            </aside>
          </>
        ) : null}

        <header className="chat-header">
          {phoneView ? (
            <div className="phone-header">
              <div className="phone-appbar">
                <button
                  type="button"
                  className="phone-icon-button"
                  aria-label="Open conversations"
                  aria-expanded={drawerOpen}
                  onClick={() => setDrawerOpen(true)}
                >
                  <MenuIcon />
                </button>
                <h1>{view?.session.title ?? 'New chat'}</h1>
                <button
                  type="button"
                  className="phone-icon-button"
                  aria-label="New chat"
                  onClick={() => {
                    newChat();
                    setDrawerOpen(false);
                  }}
                >
                  <ComposeIcon />
                </button>
              </div>
            </div>
          ) : (
            <>
              <button
                type="button"
                className="sidebar-toggle"
                aria-label="Open conversations"
                aria-expanded={drawerOpen}
                onClick={() => {
                  setDrawerOpen(true);
                  setMenuOpen(false);
                }}
              >
                <MenuIcon />
              </button>
              <div className="chat-heading">
                <p className="eyebrow">Private · on this computer</p>
                <div className="chat-title-row">
                  <h1>{view?.session.title ?? 'New chat'}</h1>
                  {modelPicker}
                </div>
              </div>
              <div className="header-tools">
            <button
              type="button"
              className="phone-toggle"
              aria-pressed={phoneView}
              title="Show only the chat in an exact phone-sized screen"
              onClick={togglePhoneView}
            >
              <span className="phone-glyph" aria-hidden="true">
                ▯
              </span>
              Mobile
            </button>

            <div className="badges">
              <span className="badge badge-local">On this computer</span>
              <span
                className="badge badge-chat"
                title="A conversation, not checked work. Use Tasks for runs the app verifies."
              >
                Chat
              </span>
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
          </div>
            </>
          )}
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
            The writing model isn’t set up yet. Open the <strong>model menu</strong> to download it,
            or add a model file to the models folder and reload.
          </p>
        ) : null}

        <div className="chat-scroll" ref={scrollRef}>
          {showEmpty ? (
            <div className={`chat-empty ${phoneView ? 'phone-empty' : ''}`}>
              {phoneView ? (
                <span className="phone-empty-brand">LFM3</span>
              ) : (
                <p className="eyebrow">Your local assistant</p>
              )}
              <h2>What can I help you with?</h2>
              <p className="muted">
                {phoneView
                  ? 'Ask a question, or describe a job. I can read this folder and show you every change before it happens.'
                  : 'Ask a question or describe something you’d like done. I can read and search the folder you chose, summarise files, look at images, and draft reports. I’ll show you any change before it happens, and nothing leaves this computer.'}
              </p>
              {phoneView ? (
                <div className="phone-suggestions">
                  {PHONE_SUGGESTIONS.map((suggestion) => (
                    <button
                      key={suggestion.label}
                      type="button"
                      className="phone-suggestion"
                      disabled={!bridge || sending}
                      onClick={() => void send(suggestion.prompt)}
                    >
                      <span>{suggestion.label}</span>
                      <span className="phone-suggestion-arrow" aria-hidden="true">
                        ↗
                      </span>
                    </button>
                  ))}
                </div>
              ) : (
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
              )}
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

          {state.decision ? (
            <div
              className={`decision-chip decision-${state.decision.outcome}`}
              title={state.decision.reason}
            >
              <span className="decision-point">
                {state.decision.point === 'write_gate' ? 'write preflight' : 'evidence check'}
              </span>
              <span className="decision-outcome">{state.decision.outcome}</span>
              <span className="decision-detail">
                {state.decision.label.replace(/_/g, ' ')} ·{' '}
                {(state.decision.confidence_bps / 100).toFixed(0)}%
                {state.decision.target ? ` · ${state.decision.target}` : ''}
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

        <div className="composer-dock">
          {phoneView ? (
            <div className="phone-composer-toolbar">
              {modelPicker}
              <WorkspaceSwitcher
                label={
                  switchingFolder
                    ? 'Choosing…'
                    : health?.workspace
                      ? workspaceLabel
                      : 'Choose folder'
                }
                title={workspaceTitle}
                disabled={!bridge || switchingFolder || running}
                busy={switchingFolder}
                compact
                onChange={() => void pickWorkspace()}
              />
            </div>
          ) : null}
          <Composer
            disabled={!bridge || sending || loadingView || elsewhere}
            running={running}
            rows={phoneView ? 1 : 3}
            placeholder={phoneView ? 'Ask anything…' : undefined}
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
        </div>
      </main>
    </div>
  );

  if (phoneView) {
    return (
      <div className="phone-stage">
        <button type="button" className="phone-stage-exit" onClick={exitPhoneView}>
          Exit mobile view
        </button>
        <div
          className="phone-scale-box"
          style={{
            width: PHONE_WIDTH * phoneScale,
            height: PHONE_HEIGHT * phoneScale,
          }}
        >
          <div
            className="phone-frame"
            role="region"
            aria-label="Mobile chat preview"
            style={{ transform: `scale(${phoneScale})` }}
          >
            {shell}
          </div>
        </div>
      </div>
    );
  }

  return shell;
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
  if (status.state === 'preflight') {
    return status.detail ?? 'Checking the proposed change against your request…';
  }
  if (status.state === 'checking_evidence') {
    return status.detail ?? 'Checking whether the observations answer your request…';
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

function phoneFitScale(): number {
  // The frame keeps its exact 390x844 logical size. Reserve a small margin for
  // the frame ring and only ever shrink, so the whole phone stays on screen.
  // Narrow stages also reserve a bar for the exit button above the phone.
  const stacked = window.innerWidth <= PHONE_STAGE_BREAKPOINT;
  const verticalMargin = stacked ? 84 : 32;
  const fit = Math.min(
    (window.innerWidth - 32) / PHONE_WIDTH,
    (window.innerHeight - verticalMargin) / PHONE_HEIGHT,
  );
  return Math.min(1, Math.max(fit, 0.01));
}

function folderName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, '');
  const parts = trimmed.split(/[\\/]/);
  return parts.at(-1) || path;
}

function MenuIcon() {
  return (
    <svg viewBox="0 0 18 18" width="18" height="18" fill="none" aria-hidden="true">
      <path
        d="M2.5 5h13M2.5 9h13M2.5 13h13"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
      />
    </svg>
  );
}

function ComposeIcon() {
  return (
    <svg viewBox="0 0 20 20" width="19" height="19" fill="none" aria-hidden="true">
      <path
        d="M11.2 3.6H5a2 2 0 0 0-2 2v9.4a2 2 0 0 0 2 2h9.4a2 2 0 0 0 2-2V8.7"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
      />
      <path
        d="m14.1 2.6 3.3 3.3-7.7 7.7-4 .8.8-4 7.6-7.8Z"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinejoin="round"
      />
    </svg>
  );
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
