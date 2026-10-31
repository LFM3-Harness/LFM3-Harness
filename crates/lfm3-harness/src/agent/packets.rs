//! Compact, typed failure packets.
//!
//! Recovery must hand the model exactly the evidence it needs to change
//! strategy: what failed, which observations back the failure, what was tried
//! before, and what budget remains. Packets never replay transcripts and never
//! reinterpret a check; they reference the evidence the verifier already
//! produced.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    mission::{FrozenAcceptance, PlannedStep},
    verify::{CheckResult, CheckStatus},
    AttemptId,
};

use super::recovery::{Budget, FailureClass, FailureRecord};

/// Excerpt limits keep packets small enough for a 1.7B model's context.
pub const MAX_EXCERPT_CHARS: usize = 400;
pub const MAX_EVIDENCE_REFS: usize = 8;

/// A pointer to what a check observed. The bytes themselves stay in the store.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub source: String,
    pub attempt: Option<AttemptId>,
    pub sha256: Option<String>,
    pub detail: String,
}

/// One failed check as the model sees it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailedCheck {
    pub kind: String,
    pub status: CheckStatus,
    pub message: String,
}

/// Budget remaining when the packet was built.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemainingBudget {
    pub attempts_left_in_strategy: u32,
    pub strategies_left: u32,
    pub total_attempts_left: u32,
}

impl RemainingBudget {
    #[must_use]
    pub fn from_counts(
        budget: Budget,
        attempts_in_strategy: u32,
        strategies_started: u32,
        total: u32,
    ) -> Self {
        Self {
            attempts_left_in_strategy: budget
                .attempts_per_strategy
                .saturating_sub(attempts_in_strategy),
            strategies_left: budget.strategies.saturating_sub(strategies_started),
            total_attempts_left: budget.total_attempts.saturating_sub(total),
        }
    }
}

/// Everything a retry or replan needs, and nothing more.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailurePacket {
    pub acceptance_hash: String,
    pub step_index: u32,
    pub criterion: String,
    pub strategy: u32,
    pub class: FailureClass,
    pub action: String,
    pub signature: String,
    pub message: String,
    pub failed_checks: Vec<FailedCheck>,
    pub evidence: Vec<EvidenceRef>,
    pub prior_signatures: Vec<String>,
    pub remaining: RemainingBudget,
}

impl FailurePacket {
    /// Builds a packet from the frozen step and the verified failure.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn build(
        acceptance: &FrozenAcceptance,
        step: &PlannedStep,
        strategy: u32,
        record: &FailureRecord,
        checks: &[CheckResult],
        prior_signatures: &[String],
        remaining: RemainingBudget,
    ) -> Self {
        Self {
            acceptance_hash: acceptance.hash().to_owned(),
            step_index: step.index,
            criterion: step.criterion.clone(),
            strategy,
            class: record.class,
            action: record.action.clone(),
            signature: record.signature.clone(),
            message: excerpt(&record.message, MAX_EXCERPT_CHARS),
            failed_checks: failed_checks(checks),
            evidence: evidence_refs(checks),
            prior_signatures: prior_signatures.to_vec(),
            remaining,
        }
    }
}

/// The action exactly as recorded, independent of whitespace.
#[must_use]
pub fn canonical_action(action: &Value) -> String {
    serde_json::to_string(action).unwrap_or_else(|_| action.to_string())
}

/// Canonical, whitespace-stable form used for no-progress detection.
#[must_use]
pub fn normalize_text(raw: &str) -> String {
    let mut normalized = String::with_capacity(raw.len());
    for (position, token) in raw.split_whitespace().enumerate() {
        if position > 0 {
            normalized.push(' ');
        }
        normalized.push_str(&normalize_token(token));
    }
    normalized
}

fn normalize_token(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    if lower.len() >= 16 && lower.chars().all(|character| character.is_ascii_hexdigit()) {
        return "<id>".to_owned();
    }
    let mut out = String::with_capacity(lower.len());
    let mut in_digits = false;
    for character in lower.chars() {
        if character.is_ascii_digit() {
            if !in_digits {
                out.push_str("<n>");
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(character);
        }
    }
    out
}

/// A failure is the same when the same class produced the same check messages.
#[must_use]
pub fn failure_signature(class: FailureClass, checks: &[CheckResult]) -> String {
    let mut signature = class.as_str().to_owned();
    for check in checks
        .iter()
        .filter(|check| check.status != CheckStatus::Pass)
    {
        signature.push('|');
        signature.push_str(&check.kind);
        signature.push(':');
        signature.push_str(check.status.as_str());
        signature.push(':');
        signature.push_str(&normalize_text(&check.message));
    }
    if signature == class.as_str() {
        signature.push_str("|no-evidence");
    }
    signature
}

#[must_use]
pub fn excerpt(raw: &str, limit: usize) -> String {
    if raw.chars().count() <= limit {
        return raw.to_owned();
    }
    let mut clipped: String = raw.chars().take(limit).collect();
    clipped.push_str("...");
    clipped
}

fn failed_checks(checks: &[CheckResult]) -> Vec<FailedCheck> {
    checks
        .iter()
        .filter(|check| check.status != CheckStatus::Pass)
        .map(|check| FailedCheck {
            kind: check.kind.clone(),
            status: check.status,
            message: excerpt(&check.message, MAX_EXCERPT_CHARS),
        })
        .collect()
}

/// Typed evidence pointers from failed checks, deduplicated and bounded.
#[must_use]
pub fn evidence_refs(checks: &[CheckResult]) -> Vec<EvidenceRef> {
    let mut refs: Vec<EvidenceRef> = Vec::new();
    for check in checks
        .iter()
        .filter(|check| check.status != CheckStatus::Pass)
    {
        for item in &check.evidence {
            let sha256 = item.digest.as_ref().map(|digest| digest.sha256.clone());
            if refs
                .iter()
                .any(|seen| seen.source == item.source && seen.sha256 == sha256)
            {
                continue;
            }
            refs.push(EvidenceRef {
                source: item.source.clone(),
                attempt: item.attempt.clone(),
                sha256,
                detail: excerpt(&item.detail, MAX_EXCERPT_CHARS),
            });
            if refs.len() >= MAX_EVIDENCE_REFS {
                return refs;
            }
        }
    }
    refs
}
