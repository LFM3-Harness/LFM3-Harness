//! Semantic judgements and their provenance.
//!
//! A semantic check never stands alone. It hashes the source it read, records
//! the exact prompt and raw response of every model it consulted, records the
//! model identity and confidence, and names the output JSON pointers it judged.
//! A judgement made by d1 over a vision description is one evidence chain, not
//! independent pixel evidence.
//!
//! Automatic passing is a mission capability. Until a mission carries a
//! validated holdout qualification, every semantic criterion is inconclusive
//! and asks for human review.

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    mission::CheckSpec,
    model::{ChatMessage, ChatRequest, ContentPart, ImageUrl, ModelGateway, ResponseFormat},
    paths::{self, Mode},
    CancellationContext,
};

use super::{
    deterministic::{CheckContext, CheckEvidence, CheckResult, CheckStatus},
    media_type_for, ContentDigest,
};

/// Future returned by a semantic judge.
pub type JudgeFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const JUDGE_TIMEOUT: Duration = Duration::from_secs(120);

/// How a mission may use semantic criteria.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticMode {
    /// Semantic checks are informational; no run may pass them automatically.
    HumanReviewOnly,
    /// Semantic checks may pass a step only under a validated qualification.
    Automatic,
}

/// Evidence that a mission's semantic criteria survived a frozen holdout.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    /// SHA-256 of the frozen holdout manifest that was evaluated.
    pub holdout_sha256: String,
    /// Packaged real-model runs observed with no false completion.
    pub runs: u32,
    /// Confident false passes observed on the holdout. Must be zero.
    pub false_completions: u32,
    /// Date or version label of the evaluation.
    pub evaluated_at: String,
}

/// Mission-level semantic capability, frozen with the manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticPolicy {
    pub mode: SemanticMode,
    #[serde(default = "default_confidence_bps")]
    pub min_confidence_bps: u16,
    #[serde(default)]
    pub qualification: Option<Qualification>,
    /// Explicit, user-visible reason when automatic passing is unavailable.
    #[serde(default)]
    pub reason: Option<String>,
}

impl Default for SemanticPolicy {
    fn default() -> Self {
        Self {
            mode: SemanticMode::HumanReviewOnly,
            min_confidence_bps: default_confidence_bps(),
            qualification: None,
            reason: Some("semantic judging is not configured for this mission".to_owned()),
        }
    }
}

impl SemanticPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.min_confidence_bps == 0 || self.min_confidence_bps > 10_000 {
            return Err("min_confidence_bps must be between 1 and 10000".to_owned());
        }
        match self.mode {
            SemanticMode::HumanReviewOnly => {
                if self
                    .reason
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(str::is_empty)
                {
                    return Err(
                        "human_review_only needs a reason that is shown to the user".to_owned()
                    );
                }
            }
            SemanticMode::Automatic => {
                let Some(qualification) = &self.qualification else {
                    return Err(
                        "automatic semantic passing needs a holdout qualification".to_owned()
                    );
                };
                if qualification.runs < 3 {
                    return Err("a qualification needs at least three packaged runs".to_owned());
                }
                if qualification.false_completions != 0 {
                    return Err(
                        "a confident false completion disqualifies automatic semantic passing"
                            .to_owned(),
                    );
                }
                if qualification.holdout_sha256.len() != 64
                    || !qualification
                        .holdout_sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(
                        "qualification.holdout_sha256 must be a SHA-256 hex digest".to_owned()
                    );
                }
                if qualification.evaluated_at.trim().is_empty() {
                    return Err("qualification.evaluated_at must not be empty".to_owned());
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn automatic(&self) -> bool {
        self.mode == SemanticMode::Automatic
    }

    #[must_use]
    pub fn threshold(&self) -> f64 {
        f64::from(self.min_confidence_bps) / 10_000.0
    }
}

const fn default_confidence_bps() -> u16 {
    8_000
}

/// Typed record of one semantic judgement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticEvidence {
    /// `d1` or `vl->d1`; a chain is one line of evidence, not two.
    pub chain: String,
    pub model: String,
    pub prompt: String,
    pub response: String,
    pub label: String,
    pub confidence_bps: u16,
    pub threshold_bps: u16,
    /// Output JSON pointers that were judged, in stable order.
    pub pointers: Vec<String>,
    /// Pointers the judge found unsupported.
    pub unsupported: Vec<String>,
}

