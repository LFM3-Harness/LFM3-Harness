import { useRef, useState } from 'react';

import type { ChatSession, HealthReport, ModelState, ModelStatus } from '../bridge/types';
import { WorkspaceSwitcher } from './WorkspaceSwitcher';

export function ChatSidebar({
  sessions,
  activeId,
  health,
  workspaceLabel,
  workspaceTitle,
  workspaceDisabled,
  workspaceBusy,
  onChooseWorkspace,
  onSelect,
  onNew,
  onRename,
  onDelete,
  onOpenTasks,
}: {
  sessions: ChatSession[];
  activeId: string | null;
  health: HealthReport | null;
  workspaceLabel: string;
  workspaceTitle: string;
  workspaceDisabled: boolean;
  workspaceBusy: boolean;
  onChooseWorkspace: () => void;
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
        <span className="brand-sub">Your local coworker</span>
      </div>

      <WorkspaceSwitcher
        label={workspaceLabel}
        title={workspaceTitle}
        disabled={workspaceDisabled}
        busy={workspaceBusy}
        onChange={onChooseWorkspace}
      />

      <button type="button" className="new-chat" onClick={onNew}>
        <span aria-hidden="true">+</span> New chat
      </button>

      <nav className="session-list" aria-label="Chat history">
        {sessions.length === 0 ? (
          <p className="sidebar-empty">No chats yet. Everything you do here stays on this computer.</p>
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
          Tasks
          <span className="tasks-hint">Guided work, checked step by step</span>
        </button>
        <div className="model-badges" aria-label="Local model status">
          <span
            className={`model-chip model-${textModel?.state ?? 'absent'}`}
            title="The writing model answers questions and edits files."
          >
            <span className="dot" aria-hidden="true" />
            Writing {stateLabel(textModel)}
          </span>
          <span
            className={`model-chip model-${visionModel?.state ?? 'absent'}`}
            title="Reads attached and workspace images."
          >
            <span className="dot" aria-hidden="true" />
            Images {stateLabel(visionModel)}
          </span>
          {audioModel ? (
            <span
              className={`model-chip model-${audioModel.state}`}
              title="Turns your voice into text, locally."
            >
              <span className="dot" aria-hidden="true" />
              Voice input {stateLabel(audioModel)}
            </span>
          ) : (
            <span className="model-chip model-absent" title="Add the LFM3 audio model to speak instead of type.">
              <span className="dot" aria-hidden="true" />
              Voice input not set up
            </span>
          )}
          <span
            className={`model-chip model-${decisionModel?.state ?? 'absent'}`}
            title="Chooses which local model should handle each message."
          >
            <span className="dot" aria-hidden="true" />
            Router {stateLabel(decisionModel)}
          </span>
          <span
            className={`model-chip ${voiceOutput?.available ? 'model-ready' : 'model-absent'}`}
            title={voiceOutput?.note ?? 'Spoken replies need a local voice.'}
          >
            <span className="dot" aria-hidden="true" />
            Voice replies{' '}
            {voiceOutput?.backend === 'lfm3-tts'
              ? 'ready'
              : voiceOutput?.available
                ? 'ready'
                : 'not set up'}
          </span>
        </div>
        {health ? (
          <p className="sidebar-note">
            Runs offline with llama.cpp {health.llama.release || health.llama.build || 'pinned'}
          </p>
        ) : null}
      </div>
    </aside>
  );
}

function stateLabel(model: ModelStatus | null): string {
  const state: ModelState | undefined = model?.state;
  if (state === 'ready' || state === 'installed') {
    return 'ready';
  }
  if (state === 'loading') {
    return 'starting';
  }
  if (state === 'evicted') {
    return 'idle';
  }
  if (state === 'error') {
    return 'needs attention';
  }
  return 'not set up';
}
