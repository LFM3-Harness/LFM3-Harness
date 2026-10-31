import type { ModelCatalog, ModelKey } from '../bridge/types';

import { formatBytes } from '../components/format';

export interface DownloadProgress {
  key: ModelKey;
  file: string;
  received: number;
  fileTotal: number;
  overallReceived: number;
  overallTotal: number;
}

/**
 * The Models menu in the chat header. It shows exactly what is installed,
 * what is loaded into memory, and what is missing, and it only ever fetches a
 * model after the person clicks Download for that role.
 */
export function ChatModelMenu({
  catalog,
  downloading,
  progress,
  error,
  refreshing,
  busy,
  busyReason,
  onRefresh,
  onDownload,
  onCancel,
  onReload,
  onClose,
}: {
  catalog: ModelCatalog | null;
  downloading: ModelKey | null;
  progress: DownloadProgress | null;
  error: string | null;
  refreshing: boolean;
  busy: boolean;
  busyReason: string | null;
  onRefresh: () => void;
  onDownload: (key: ModelKey) => void;
  onCancel: (key: ModelKey) => void;
  onReload: () => void;
  onClose: () => void;
}) {
  const pendingReload = catalog?.pending_reload ?? false;
  const installedNotLoaded =
    catalog?.models.some((model) => model.installed && !model.loaded) ?? false;

  return (
    <section
      className="model-menu"
      role="dialog"
      aria-label="Local models"
      onKeyDown={(event) => {
        if (event.key === 'Escape') {
          event.stopPropagation();
          onClose();
        }
      }}
    >
      <header className="model-menu-head">
        <div>
          <p className="eyebrow">Local models</p>
          <h2>Download and manage</h2>
        </div>
        <button type="button" className="model-menu-close" aria-label="Close models" onClick={onClose}>
          ×
        </button>
      </header>

      {error ? (
        <p className="model-menu-error" role="alert">
          {error}
        </p>
      ) : null}

      {catalog === null ? (
        <p className="model-menu-note" role="status">
          Reading the model catalog…
        </p>
      ) : (
        <>
          <p className="model-menu-path" title={catalog.models_dir}>
            <span aria-hidden="true">▸</span> {catalog.models_dir}
          </p>

          {!catalog.runtime_ready ? (
            <p className="model-menu-warning" role="note">
              The local model runner isn’t ready yet
              {catalog.runtime_note ? `: ${catalog.runtime_note}` : '.'} Downloads can still finish;
              chats start once it’s available.
            </p>
          ) : null}

          {pendingReload ? (
            <p className="model-menu-warning" role="note">
              A finished download is waiting for this conversation to go idle before it is loaded.{' '}
              <button type="button" className="small" onClick={onReload} disabled={busy}>
                Load now
              </button>
            </p>
          ) : null}

          <ul className="model-rows">
            {catalog.models.map((model) => {
              const active = downloading === model.key;
              const percent =
                active && progress && progress.overallTotal > 0
                  ? Math.min(100, Math.round((progress.overallReceived / progress.overallTotal) * 100))
                  : null;
              return (
                <li key={model.key} className={`model-row model-${model.state}`}>
                  <div className="model-row-head">
                    <div>
                      <strong>{model.title}</strong>
                      <span className="model-row-key">{model.key}</span>
                    </div>
                    <span className={`status-pill pill-${model.state}`}>
                      {model.loaded ? 'Loaded' : model.installed ? 'Installed' : 'Missing'}
                    </span>
                  </div>

                  <p className="model-row-meta">
                    {formatBytes(model.total_bytes)} · {model.repo}
                  </p>

                  {active && progress ? (
                    <div className="model-progress">
                      <div
                        className="model-progress-track"
                        role="progressbar"
                        aria-valuemin={0}
                        aria-valuemax={100}
                        aria-valuenow={percent ?? 0}
                        aria-label={`${model.title} download`}
                      >
                        <span style={{ width: `${percent ?? 0}%` }} />
                      </div>
                      <p className="model-progress-text">
                        {percent !== null ? `${percent}% · ` : ''}
                        {formatBytes(progress.overallReceived)} of {formatBytes(progress.overallTotal)}
                        {progress.file ? ` · ${progress.file}` : ' · contacting the model source…'}
                      </p>
                    </div>
                  ) : null}

                  <div className="model-row-actions">
                    {active ? (
                      <button type="button" className="small" onClick={() => onCancel(model.key)}>
                        Cancel
                      </button>
                    ) : model.installed ? (
                      <span className="model-row-installed">
                        {model.loaded ? 'In use' : 'Ready to load'}
                      </span>
                    ) : (
                      <button
                        type="button"
                        className="small model-download"
                        disabled={busy || downloading !== null}
                        title={
                          busy
                            ? busyReason ?? 'Wait for the current work to finish.'
                            : downloading !== null
                              ? 'One download runs at a time.'
                              : `Download ${formatBytes(model.total_bytes)} from ${model.source}`
                        }
                        onClick={() => onDownload(model.key)}
                      >
                        Download {formatBytes(model.total_bytes)}
                      </button>
                    )}
                  </div>
                </li>
              );
            })}
          </ul>

          {installedNotLoaded && !pendingReload ? (
            <div className="model-menu-reload">
              <button type="button" className="small" onClick={onReload} disabled={busy}>
                Reload installed models
              </button>
            </div>
          ) : null}

          <p className="model-menu-license">
            Downloads are explicit: only the {catalog.models.length} pinned roles above are fetched,
            from LiquidAI on Hugging Face over HTTPS, and every file is verified against its
            published size and SHA-256 before use. {catalog.models[0]?.license ?? ''}
          </p>
        </>
      )}

      <footer className="model-menu-foot">
        <button type="button" className="small" onClick={onRefresh} disabled={refreshing}>
          {refreshing ? 'Refreshing…' : 'Refresh'}
        </button>
        <span className="model-menu-hint">Nothing is sent off this machine.</span>
      </footer>
    </section>
  );
}