/// One judgement request. Empty `labels` means a free-form observation.
#[derive(Clone, Debug, PartialEq)]
pub struct JudgeRequest {
    pub instruction: String,
    pub evidence: String,
    pub labels: BTreeMap<String, String>,
    /// Data URL for a vision request.
    pub image: Option<String>,
    pub max_tokens: u32,
}

/// What the judge answered, with the exact prompt and raw response preserved.
#[derive(Clone, Debug, PartialEq)]
pub struct JudgeVerdict {
    pub label: String,
    pub confidence: f64,
    pub model: String,
    pub prompt: String,
    pub response: String,
    pub rationale: String,
}

#[derive(Debug, Error)]
pub enum JudgeError {
    #[error("semantic judge unavailable: {0}")]
    Unavailable(String),
    #[error("semantic judge returned an invalid answer: {0}")]
    Protocol(String),
}

/// A bounded semantic judge. It has no tool authority and no access to the
/// workspace; the verifier passes it only the evidence it already read.
pub trait SemanticJudge: Send + Sync {
    fn judge<'a>(
        &'a self,
        request: JudgeRequest,
    ) -> JudgeFuture<'a, Result<JudgeVerdict, JudgeError>>;
}

/// A judge backed by the local d1 and vision chat endpoints.
pub struct ModelJudge {
    d1: Arc<dyn ModelGateway>,
    vision: Option<Arc<dyn ModelGateway>>,
}

impl ModelJudge {
    #[must_use]
    pub fn new(d1: Arc<dyn ModelGateway>, vision: Option<Arc<dyn ModelGateway>>) -> Self {
        Self { d1, vision }
    }
}

