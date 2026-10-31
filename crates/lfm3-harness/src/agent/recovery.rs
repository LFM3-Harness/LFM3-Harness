//! Evidence-driven retry and immutable strategy replanning.
//!
//! Recovery sees only typed failures: a classified failure, the canonical
//! action that was attempted, and a normalized check signature. It never sees
//! or edits acceptance criteria. Replanning spends a strategy slot; it never
//! resets the per-step attempt budget, so no failure loop can outlive the
//! frozen limits.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::mission::{ExecutionStrategy, FrozenAcceptance, PlanError};

use super::packets::{EvidenceRef, RemainingBudget};

/// How a failure is classified before recovery decides anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// A deterministic or semantic check observed the criterion to be false.
    CheckFailed,
    /// Infrastructure hiccup: transport reset, model process exit, timeout.
    Transient,
    /// The model produced an action the executor could not accept.
    InvalidAction,
    /// The exact same action was proposed twice; never retried in place.
    RepeatedAction,
    /// A judge could not decide; requires review, never a pass.
    Inconclusive,
    /// Approval denied, model missing, or the goal cannot be verified.
    Unrecoverable,
    /// A person or deadline cancelled the run.
    Cancelled,
}

impl FailureClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CheckFailed => "check_failed",
            Self::Transient => "transient",
            Self::InvalidAction => "invalid_action",
            Self::RepeatedAction => "repeated_action",
            Self::Inconclusive => "inconclusive",
            Self::Unrecoverable => "unrecoverable",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Frozen recovery limits for one step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    pub attempts_per_strategy: u32,
    /// Total strategies available for the step, including the first.
    pub strategies: u32,
    /// Total attempts across every strategy for the step.
    pub total_attempts: u32,
}

impl Budget {
    #[must_use]
    pub const fn fixture(attempts_per_strategy: u32, strategies: u32, total_attempts: u32) -> Self {
        Self {
            attempts_per_strategy,
            strategies,
            total_attempts,
        }
    }

    /// The acceptance-backed limit: `max_attempts` per step, no replanning
    /// unless a strategy provider is installed.
    #[must_use]
    pub const fn from_max_attempts(max_attempts: u32) -> Self {
        Self {
            attempts_per_strategy: max_attempts,
            strategies: 1,
            total_attempts: max_attempts,
        }
    }
}

/// Why recovery stopped the step.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    BudgetExhausted,
    NoProgress,
    Unrecoverable,
}

impl StopReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BudgetExhausted => "budget_exhausted",
            Self::NoProgress => "no_progress",
            Self::Unrecoverable => "unrecoverable",
        }
    }
}

/// What to do after a classified failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDecision {
    Retry,
    Backoff,
    Replan,
    NeedsReview,
    Incomplete { reason: StopReason },
    Cancelled,
}

impl RecoveryDecision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Backoff => "backoff",
            Self::Replan => "replan",
            Self::NeedsReview => "needs_review",
            Self::Incomplete { .. } => "incomplete",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub const fn stop_reason(self) -> Option<StopReason> {
        match self {
            Self::Incomplete { reason } => Some(reason),
            _ => None,
        }
    }
}

/// One classified failure, already normalized for comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailureRecord {
    pub step_index: u32,
    pub class: FailureClass,
    /// Canonical action identity (`packets::canonical_action`).
    pub action: String,
    /// Normalized failure signature (`packets::failure_signature`).
    pub signature: String,
    pub evidence: Vec<EvidenceRef>,
    pub message: String,
}

impl FailureRecord {
    #[must_use]
    pub fn new(
        step_index: u32,
        class: FailureClass,
        action: impl Into<String>,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            step_index,
            class,
            action: action.into(),
            signature: signature.into(),
            evidence: Vec::new(),
            message: String::new(),
        }
    }

    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    #[must_use]
    pub fn fixture(number: u32) -> Self {
        Self::new(
            1,
            FailureClass::CheckFailed,
            format!("action-{number}"),
            format!("signature-{number}"),
        )
        .with_message(format!("fixture failure {number}"))
    }
}

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("recovery has no strategy budget left for this step")]
    NoStrategyBudget,
}

/// Per-step recovery state. Cheap to clone and independent across steps.
#[derive(Clone, Debug)]
pub struct RecoveryController {
    budget: Budget,
    step_index: u32,
    strategies_started: u32,
    attempts_in_strategy: u32,
    total_attempts: u32,
    history: Vec<FailureRecord>,
}

