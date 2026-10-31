import { useRef, useState } from 'react';

import type { ChatSession, HealthReport } from '../bridge/types';

export function ChatSidebar({
  sessions,
  activeId,
  health,
  onSelect,
  onNew,
  onRename,
  onDelete,
  onOpenTasks,
}: {
  sessions: ChatSession[];
  activeId: string | null;
  health: HealthReport | null;
  onSelect: (sessionId: string) => void;
  onNew: () => void;
  onRename: (sessionId: string, title: string) => void;
  onDelete: (sessionId: string) => void;
  onOpenTasks: () => void;
}) {
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState('');
  const [confirming, setConfirming] = useState<string | null>(null);
  const editingRef = useRef<string | null>(null);

  function beginRename(session: ChatSession) {
    editingRef.current = session.id;
    setEditing(session.id);
    setDraft(session.title);
    setConfirming(null);
  }

  function commitRename() {
    const sessionId = editingRef.current;
    if (!sessionId) {
      return;
    }
    editingRef.current = null;
    setEditing(null);
    const title = draft.trim();
    if (title.length > 0) {
      onRename(sessionId, title);
    }
  }

  function cancelRename() {
    editingRef.current = null;
    setEditing(null);
  }

  const textModel = health?.models.find((model) => model.key === 'text') ?? null;
  const audioModel = health?.models.find((model) => model.key === 'audio') ?? null;
  const visionModel = health?.models.find((model) => model.key === 'vision') ?? null;
  const decisionModel = health?.models.find((model) => model.key === 'd1') ?? null;
  const voiceOutput = health?.voice_output ?? null;

  return (
    <aside className="chat-sidebar" aria-label="Conversations">
      <div className="sidebar-brand">
        <span className="brand-mark">LFM3</span>
        <span className="brand-sub">Local cowork</span>
      </div>

      <button type="button" className="new-chat" onClick={onNew}>
        <span aria-hidden="true">+</span> New chat
      </button>

      <nav className="session-list" aria-label="Chat history">
        {sessions.length === 0 ? (
          <p className="sidebar-empty">No conversations yet. Your chats stay on this machine.</p>
        ) : (
          sessions.map((session) => {
            const active = session.id === activeId;
            return (
              <div
                key={session.id}
                className={`session-row ${active ? 'session-active' : ''}`}
                onMouseLeave={() => setConfirming((current) => (current === session.id ? null : current))}
              >
                {editing === session.id ? (
                  <input
                    className="session-input"
                    value={draft}
                    autoFocus
                    maxLength={80}
                    aria-label="Conversation title"
                    onChange={(event) => setDraft(event.target.value)}
                    onBlur={commitRename}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter') {
                        event.preventDefault();
                        commitRename();
                      } else if (event.key === 'Escape') {
                        cancelRename();
                      }
                    }}
                  />
                ) : (
                  <button
                    type="button"
                    className="session-title"
                    aria-current={active ? 'page' : undefined}
                    title={session.title}
                    onClick={() => onSelect(session.id)}
                    onDoubleClick={() => beginRename(session)}
                  >
                    {session.title}
                  </button>
                )}
                <button
                  type="button"
                  className="session-edit"
                  aria-label={`Rename ${session.title}`}
                  title="Rename"
                  onClick={() => beginRename(session)}
                >
                  ✎
                </button>
                <button
                  type="button"
                  className={`session-delete ${confirming === session.id ? 'confirming' : ''}`}
                  aria-label={
                    confirming === session.id
                      ? `Confirm delete ${session.title}`
                      : `Delete ${session.title}`
                  }
                  title={confirming === session.id ? 'Click again to delete' : 'Delete conversation'}
                  onClick={() => {
                    if (confirming === session.id) {
                      setConfirming(null);
                      onDelete(session.id);
                    } else {
                      setConfirming(session.id);
                    }
                  }}
                >
                  {confirming === session.id ? 'Delete?' : '×'}
                </button>
              </div>
            );
          })
        )}
      </nav>

      <div className="sidebar-foot">
        <button type="button" className="tasks-link" onClick={onOpenTasks}>
          <span className="tasks-icon" aria-hidden="true">
            ✓
          </span>
          Verified tasks
          <span className="tasks-hint">runs with frozen acceptance</span>
        </button>
        <div className="model-badges" aria-label="Local model status">
          <span className={`model-chip model-${textModel?.state ?? 'absent'}`}>
            <span className="dot" aria-hidden="true" />
            text {textModel?.state ?? 'offline'}
          </span>
          <span
            className={`model-chip model-${visionModel?.state ?? 'absent'}`}
            title="Reads attached and workspace images with the vision model."
          >
            <span className="dot" aria-hidden="true" />
            vision {visionModel?.state ?? 'offline'}
          </span>
          {audioModel ? (
            <span className={`model-chip model-${audioModel.state}`}>
              <span className="dot" aria-hidden="true" />
              voice in {audioModel.state}
            </span>
          ) : (
            <span className="model-chip model-absent" title="Add LFM3 audio weights and an mmproj to enable voice input.">
              <span className="dot" aria-hidden="true" />
              voice in unavailable
            </span>
          )}
          <span
            className={`model-chip model-${decisionModel?.state ?? 'absent'}`}
            title="Chooses each turn's route with the decision model."
          >
            <span className="dot" aria-hidden="true" />
            decision {decisionModel?.state ?? 'offline'}
          </span>
          <span
            className={`model-chip ${voiceOutput?.available ? 'model-ready' : 'model-absent'}`}
            title={voiceOutput?.note ?? 'Spoken replies need a local voice.'}
          >
            <span className="dot" aria-hidden="true" />
            voice out{' '}
            {voiceOutput?.backend === 'lfm3-tts'
              ? 'LFM3'
              : voiceOutput?.available
                ? 'system'
                : 'unavailable'}
          </span>
          <span className="model-chip model-local">
            <span className="dot" aria-hidden="true" />
            loopback only
          </span>
        </div>
        {health ? (
          <p className="sidebar-note">
            llama.cpp {health.llama.release || health.llama.build || 'pinned'} · v{health.version}
          </p>
        ) : null}
      </div>
    </aside>
  );
}