impl SemanticJudge for ModelJudge {
    fn judge<'a>(
        &'a self,
        request: JudgeRequest,
    ) -> JudgeFuture<'a, Result<JudgeVerdict, JudgeError>> {
        Box::pin(async move {
            let freeform = request.labels.is_empty();
            let gateway = if request.image.is_some() {
                self.vision.as_ref().ok_or_else(|| {
                    JudgeError::Unavailable("no local vision endpoint is configured".to_owned())
                })?
            } else {
                &self.d1
            };
            let prompt = compose_prompt(&request);
            let user = match &request.image {
                Some(url) => ChatMessage::user_parts(vec![
                    ContentPart::Text {
                        text: request.evidence.clone(),
                    },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl { url: url.clone() },
                    },
                ]),
                None => ChatMessage::user(request.evidence.clone()),
            };
            let mut chat = ChatRequest::new(vec![ChatMessage::system(prompt.clone()), user])
                .with_temperature(0.0)
                .with_max_tokens(request.max_tokens);
            if !freeform {
                chat = chat.with_response_format(ResponseFormat::json_object());
            }
            let cancel = CancellationContext::new(JUDGE_TIMEOUT);
            let response = cancel
                .run(gateway.chat(chat, cancel.child()))
                .await
                .map_err(|_| {
                    JudgeError::Unavailable(
                        "judge call exceeded its deadline or was cancelled".to_owned(),
                    )
                })?
                .map_err(|error| JudgeError::Unavailable(error.to_string()))?;
            let raw = response.content().unwrap_or("").trim().to_owned();
            if raw.is_empty() {
                return Err(JudgeError::Protocol("judge returned no content".to_owned()));
            }
            let model = if response.model.trim().is_empty() {
                "unknown".to_owned()
            } else {
                response.model.clone()
            };
            if freeform {
                return Ok(JudgeVerdict {
                    label: String::new(),
                    confidence: 1.0,
                    model,
                    prompt,
                    response: raw,
                    rationale: String::new(),
                });
            }
            let wire: JudgeWire = serde_json::from_str(&raw)
                .map_err(|error| JudgeError::Protocol(format!("judge JSON: {error}")))?;
            if !request.labels.contains_key(&wire.label) {
                return Err(JudgeError::Protocol(format!(
                    "judge label {:?} is outside the declared set",
                    wire.label
                )));
            }
            if !(0.0..=1.0).contains(&wire.confidence) || !wire.confidence.is_finite() {
                return Err(JudgeError::Protocol(format!(
                    "judge confidence {} is outside [0,1]",
                    wire.confidence
                )));
            }
            Ok(JudgeVerdict {
                label: wire.label,
                confidence: wire.confidence,
                model,
                prompt,
                response: raw,
                rationale: wire.rationale.unwrap_or_default(),
            })
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgeWire {
    label: String,
    confidence: f64,
    #[serde(default)]
    rationale: Option<String>,
}

fn compose_prompt(request: &JudgeRequest) -> String {
    if request.labels.is_empty() {
        return format!(
            "{}\nDescribe only what is visibly present or explicitly stated. \
             Do not guess, do not add facts, and say so when the evidence is absent. \
             Reply in plain text.",
            request.instruction
        );
    }
    let labels = request
        .labels
        .iter()
        .map(|(label, description)| format!("- {label}: {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\nChoose exactly one label:\n{labels}\n\
         Reply with a JSON object having string `label`, number `confidence` in [0,1], \
         and string `rationale`. Do not add other keys.",
        request.instruction
    )
}

/// True for every check kind that needs a semantic judge.
#[must_use]
pub fn is_semantic_kind(kind: &str) -> bool {
    matches!(
        kind,
        "vl_criterion" | "d1_noul" | "d1_choice" | "d1_grounded"
    )
}

/// Every semantic check kind, for manifest validation.
pub const SEMANTIC_KINDS: &[&str] = &["vl_criterion", "d1_noul", "d1_choice", "d1_grounded"];

/// Evaluates one semantic check. Deterministic failures are not consulted here
/// because the runner requires every check in the step to pass; a semantic pass
/// can never compensate for a deterministic failure.
pub async fn evaluate(
    spec: CheckSpec,
    context: CheckContext,
    judge: Arc<dyn SemanticJudge>,
    policy: SemanticPolicy,
) -> CheckResult {
    if !policy.automatic() {
        let reason = policy
            .reason
            .as_deref()
            .unwrap_or("this mission is not holdout-qualified");
        return outcome(
            &spec,
            &context,
            CheckStatus::Error,
            format!("human review required: automatic semantic passing is disabled ({reason})"),
            vec![policy_evidence(&context, reason)],
        );
    }
    match spec.kind.as_str() {
        "d1_noul" => judge_text_criterion(&spec, &context, &judge, &policy, false).await,
        "d1_choice" => judge_choice(&spec, &context, &judge, &policy).await,
        "d1_grounded" => judge_grounded(&spec, &context, &judge, &policy).await,
        "vl_criterion" => judge_visual(&spec, &context, &judge, &policy).await,
        other => outcome(
            &spec,
            &context,
            CheckStatus::Error,
            format!("{other} is not a semantic check"),
            Vec::new(),
        ),
    }
}

async fn judge_text_criterion(
    spec: &CheckSpec,
    context: &CheckContext,
    judge: &Arc<dyn SemanticJudge>,
    policy: &SemanticPolicy,
    _choice: bool,
) -> CheckResult {
    let Some(criterion) = string_arg(spec, "criterion") else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "the semantic check needs a criterion",
            Vec::new(),
        );
    };
    let Some(path) = string_arg(spec, "path") else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "the semantic check needs the output path it judges",
            Vec::new(),
        );
    };
    let (text, digest) = match read_workspace_text(context, path) {
        Ok(value) => value,
        Err(message) => {
            return outcome(spec, context, CheckStatus::Error, message, Vec::new());
        }
    };
    let request = JudgeRequest {
        instruction: format!(
            "Decide whether the evidence supports this criterion: {criterion}. \
             Use `supported` only when the evidence states it directly, `unsupported` \
             when the evidence contradicts or lacks it, and `ambiguous` when it cannot be decided."
        ),
        evidence: format!("EVIDENCE ({path}):\n{text}"),
        labels: decision_labels(),
        image: None,
        max_tokens: 256,
    };
    let verdict = match judge.judge(request).await {
        Ok(verdict) => verdict,
        Err(error) => {
            return outcome(
                spec,
                context,
                CheckStatus::Error,
                error.to_string(),
                vec![digest_evidence(context, path, digest, "judged evidence")],
            );
        }
    };
    let semantic = semantic_evidence("d1", &verdict, policy, Vec::new(), Vec::new());
    let evidence = vec![judged_evidence(context, path, digest, &verdict, semantic)];
    match verdict.label.as_str() {
        "supported" if verdict_clears(&verdict, policy) => outcome(
            spec,
            context,
            CheckStatus::Pass,
            format!("semantic evidence supports {criterion:?}"),
            evidence,
        ),
        "supported" => outcome(
            spec,
            context,
            CheckStatus::Error,
            format!(
                "the judge supported the criterion at {:.0}% confidence, below the {:.0}% threshold",
                verdict.confidence * 100.0,
                policy.threshold() * 100.0
            ),
            evidence,
        ),
        "unsupported" => outcome(
            spec,
            context,
            CheckStatus::Fail,
            format!("the evidence does not support {criterion:?}"),
            evidence,
        ),
        _ => outcome(
            spec,
            context,
            CheckStatus::Error,
            "the semantic judge found the evidence ambiguous".to_owned(),
            evidence,
        ),
    }
}

