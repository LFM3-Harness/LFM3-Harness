//! Cancellation and deadline propagation.
//!
//! A run owns one context. Every child call receives a clone that shares the
//! cancel token and the absolute deadline, so active awaits abort promptly
//! instead of only discovering cancellation between calls.

use std::{
    fmt,
    future::Future,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

/// Why a scoped future stopped early.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelReason {
    /// A person or the engine requested cancellation.
    Requested,
    /// The run or call deadline passed.
    Deadline,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requested => formatter.write_str("cancelled by request"),
            Self::Deadline => formatter.write_str("deadline exceeded"),
        }
    }
}

/// Failure of work that was aborted by cancellation or a deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancelled {
    pub reason: CancelReason,
}

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            CancelReason::Requested => formatter.write_str("cancelled by request"),
            CancelReason::Deadline => formatter.write_str("deadline exceeded"),
        }
    }
}

impl std::error::Error for Cancelled {}

/// The root cancellation handle for one run or one bounded operation.
#[derive(Clone, Debug)]
pub struct CancellationContext {
    token: CancellationToken,
    deadline: Instant,
}

impl CancellationContext {
    /// Creates a context whose deadline is `budget` from now.
    #[must_use]
    pub fn new(budget: Duration) -> Self {
        Self::with_deadline(Instant::now() + budget)
    }

    #[must_use]
    pub fn with_deadline(deadline: Instant) -> Self {
        Self {
            token: CancellationToken::new(),
            deadline,
        }
    }

    /// Shares the same cancel token and absolute deadline.
    #[must_use]
    pub fn child(&self) -> CancellationChild {
        CancellationChild {
            token: self.token.clone(),
            deadline: self.deadline,
        }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Resolves the future or aborts it when cancellation or the deadline fires.
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, Cancelled> {
        self.child().run(future).await
    }

    /// Completes when the context is cancelled or its deadline passes.
    pub async fn cancelled_or_expired(&self) -> CancelReason {
        self.child().cancelled_or_expired().await
    }
}

/// A clone of a run's cancellation state handed to one call.
#[derive(Clone, Debug)]
pub struct CancellationChild {
    token: CancellationToken,
    deadline: Instant,
}

impl CancellationChild {
    pub fn cancel(&self) {
        self.token.cancel();
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Awaits cancellation only. Does not resolve on the deadline.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Resolves with the first cause: explicit cancellation or deadline.
    pub async fn cancelled_or_expired(&self) -> CancelReason {
        if self.token.is_cancelled() {
            return CancelReason::Requested;
        }
        let deadline = tokio::time::Instant::from_std(self.deadline);
        tokio::select! {
            biased;
            () = self.token.cancelled() => CancelReason::Requested,
            () = tokio::time::sleep_until(deadline) => CancelReason::Deadline,
        }
    }

    /// Resolves the future or aborts it when cancellation or the deadline fires.
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, Cancelled> {
        tokio::pin!(future);
        tokio::select! {
            biased;
            reason = self.cancelled_or_expired() => Err(Cancelled { reason }),
            value = &mut future => Ok(value),
        }
    }
}
