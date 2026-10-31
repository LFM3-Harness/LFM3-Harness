//! One active run per engine.
//!
//! The coordinator rejects a second start with a typed `RunBusy` outcome and
//! also consults durable run state, so a stale in-memory slot cannot start a
//! second mutating run over the same engine database.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::Mutex;

use crate::{store::Store, HarnessError, RunId};

use super::{RunOutcome, Runner};

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error("run {active} is already active on this engine")]
    RunBusy { active: RunId },
    #[error(transparent)]
    Store(#[from] HarnessError),
}

/// Serializes run execution for one engine instance.
#[derive(Clone)]
pub struct RunCoordinator {
    store: Store,
    active: Arc<Mutex<Option<RunId>>>,
}

impl RunCoordinator {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            active: Arc::new(Mutex::new(None)),
        }
    }

    /// Runs one runner, or returns [`CoordinatorError::RunBusy`] immediately.
    pub async fn start(
        &self,
        run_id: RunId,
        runner: Runner,
    ) -> Result<RunOutcome, CoordinatorError> {
        {
            let mut slot = self.active.lock().await;
            if let Some(active) = slot.clone() {
                return Err(CoordinatorError::RunBusy { active });
            }
            if let Some(active) = self.store.active_run().await? {
                if active != run_id {
                    return Err(CoordinatorError::RunBusy { active });
                }
            }
            *slot = Some(run_id.clone());
        }

        let outcome = runner.execute(&run_id).await;
        *self.active.lock().await = None;
        Ok(outcome?)
    }

    /// The in-process active run, if any.
    pub async fn active(&self) -> Option<RunId> {
        self.active.lock().await.clone()
    }
}