async fn judge_choice(
    spec: &CheckSpec,
    context: &CheckContext,
    judge: &Arc<dyn SemanticJudge>,
    policy: &SemanticPolicy,
) -> CheckResult {
    let Some(criterion) = string_arg(spec, "criterion") else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "d1_choice needs a criterion",
            Vec::new(),
        );
    };
    let Some(expect) = string_arg(spec, "expect") else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "d1_choice needs an expected label",
            Vec::new(),
        );
    };
    let Some(options) = spec.args.get("options").and_then(Value::as_object) else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "d1_choice needs an options object",
            Vec::new(),
        );
    };
    let mut labels = BTreeMap::new();
    for (label, description) in options {
        labels.insert(
            label.clone(),
            description
                .as_str()
                .map_or_else(|| description.to_string(), str::to_owned),
        );
    }
    let (text, digest, source) = match string_arg(spec, "path") {
        Some(path) => match read_workspace_text(context, path) {
            Ok((text, digest)) => (text, digest, path.to_owned()),
            Err(message) => {
                return outcome(spec, context, CheckStatus::Error, message, Vec::new());
            }
        },
        None => match read_report_text(context) {
            Some(value) => value,
            None => {
                return outcome(
                    spec,
                    context,
                    CheckStatus::Error,
                    "d1_choice needs a path or a report to judge".to_owned(),
                    Vec::new(),
                );
            }
        },
    };
    let request = JudgeRequest {
        instruction: format!("Choose the label that best matches the evidence for: {criterion}."),
        evidence: format!("EVIDENCE:\n{text}"),
        labels,
        image: None,
        max_tokens: 256,
    };
    let verdict = match judge.judge(request).await {
        Ok(verdict) => verdict,
        Err(error) => {
            return outcome(
                spec,
                context,
                CheckStatus::Error,
                error.to_string(),
                vec![digest_evidence(context, &source, digest, "judged evidence")],
            );
        }
    };
    let semantic = semantic_evidence("d1", &verdict, policy, Vec::new(), Vec::new());
    let evidence = vec![judged_evidence(
        context, &source, digest, &verdict, semantic,
    )];
    if verdict.label != expect {
        return outcome(
            spec,
            context,
            CheckStatus::Fail,
            format!(
                "the judge chose {:?} instead of the expected {:?}",
                verdict.label, expect
            ),
            evidence,
        );
    }
    if !verdict_clears(&verdict, policy) {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "the choice matched but its confidence is below the threshold".to_owned(),
            evidence,
        );
    }
    outcome(
        spec,
        context,
        CheckStatus::Pass,
        format!("the judge chose the expected label {expect:?}"),
        evidence,
    )
}

