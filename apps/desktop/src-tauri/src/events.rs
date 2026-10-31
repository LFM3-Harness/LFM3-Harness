//! Post-commit journal forwarding.
//!
//! The store broadcasts only after a transaction commits, so the UI can never
//! observe an event whose state change was rolled back. When the broadcast
//! lags, the stream simply stops delivering; the UI's sequence gap detector
//! then refetches the authoritative snapshot and journal.

use lfm3_harness::Store;
use tauri::{AppHandle, Emitter};

/// Must match `RUN_EVENT_CHANNEL` in `apps/ui/src/bridge/EngineBridge.ts`.
pub const RUN_EVENT_CHANNEL: &str = "lfm3:run-event";

pub fn spawn(app: AppHandle, store: Store) {
    tauri::async_runtime::spawn(async move {
        let mut events = store.subscribe();
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Err(error) = app.emit(RUN_EVENT_CHANNEL, &event) {
                        tracing::warn!(%error, "could not deliver run event");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "event forwarder lagged; UI will resynchronize");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
