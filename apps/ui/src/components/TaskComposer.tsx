import { useState } from 'react';

import type { TaskRoute } from '../bridge/types';

export interface TaskComposerProps {
  busy?: boolean;
  demo?: boolean;
  route: TaskRoute | null;
  onStart(goal: string, deliverable: string, minutes: number, maxTurns: number): void;
}

/**
 * One stated job, driven turn by turn by the local engine.
 *
 * There is no plan to approve because nothing is frozen: the acting model reads
 * the workspace, writes its own todo list, and keeps going until the required
 * output exists or the budget runs out. Every turn is journaled. The routing
 * chips show which local models the job will consult.
 */
export function TaskComposer({ busy = false, demo = false, route, onStart }: TaskComposerProps) {
  const [goal, setGoal] = useState('');
  const [deliverable, setDeliverable] = useState('');
  const [minutes, setMinutes] = useState('30');
  const [turns, setTurns] = useState('24');
  const blocked = busy || demo || goal.trim().length === 0;

  return (
    <section className="panel" aria-labelledby="task-composer-title">
      <h2 id="task-composer-title">Describe a job</h2>
      <p className="muted">
        The engine hands the goal to the local text model and lets it work turn by turn. It first
        writes a short plan with a check per step, then takes one bounded action at a time. A step
        is only marked done when its check passes against the real workspace; after writing the
        output the model re-reads it before finishing. It stops when the output is proven, when the
        budget runs out, or when you stop it. Nothing is pre-approved and nothing is claimed as
        verified work.
      </p>
      <form
        aria-label="General task"
        onSubmit={(event) => {
          event.preventDefault();
          if (!blocked) {
            onStart(
              goal.trim(),
              deliverable.trim(),
              clampNumber(minutes, 1, 1440, 30),
              clampNumber(turns, 1, 64, 24),
            );
          }
        }}
      >
        <div className="field">
          <label htmlFor="task-goal">Job</label>
          <textarea
            id="task-goal"
            rows={4}
            value={goal}
            spellCheck={false}
            placeholder="Rewrite every note in notes/ into a single report at out/report.md, one section per note, and keep every figure the sources state"
            onChange={(event) => setGoal(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor="task-deliverable">Required output (optional)</label>
          <input
            id="task-deliverable"
            value={deliverable}
            spellCheck={false}
            placeholder="out/report.md"
            onChange={(event) => setDeliverable(event.target.value)}
          />
          <p className="hint">
            The job is only passed when this file exists with real bytes; the journal records its
            size and digest.
          </p>
        </div>
        <div className="field">
          <label htmlFor="task-minutes">Job budget (minutes)</label>
          <input
            id="task-minutes"
            type="number"
            min={1}
            max={1440}
            value={minutes}
            onChange={(event) => setMinutes(event.target.value)}
          />
          <p className="hint">The job is cancelled when this deadline passes, never marked done.</p>
        </div>
        <div className="field">
          <label htmlFor="task-turns">Maximum turns</label>
          <input
            id="task-turns"
            type="number"
            min={1}
            max={64}
            value={turns}
            onChange={(event) => setTurns(event.target.value)}
          />
          <p className="hint">
            Each turn is one model decision with up to three tool calls. The loop stops here even
            if the output is unfinished.
          </p>
        </div>
        <div className="actions">
          <button type="submit" className="primary" disabled={blocked}>
            {busy ? 'Starting…' : demo ? 'Desktop app only' : 'Start job'}
          </button>
        </div>
      </form>

      {demo ? (
        <p className="notice">
          Demo fixtures describe verified runs only. Start a job in the desktop app.
        </p>
      ) : null}

      {route ? (
        <div aria-label="Model routing">
          <p className="label">Routed models</p>
          <p className="chips">
            {route.roles.map((entry) => (
              <span
                key={entry.role}
                className={entry.used ? 'chip' : 'chip muted'}
                title={entry.reason ?? entry.capability}
              >
                {entry.role} · {entry.model}
                {entry.used ? '' : ' (idle)'}
              </span>
            ))}
          </p>
        </div>
      ) : null}
    </section>
  );
}

function clampNumber(raw: string, minimum: number, maximum: number, fallback: number): number {
  const value = Number.parseInt(raw, 10);
  if (!Number.isFinite(value)) {
    return fallback;
  }
  return Math.min(Math.max(value, minimum), maximum);
}