async fn judge_grounded(
    spec: &CheckSpec,
    context: &CheckContext,
    judge: &Arc<dyn SemanticJudge>,
    policy: &SemanticPolicy,
) -> CheckResult {
    let Some(path) = string_arg(spec, "path") else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "d1_grounded needs the output JSON path",
            Vec::new(),
        );
    };
    let Some(source_name) = string_arg(spec, "source") else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "d1_grounded needs the acceptance input name that holds the source",
            Vec::new(),
        );
    };
    let Some(fields) = spec.args.get("fields").and_then(Value::as_object) else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "d1_grounded needs a fields object",
            Vec::new(),
        );
    };
    if fields.is_empty() {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "d1_grounded needs at least one output pointer",
            Vec::new(),
        );
    }

    let (output_text, output_digest) = match read_workspace_text(context, path) {
        Ok(value) => value,
        Err(message) => {
            return outcome(spec, context, CheckStatus::Error, message, Vec::new());
        }
    };
    let output: Value = match serde_json::from_str(&output_text) {
        Ok(value) => value,
        Err(error) => {
            return outcome(
                spec,
                context,
                CheckStatus::Error,
                format!("{path} is not JSON: {error}"),
                vec![digest_evidence(
                    context,
                    path,
                    output_digest,
                    "unparseable output",
                )],
            );
        }
    };
    let (source_digest, source_text) = match read_input_text(context, source_name) {
        Ok(value) => value,
        Err(message) => {
            return outcome(
                spec,
                context,
                CheckStatus::Error,
                message,
                vec![digest_evidence(
                    context,
                    path,
                    output_digest,
                    "output without a recoverable source",
                )],
            );
        }
    };

    let mut evidence = vec![
        digest_evidence(context, path, output_digest.clone(), "judged output"),
        digest_evidence(
            context,
            &format!("input:{source_name}"),
            source_digest,
            "immutable semantic source",
        ),
    ];
    let mut pointers = Vec::new();
    let mut unsupported = Vec::new();
    let mut ambiguous = Vec::new();
    let normalized_source = normalize(&source_text);

    for (pointer, description) in fields {
        let Some(value) = resolve_pointer(&output, pointer) else {
            return outcome(
                spec,
                context,
                CheckStatus::Error,
                format!("output pointer {pointer:?} does not resolve"),
                evidence,
            );
        };
        let text = scalar_text(value);
        if text.is_empty() {
            return outcome(
                spec,
                context,
                CheckStatus::Error,
                format!("output pointer {pointer:?} is empty"),
                evidence,
            );
        }
        // A necessary, independent condition: the exact value must literally
        // occur in the immutable source before any model is consulted. This
        // blocks a confident judge from passing a fabricated field.
        if !normalized_source.contains(&normalize(&text)) {
            unsupported.push(pointer.clone());
            evidence.push(CheckEvidence {
                source: format!("workspace:{path}"),
                attempt: Some(context.attempt.clone()),
                digest: Some(output_digest.clone()),
                detail: format!(
                    "pointer {pointer:?} value {text:?} does not occur in input {source_name:?}"
                ),
                semantic: None,
            });
            continue;
        }
        pointers.push(pointer.clone());
        let description = description
            .as_str()
            .map_or_else(|| description.to_string(), str::to_owned);
        let request = JudgeRequest {
            instruction: format!(
                "Decide whether the output field {pointer} ({description}) is grounded in \
                 the source. Use `grounded` only when the source states it, `unsupported` \
                 when it is absent or contradicted, and `ambiguous` when it cannot be decided."
            ),
            evidence: format!("SOURCE ({source_name}):\n{source_text}\n\nOUTPUT {pointer}: {text}"),
            labels: grounding_labels(),
            image: None,
            max_tokens: 256,
        };
        let verdict = match judge.judge(request).await {
            Ok(verdict) => verdict,
            Err(error) => {
                return outcome(
                    spec,
                    context,
                    CheckStatus::Error,
                    error.to_string(),
                    evidence,
                );
            }
        };
        let semantic = semantic_evidence(
            "d1",
            &verdict,
            policy,
            vec![pointer.clone()],
            if verdict.label == "grounded" {
                Vec::new()
            } else {
                vec![pointer.clone()]
            },
        );
        evidence.push(judged_evidence(
            context,
            path,
            output_digest.clone(),
            &verdict,
            semantic,
        ));
        match verdict.label.as_str() {
            "grounded" if verdict_clears(&verdict, policy) => {}
            "grounded" => ambiguous.push(pointer.clone()),
            "unsupported" => unsupported.push(pointer.clone()),
            _ => ambiguous.push(pointer.clone()),
        }
    }

    if !unsupported.is_empty() {
        return outcome(
            spec,
            context,
            CheckStatus::Fail,
            format!("unsupported output pointers: {}", unsupported.join(", ")),
            evidence,
        );
    }
    if !ambiguous.is_empty() {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            format!(
                "the judge could not decide these pointers confidently: {}",
                ambiguous.join(", ")
            ),
            evidence,
        );
    }
    outcome(
        spec,
        context,
        CheckStatus::Pass,
        format!(
            "every field is grounded in input {source_name:?}: {}",
            pointers.join(", ")
        ),
        evidence,
    )
}

