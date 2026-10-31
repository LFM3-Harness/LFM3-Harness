import type { LlamaRuntimeStatus, ModelKey, ModelStatus } from '../bridge/types';
import { formatBytes } from './format';

export interface ModelDockProps {
  models: ModelStatus[];
  llama: LlamaRuntimeStatus;
  version?: string;
  dataDir?: string;
  releasing?: ModelKey | null;
  onRelease(modelKey: ModelKey): void;
}

/**
 * Local runtime facts. Models are shown even when absent or failed so a
 * missing weight is never silently hidden behind an available-looking run.
 */
export function ModelDock({
  models,
  llama,
  version,
  dataDir,
  releasing = null,
  onRelease,
}: ModelDockProps) {
  const pinned = llama.pinned && llama.executable !== null;

  return (
    <section className="panel" aria-labelledby="models-title">
      <h2 id="models-title">Local models</h2>
      <p className="muted">These power chat and tasks. Everything runs on this computer.</p>
      <dl className="facts">
        {version ? (
          <div>
            <dt>Engine</dt>
            <dd>{version}</dd>
          </div>
        ) : null}
        <div>
          <dt>llama.cpp</dt>
          <dd>
            build {llama.build} <span className="muted">{llama.release}</span>
          </dd>
        </div>
        <div>
          <dt>Runtime</dt>
          <dd>{pinned ? 'verified pin' : 'runtime not verified'}</dd>
        </div>
        <div className="span-2">
          <dt>Executable</dt>
          <dd className="mono">{llama.executable ?? 'not found'}</dd>
        </div>
        {dataDir ? (
          <div className="span-2">
            <dt>Trusted state</dt>
            <dd className="mono">{dataDir}</dd>
          </div>
        ) : null}
      </dl>

      {models.length === 0 ? (
        <p className="muted">No models reported by the engine.</p>
      ) : (
        <ul className="model-list">
          {models.map((model) => (
            <li key={model.key}>
              <span className={`dot dot-${model.state}`} aria-hidden="true" />
              <span className="mono">{model.key}</span>
              <span>{model.model}</span>
              <span className="muted">{model.state}</span>
              <span className="mono muted">{formatBytes(model.memory_bytes)}</span>
              <button
                type="button"
                className="small"
                disabled={model.state !== 'ready' || releasing === model.key}
                onClick={() => onRelease(model.key)}
              >
                {releasing === model.key ? 'Releasing…' : `Release ${model.model}`}
              </button>
              {model.error ? <span className="attempt-error">{model.error}</span> : null}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