impl RecoveryController {
    #[must_use]
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            step_index: 0,
            strategies_started: 1,
            attempts_in_strategy: 0,
            total_attempts: 0,
            history: Vec::new(),
        }
    }

    #[must_use]
    pub const fn budget(&self) -> Budget {
        self.budget
    }

    /// The strategy number new attempts will be recorded under.
    #[must_use]
    pub const fn strategy(&self) -> u32 {
        self.strategies_started
    }

    #[must_use]
    pub fn history(&self) -> &[FailureRecord] {
        &self.history
    }

    /// One-based number of the attempt that would start next.
    #[must_use]
    pub const fn next_attempt_number(&self) -> u32 {
        self.total_attempts.saturating_add(1)
    }

    #[must_use]
    pub fn prior_signatures(&self) -> Vec<String> {
        self.history
            .iter()
            .map(|record| record.signature.clone())
            .collect()
    }

    #[must_use]
    pub fn remaining(&self) -> RemainingBudget {
        RemainingBudget::from_counts(
            self.budget,
            self.attempts_in_strategy,
            self.strategies_started,
            self.total_attempts,
        )
    }

    #[must_use]
    pub const fn can_replan(&self) -> bool {
        self.strategies_started < self.budget.strategies
    }

    /// Resets per-step counters when a new step begins.
    pub fn begin_step(&mut self, step_index: u32) {
        if self.step_index == step_index {
            return;
        }
        self.step_index = step_index;
        self.strategies_started = 1;
        self.attempts_in_strategy = 0;
        self.total_attempts = 0;
        self.history.clear();
    }

    /// Appends the failure that recovery just decided about.
    pub fn record(&mut self, failure: FailureRecord) {
        self.begin_step(failure.step_index);
        self.attempts_in_strategy = self.attempts_in_strategy.saturating_add(1);
        self.total_attempts = self.total_attempts.saturating_add(1);
        self.history.push(failure);
    }

    /// Spends one strategy slot. Returns false when no slot remains.
    pub fn record_replan(&mut self) -> bool {
        if !self.can_replan() {
            return false;
        }
        self.strategies_started += 1;
        self.attempts_in_strategy = 0;
        true
    }

    /// Accepts a planner-produced strategy only when it keeps the frozen
    /// acceptance hash and stays inside its declared steps and tools.
    pub fn accept_strategy(
        &mut self,
        acceptance: &FrozenAcceptance,
        strategy: &ExecutionStrategy,
    ) -> Result<u32, RecoveryError> {
        acceptance.verify_hash()?;
        acceptance.validate_strategy(strategy)?;
        if !self.record_replan() {
            return Err(RecoveryError::NoStrategyBudget);
        }
        Ok(self.strategies_started)
    }

    /// Bounded exponential backoff for transient infrastructure failures.
    #[must_use]
    pub fn backoff_delay(&self) -> Duration {
        let consecutive = self
            .history
            .iter()
            .rev()
            .take_while(|record| record.class == FailureClass::Transient)
            .count();
        let exponent = u32::try_from(consecutive.min(6)).unwrap_or(6);
        Duration::from_millis(50)
            .saturating_mul(1u32 << exponent)
            .min(Duration::from_secs(2))
    }

    /// Decides what happens after `failure`. The incoming failure counts as an
    /// attempt for both budgets; it is recorded separately via [`record`].
    pub fn next(&mut self, failure: FailureRecord) -> RecoveryDecision {
        self.begin_step(failure.step_index);
        let pending_total = self.total_attempts.saturating_add(1);
        let pending_strategy = self.attempts_in_strategy.saturating_add(1);
        let total_exhausted = pending_total >= self.budget.total_attempts;
        let strategy_exhausted = pending_strategy >= self.budget.attempts_per_strategy;

        match failure.class {
            FailureClass::Cancelled => RecoveryDecision::Cancelled,
            FailureClass::Unrecoverable => RecoveryDecision::Incomplete {
                reason: StopReason::Unrecoverable,
            },
            FailureClass::Inconclusive => RecoveryDecision::NeedsReview,
            FailureClass::Transient => {
                let consecutive = self
                    .history
                    .iter()
                    .rev()
                    .take_while(|record| record.class == FailureClass::Transient)
                    .count()
                    .saturating_add(1);
                if total_exhausted {
                    RecoveryDecision::Incomplete {
                        reason: StopReason::BudgetExhausted,
                    }
                } else if u32::try_from(consecutive).unwrap_or(u32::MAX)
                    > self.budget.attempts_per_strategy
                {
                    RecoveryDecision::Incomplete {
                        reason: StopReason::NoProgress,
                    }
                } else {
                    RecoveryDecision::Backoff
                }
            }
            FailureClass::RepeatedAction => {
                if total_exhausted {
                    RecoveryDecision::Incomplete {
                        reason: StopReason::BudgetExhausted,
                    }
                } else {
                    self.no_progress()
                }
            }
            FailureClass::InvalidAction | FailureClass::CheckFailed => {
                if total_exhausted {
                    return RecoveryDecision::Incomplete {
                        reason: StopReason::BudgetExhausted,
                    };
                }
                let repeated_action = !matches!(failure.class, FailureClass::InvalidAction)
                    && self
                        .history
                        .iter()
                        .any(|record| record.action == failure.action);
                let repeated_signature = self
                    .history
                    .iter()
                    .filter(|record| record.signature == failure.signature)
                    .count()
                    >= 2;
                if repeated_action || repeated_signature || self.alternating_cycle(&failure) {
                    return self.no_progress();
                }
                if strategy_exhausted {
                    return self.no_progress();
                }
                RecoveryDecision::Retry
            }
        }
    }

    fn no_progress(&self) -> RecoveryDecision {
        if self.can_replan() {
            RecoveryDecision::Replan
        } else {
            RecoveryDecision::Incomplete {
                reason: StopReason::NoProgress,
            }
        }
    }

    /// True when the incoming pair completes an `A, B, A, B` pattern.
    fn alternating_cycle(&self, failure: &FailureRecord) -> bool {
        let count = self.history.len();
        if count < 3 {
            return false;
        }
        let pair = |record: &FailureRecord| (record.action.clone(), record.signature.clone());
        let first = pair(&self.history[count - 3]);
        let second = pair(&self.history[count - 2]);
        let third = pair(&self.history[count - 1]);
        let incoming = pair(failure);
        first == third && second == incoming && first != second
    }
}