async fn judge_visual(
    spec: &CheckSpec,
    context: &CheckContext,
    judge: &Arc<dyn SemanticJudge>,
    policy: &SemanticPolicy,
) -> CheckResult {
    let Some(criterion) = string_arg(spec, "criterion") else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "vl_criterion needs a criterion",
            Vec::new(),
        );
    };
    let Some(image_name) = string_arg(spec, "image") else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "vl_criterion needs the acceptance image input name",
            Vec::new(),
        );
    };
    let (image_digest, bytes) = match read_input_bytes(context, image_name) {
        Ok(value) => value,
        Err(message) => {
            return outcome(spec, context, CheckStatus::Error, message, Vec::new());
        }
    };
    let Some(media_type) = image_media_type(&bytes) else {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            format!("input {image_name:?} is not a supported image"),
            vec![digest_evidence(
                context,
                &format!("input:{image_name}"),
                image_digest,
                "unrecognized image bytes",
            )],
        );
    };
    let data_url = format!("data:{media_type};base64,{}", encode_base64(&bytes));
    let observation = match judge
        .judge(JudgeRequest {
            instruction: format!(
                "Describe only the visible evidence relevant to this criterion: {criterion}. \
                 If the image does not show it, say so explicitly."
            ),
            evidence: format!("Criterion: {criterion}"),
            labels: BTreeMap::new(),
            image: Some(data_url),
            max_tokens: 512,
        })
        .await
    {
        Ok(verdict) => verdict,
        Err(error) => {
            return outcome(
                spec,
                context,
                CheckStatus::Error,
                error.to_string(),
                vec![digest_evidence(
                    context,
                    &format!("input:{image_name}"),
                    image_digest,
                    "vision observation unavailable",
                )],
            );
        }
    };
    if observation.response.trim().is_empty() {
        return outcome(
            spec,
            context,
            CheckStatus::Error,
            "the vision model returned no observation".to_owned(),
            vec![digest_evidence(
                context,
                &format!("input:{image_name}"),
                image_digest,
                "empty vision observation",
            )],
        );
    }
    let vision_evidence = CheckEvidence {
        source: format!("input:{image_name}"),
        attempt: Some(context.attempt.clone()),
        digest: Some(image_digest.clone()),
        detail: "vision observation".to_owned(),
        semantic: Some(SemanticEvidence {
            chain: "vl->d1".to_owned(),
            model: observation.model.clone(),
            prompt: observation.prompt.clone(),
            response: observation.response.clone(),
            label: "observed".to_owned(),
            confidence_bps: 10_000,
            threshold_bps: policy.min_confidence_bps,
            pointers: Vec::new(),
            unsupported: Vec::new(),
        }),
    };
    let verdict = match judge
        .judge(JudgeRequest {
            instruction: format!(
                "Decide whether the visual description supports this criterion: {criterion}. \
                 Use `supported` only when the description states it directly, `unsupported` \
                 when the description is silent or contradicts it, and `ambiguous` when it \
                 cannot be decided. The description is one model's observation, not \
                 independent pixel evidence."
            ),
            evidence: format!(
                "CRITERION: {criterion}\n\nVISUAL DESCRIPTION ({model}):\n{response}",
                model = observation.model,
                response = observation.response
            ),
            labels: decision_labels(),
            image: None,
            max_tokens: 256,
        })
        .await
    {
        Ok(verdict) => verdict,
        Err(error) => {
            return outcome(
                spec,
                context,
                CheckStatus::Error,
                error.to_string(),
                vec![vision_evidence],
            );
        }
    };
    let semantic = semantic_evidence(
        "vl->d1",
        &verdict,
        policy,
        Vec::new(),
        if verdict.label == "supported" {
            Vec::new()
        } else {
            vec!["/".to_owned()]
        },
    );
    let mut evidence = vec![vision_evidence];
    evidence.push(judged_evidence(
        context,
        &format!("input:{image_name}"),
        image_digest,
        &verdict,
        semantic,
    ));
    match verdict.label.as_str() {
        "supported" if verdict_clears(&verdict, policy) => outcome(
            spec,
            context,
            CheckStatus::Pass,
            format!("the visual evidence chain supports {criterion:?}"),
            evidence,
        ),
        "supported" => outcome(
            spec,
            context,
            CheckStatus::Error,
            format!(
                "the visual judgement supported the criterion at {:.0}% confidence, below the {:.0}% threshold",
                verdict.confidence * 100.0,
                policy.threshold() * 100.0
            ),
            evidence,
        ),
        "unsupported" => outcome(
            spec,
            context,
            CheckStatus::Fail,
            format!("the visual evidence does not support {criterion:?}"),
            evidence,
        ),
        _ => outcome(
            spec,
            context,
            CheckStatus::Error,
            "the visual judgement is ambiguous".to_owned(),
            evidence,
        ),
    }
}

