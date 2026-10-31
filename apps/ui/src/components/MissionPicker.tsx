import type { InputSpec, MissionManifest } from '../bridge/types';

/**
 * The engine refuses to start a mission whose semantic criteria are not
 * holdout-qualified. The UI states the same reason up front instead of
 * offering a Start button that would fail.
 */
export function semanticReviewReason(mission: MissionManifest): string | null {
  const policy = mission.semantic;
  if (!policy || policy.mode !== 'human_review_only') {
    return null;
  }
  return (
    policy.reason?.trim() ||
    'Semantic criteria need human review for this mission; automatic passing is disabled.'
  );
}

export interface MissionPickerProps {
  missions: MissionManifest[];
  selected: string | null;
  inputs: Record<string, string>;
  busy?: boolean;
  demo?: boolean;
  onSelect(name: string): void;
  onInputChange(key: string, value: string): void;
  onChooseFile?(mission: string, key: string): void;
  onStart(): void;
}

/**
 * Specialized mission selection. There is deliberately no free-form chat
 * field: a run starts from a mission's declared inputs and frozen contract.
 */
export function MissionPicker({
  missions,
  selected,
  inputs,
  busy = false,
  demo = false,
  onSelect,
  onInputChange,
  onChooseFile,
  onStart,
}: MissionPickerProps) {
  const mission = missions.find((entry) => entry.name === selected) ?? null;
  const missing = mission
    ? Object.entries(mission.inputs)
        .filter(([key, spec]) => spec.required && !(inputs[key] ?? '').trim())
        .map(([key]) => key)
    : [];
  const review = mission ? semanticReviewReason(mission) : null;
  const blocked = mission === null || missing.length > 0 || busy || review !== null;

  return (
    <section className="panel" aria-labelledby="mission-picker-title">
      <h2 id="mission-picker-title">Missions</h2>
      <p className="muted">
        Choose one specialized mission. Every run is bounded by its declared tools, checks,
        and budgets, never by free-form chat.
      </p>

      <div className="mission-grid">
        {missions.map((entry) => (
          <button
            className="mission mission-button"
            key={entry.name}
            type="button"
            aria-pressed={entry.name === selected}
            aria-label={`${entry.title} (${entry.name} v${entry.version})`}
            onClick={() => onSelect(entry.name)}
          >
            <header>
              <h3>{entry.title}</h3>
              <span className="mono muted">
                {entry.name} v{entry.version}
              </span>
            </header>
            <p>{entry.description}</p>
            {entry.semantic?.mode === 'human_review_only' ? (
              <p className="chips">
                <span className="chip warning">human review only</span>
              </p>
            ) : null}
            <p className="label">Tools</p>
            <p className="chips">
              {entry.tools.map((tool) => (
                <span className="chip" key={tool}>
                  {tool}
                </span>
              ))}
            </p>
            <p className="label">Checks</p>
            <p className="chips">
              {entry.checks.map((check) => (
                <span className="chip" key={check}>
                  {check}
                </span>
              ))}
            </p>
            <p className="label">Required output</p>
            <p className="mono">
              {entry.output.required_artifacts.length === 0
                ? 'none'
                : entry.output.required_artifacts.map((artifact) => artifact.path).join(', ')}
            </p>
          </button>
        ))}
      </div>

      {mission ? (
        <form
          className="mission-form"
          aria-label={`${mission.title} inputs`}
          onSubmit={(event) => {
            event.preventDefault();
            if (!blocked) {
              onStart();
            }
          }}
        >
          <h3>Run inputs</h3>
          {Object.entries(mission.inputs).map(([key, spec]) => (
            <MissionInput
              key={key}
              inputKey={key}
              spec={spec}
              value={inputs[key] ?? ''}
              onChange={(value) => onInputChange(key, value)}
              onChooseFile={onChooseFile ? () => onChooseFile(mission.name, key) : undefined}
            />
          ))}
          {review ? (
            <p className="notice" role="status">
              This mission cannot start: {review}
            </p>
          ) : null}
          {missing.length > 0 ? (
            <p className="notice">
              Required input missing: {missing.join(', ')}. The run cannot start without it.
            </p>
          ) : null}
          <div className="actions">
            <button type="submit" className="primary" disabled={blocked}>
              {busy ? 'Starting…' : demo ? 'Create demo run' : 'Create run'}
            </button>
            <span className="muted mono">
              {mission.limits.max_steps} steps · {mission.limits.max_attempts} attempts
            </span>
          </div>
        </form>
      ) : (
        <p className="muted">Select a mission to see its inputs and verification methods.</p>
      )}
    </section>
  );
}

function MissionInput({
  inputKey,
  spec,
  value,
  onChange,
  onChooseFile,
}: {
  inputKey: string;
  spec: InputSpec;
  value: string;
  onChange(value: string): void;
  onChooseFile?: () => void;
}) {
  const label = inputKey
    .replaceAll('_', ' ')
    .replace(/^\w/, (letter) => letter.toUpperCase());
  if (spec.kind === 'image' || spec.kind === 'audio' || spec.kind === 'file') {
    return (
      <div className="field">
        <span className="field-label">{label}</span>
        <div className="actions">
          <button type="button" onClick={onChooseFile}>
            Choose {label}
          </button>
          <span className="mono muted">{value || 'no file selected'}</span>
        </div>
        {spec.required ? <p className="hint">Required</p> : null}
      </div>
    );
  }

  return (
    <div className="field">
      <label htmlFor={`mission-input-${inputKey}`}>{label}</label>
      <textarea
        id={`mission-input-${inputKey}`}
        rows={spec.kind === 'json' ? 5 : 3}
        value={value}
        placeholder={spec.kind === 'json' ? '{ }' : undefined}
        onChange={(event) => onChange(event.target.value)}
        spellCheck={false}
      />
      {spec.kind === 'json' ? <p className="hint">JSON object</p> : null}
      {spec.required ? <p className="hint">Required</p> : null}
    </div>
  );
}