fn decision_labels() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "supported".to_owned(),
            "the evidence states the criterion directly".to_owned(),
        ),
        (
            "unsupported".to_owned(),
            "the evidence is silent, contradicts it, or lacks a required detail".to_owned(),
        ),
        (
            "ambiguous".to_owned(),
            "the evidence cannot decide the criterion".to_owned(),
        ),
    ])
}

fn grounding_labels() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "grounded".to_owned(),
            "the source states this exact field value".to_owned(),
        ),
        (
            "unsupported".to_owned(),
            "the source does not state this field value".to_owned(),
        ),
        (
            "ambiguous".to_owned(),
            "the source is insufficient to decide".to_owned(),
        ),
    ])
}

fn verdict_clears(verdict: &JudgeVerdict, policy: &SemanticPolicy) -> bool {
    verdict.confidence + f64::EPSILON >= policy.threshold()
}

fn semantic_evidence(
    chain: &str,
    verdict: &JudgeVerdict,
    policy: &SemanticPolicy,
    pointers: Vec<String>,
    unsupported: Vec<String>,
) -> SemanticEvidence {
    SemanticEvidence {
        chain: chain.to_owned(),
        model: verdict.model.clone(),
        prompt: verdict.prompt.clone(),
        response: verdict.response.clone(),
        label: verdict.label.clone(),
        confidence_bps: confidence_bps(verdict.confidence),
        threshold_bps: policy.min_confidence_bps,
        pointers,
        unsupported,
    }
}

fn confidence_bps(confidence: f64) -> u16 {
    let scaled = (confidence * 10_000.0).round();
    if scaled.is_finite() && (0.0..=10_000.0).contains(&scaled) {
        scaled as u16
    } else {
        0
    }
}

fn outcome(
    spec: &CheckSpec,
    context: &CheckContext,
    status: CheckStatus,
    message: impl Into<String>,
    evidence: Vec<CheckEvidence>,
) -> CheckResult {
    CheckResult {
        kind: spec.kind.clone(),
        acceptance_hash: context.acceptance_hash.clone(),
        step_index: context.step_index,
        attempt: context.attempt.clone(),
        status,
        message: message.into(),
        evidence,
    }
}

fn digest_evidence(
    context: &CheckContext,
    source: &str,
    digest: ContentDigest,
    detail: impl Into<String>,
) -> CheckEvidence {
    CheckEvidence {
        source: format!("workspace:{source}"),
        attempt: Some(context.attempt.clone()),
        digest: Some(digest),
        detail: detail.into(),
        semantic: None,
    }
}

fn judged_evidence(
    context: &CheckContext,
    source: &str,
    digest: ContentDigest,
    verdict: &JudgeVerdict,
    semantic: SemanticEvidence,
) -> CheckEvidence {
    CheckEvidence {
        source: format!("semantic:{}", semantic.chain),
        attempt: Some(context.attempt.clone()),
        digest: Some(digest),
        detail: format!(
            "{} judged {source} as {} at {:.0}% ({})",
            semantic.model,
            verdict.label,
            verdict.confidence * 100.0,
            verdict.rationale
        ),
        semantic: Some(semantic),
    }
}

fn policy_evidence(context: &CheckContext, reason: &str) -> CheckEvidence {
    CheckEvidence {
        source: "policy:semantic".to_owned(),
        attempt: Some(context.attempt.clone()),
        digest: None,
        detail: reason.to_owned(),
        semantic: None,
    }
}

fn read_workspace_text(
    context: &CheckContext,
    raw: &str,
) -> Result<(String, ContentDigest), String> {
    let key = normalized(raw);
    let path = paths::resolve(&context.workspace, raw, Mode::Read)
        .map_err(|error| format!("{key}: {error:?}"))?;
    let (digest, bytes) =
        ContentDigest::of_file(&path).map_err(|error| format!("{key}: {error}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{key} is not UTF-8 text"))?;
    Ok((text, digest))
}

fn read_report_text(context: &CheckContext) -> Option<(String, ContentDigest, String)> {
    let raw = context.report_path.as_deref()?;
    read_workspace_text(context, raw)
        .ok()
        .map(|(text, digest)| (text, digest, raw.to_owned()))
}

fn read_input_bytes(
    context: &CheckContext,
    name: &str,
) -> Result<(ContentDigest, Vec<u8>), String> {
    let input = context
        .inputs
        .get(name)
        .ok_or_else(|| format!("input {name:?} is not part of the frozen acceptance"))?;
    let store = context
        .artifacts
        .as_ref()
        .ok_or_else(|| "no artifact store is available for the input".to_owned())?;
    let bytes = store
        .read(&input.sha256)
        .map_err(|error| format!("read input {name:?}: {error}"))?;
    let digest = ContentDigest::of(&bytes, "application/octet-stream");
    if digest.sha256 != input.sha256 {
        return Err(format!(
            "input {name:?} bytes do not match the frozen acceptance digest"
        ));
    }
    Ok((digest, bytes))
}

fn read_input_text(context: &CheckContext, name: &str) -> Result<(ContentDigest, String), String> {
    let (digest, bytes) = read_input_bytes(context, name)?;
    let text = String::from_utf8(bytes).map_err(|_| format!("input {name:?} is not UTF-8 text"))?;
    Ok((digest, text))
}

fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"BM") {
        return Some(media_type_for(std::path::Path::new("bitmap.bmp")));
    }
    None
}

fn resolve_pointer<'a>(value: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer.is_empty() || pointer == "/" {
        return Some(value);
    }
    let mut current = value;
    for token in pointer.strip_prefix('/')?.split('/') {
        let token = token.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(map) => map.get(&token)?,
            Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn normalize(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn string_arg<'a>(spec: &'a CheckSpec, key: &str) -> Option<&'a str> {
    spec.args
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn normalized(raw: &str) -> String {
    raw.replace('\\', "/")
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map_or(0, u32::from);
        let third = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (first << 16) | (second << 8) | third;
        encoded.push(ALPHABET[((triple >> 18) & 63) as usize] as char);
        encoded.push(ALPHABET[((triple >> 12) & 63) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 63) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(triple & 63) as usize] as char
        } else {
            '='
        });
    }
    encoded
}
