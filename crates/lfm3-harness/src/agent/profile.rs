//! The operating envelope derived from the local models' measured profiles.
//!
//! The harness is deliberately built around what the small local models do
//! well, not around what a frontier model would do. LFM3-1.7B follows bounded
//! instructions, emits tool-call JSON, and summarizes short passages it has
//! actually read. It does not hold a long plan in its head, recall unread file
//! content, or certify its own work. Those boundaries become harness rules:
//!
//! - a short written plan before any side effect;
//! - one bounded action per turn, never a long silent chain;
//! - a deterministic check for every claimed step;
//! - a review turn after the output is written;
//! - completion measured by the harness, never by the model's word.
//!
//! Observation models (vision, audio) and the d1 classifier are tools inside
//! that loop, never planners or completion authorities. Each profile below
//! names both sides of the model and the harness helper that covers the gap.

use serde_json::{json, Value};

use super::loop_agent::LoopOptions;
use crate::model::Sampling;

/// A local model's operating envelope, in user-visible terms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelProfile {
    /// Model identity as it appears in the health report.
    pub name: &'static str,
    /// The single harness role this model is trusted with.
    pub role: &'static str,
    /// Tasks the model reliably performs.
    pub strengths: &'static [&'static str],
    /// Tasks the harness must not delegate to the model.
    pub weaknesses: &'static [&'static str],
    /// Harness mechanisms that cover those weaknesses.
    pub helpers: &'static [&'static str],
    /// Guidance injected into the model's prompt.
    pub guidance: &'static str,
    /// Whether this model may run the planning/acting loop at all.
    pub agentic: bool,
    /// Decoding settings from the model card.
    pub sampling: Sampling,
    /// Hard ceiling for one job on this engine.
    pub max_turns: u32,
    /// Tool calls accepted in one turn.
    pub max_calls: u32,
    /// Recent observations replayed into the prompt.
    pub replay: usize,
    /// Output token budget for one acting turn.
    pub max_output_tokens: u32,
    /// Maximum steps in the written plan.
    pub max_plan_steps: usize,
}

/// The text model that plans and acts. Its limits shape every job.
pub const LFM3_TEXT: ModelProfile = ModelProfile {
    name: "LFM3-1.7B",
    role: "planning, bounded acting, and short-extract synthesis",
    strengths: &[
        "following one bounded instruction with one to three tool calls",
        "summarizing or extracting from a short passage it has just read",
        "emitting strict JSON and tool-call envelopes under a constrained schema",
        "making a concrete local edit or short complete rewrite",
        "using a concrete failure message to change the next action",
    ],
    weaknesses: &[
        "holding a long multi-step plan without writing it down",
        "recalling file content it has not read",
        "certifying its own work without a measured check",
        "inventing plausible figures or facts that no source contains",
        "recovering from repeated or failed actions without explicit feedback",
        "keeping a large working set of observations in context",
    ],
    helpers: &[
        "written plan gate before any workspace action",
        "deterministic todo checks (file_exists, dir_exists, content_matches)",
        "read-before-write guard and tool-output-only evidence rule",
        "write, re-read, then finish review turn",
        "measured deliverable proof (bytes and digest)",
        "advisory grounding check for numbers absent from observed sources",
        "harness-owned stall and turn budgets",
        "route-scoped tool list",
    ],
    guidance: "Work on one bounded step at a time. Keep the plan visible. Read a file before \
               you summarize it, and never claim a file's content from memory. Treat tool output \
               as the only evidence. If a call fails, change the action instead of repeating it. \
               Write real content in one complete file, never a placeholder.",
    agentic: true,
    sampling: Sampling {
        temperature: 0.1,
        top_k: Some(50),
        repetition_penalty: Some(1.1),
    },
    max_turns: 64,
    max_calls: 3,
    replay: 8,
    max_output_tokens: 2048,
    max_plan_steps: 8,
};

/// The vision model is an observer: strong on printed text, documents,
/// charts, and grounded regions; not a planner and not a coding model.
pub const LFM3_VL: ModelProfile = ModelProfile {
    name: "LFM3-VL-2B",
    role: "bounded visual observation for one image at a time",
    strengths: &[
        "reading printed text, documents, and OCR-style passages",
        "chart, table, and document question answering",
        "grounding an answer in a visible region of the image",
        "reading GUI screenshots and native-resolution photos",
    ],
    weaknesses: &[
        "planning, multi-turn agency, or native tool calling",
        "coding and non-visual reasoning",
        "deciding whether a file is correct or complete",
        "knowing a file's format, dimensions, or size without being told",
    ],
    helpers: &[
        "measured file facts (format, bytes, dimensions) supplied by the harness",
        "single-shot observer prompt: no tools, no planning, no judging",
        "observation is evidence for the text model, never a completion decision",
        "one bounded instruction per call",
    ],
    guidance: "Observe one image at a time. Answer only the question you were asked and only \
               with what is visibly present. Read printed text verbatim when asked. Never plan, \
               call tools, or judge a file's correctness; the harness supplies measured file \
               facts and the text model does the reasoning.",
    agentic: false,
    sampling: Sampling::greedy(),
    max_turns: 1,
    max_calls: 1,
    replay: 0,
    max_output_tokens: 512,
    max_plan_steps: 0,
};

/// The audio model is a transcriber: strong ASR across 18 languages, weak at
/// understanding, knowledge, and multi-step reasoning.
pub const LFM3_AUDIO: ModelProfile = ModelProfile {
    name: "LFM3-Audio-2B",
    role: "verbatim transcription of one local recording",
    strengths: &[
        "speech recognition across 18 languages with low word error",
        "keeping the original language and wording of the speaker",
        "short spoken replies in the chat composer",
    ],
    weaknesses: &[
        "understanding, reasoning, or knowledge about audio content",
        "speaker identification and timestamped diarisation",
        "planning, tool calling, or deciding what the transcript means",
        "inferring a recording's duration or sample rate without being told",
    ],
    helpers: &[
        "measured container facts (format, bytes, duration when the container exposes it)",
        "transcription only: no speaker labels or timestamps invented",
        "audio size cap and local-only transport",
        "text model synthesises and verifies use of the transcript",
    ],
    guidance: "Transcribe one recording verbatim in its original language. Do not summarise, \
               interpret, translate, guess speakers, or invent timestamps; the harness reports \
               measured duration and the text model analyses the transcript.",
    agentic: false,
    sampling: Sampling::greedy(),
    max_turns: 1,
    max_calls: 1,
    replay: 0,
    max_output_tokens: 1024,
    max_plan_steps: 0,
};

/// d1-2B is a decision model, not a chat model. It chooses a label from a
/// closed set in one forward pass; the harness owns the confidence floor.
pub const D1: ModelProfile = ModelProfile {
    name: "d1-2B",
    role: "advisory classification and extraction over supplied evidence",
    strengths: &[
        "one fast forward pass over a short, explicit piece of evidence",
        "typed choices, scores, and extractions with a confidence value",
        "visual document and web-page decisions",
    ],
    weaknesses: &[
        "open conversation, knowledge retrieval, or long reasoning",
        "confidence that needs no calibration for a new task",
        "precise counts or scores; its numeric answers are estimates",
        "deciding a side effect is safe or that a job is complete",
        "long inputs; quality degrades beyond roughly 64K tokens",
    ],
    helpers: &[
        "closed label set with typed JSON output",
        "calibrated confidence floor; below it the keyword route wins",
        "evidence explicitly supplied and bounded",
        "advisory only: routing never grants tool authority",
    ],
    guidance: "Choose one label from the closed set for the supplied evidence. Confidence is \
               advisory: below the harness floor the deterministic keyword route wins. Never \
               answer conversation, retrieve knowledge, or approve a side effect.",
    agentic: false,
    sampling: Sampling::greedy(),
    max_turns: 1,
    max_calls: 1,
    replay: 0,
    max_output_tokens: 256,
    max_plan_steps: 0,
};

impl ModelProfile {
    /// Every model role the harness knows how to work with.
    pub const ALL: [Self; 4] = [LFM3_TEXT, LFM3_VL, LFM3_AUDIO, D1];

    /// Loop limits for one job, bounded by this profile.
    ///
    /// Only the agentic profile may plan; an observation or decision model is
    /// never handed the job loop, and this method makes that explicit rather
    /// than relying on a caller to remember.
    #[must_use]
    pub const fn loop_options(self, requested_turns: u32) -> LoopOptions {
        if !self.agentic {
            return LoopOptions {
                max_turns: 1,
                max_calls: 1,
                replay: 0,
                plan_first: false,
                max_plan_steps: 0,
            };
        }
        LoopOptions {
            max_turns: if requested_turns == 0 || requested_turns > self.max_turns {
                self.max_turns
            } else {
                requested_turns
            },
            max_calls: self.max_calls,
            replay: self.replay,
            plan_first: true,
            max_plan_steps: self.max_plan_steps,
        }
    }

    /// The profile as one JSON object for health and diagnostics.
    #[must_use]
    pub fn report(self) -> Value {
        json!({
            "name": self.name,
            "role": self.role,
            "agentic": self.agentic,
            "strengths": self.strengths,
            "weaknesses": self.weaknesses,
            "helpers": self.helpers,
            "sampling": {
                "temperature": self.sampling.temperature,
                "top_k": self.sampling.top_k,
                "repetition_penalty": self.sampling.repetition_penalty,
            },
            "limits": {
                "max_turns": self.max_turns,
                "max_calls_per_turn": self.max_calls,
                "replay_observations": self.replay,
                "max_output_tokens": self.max_output_tokens,
                "max_plan_steps": self.max_plan_steps,
            },
        })
    }

    /// All four envelopes, for the health report and diagnostics.
    #[must_use]
    pub fn all_reports() -> Value {
        Value::Array(Self::ALL.into_iter().map(Self::report).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_bounds_a_requested_turn_budget() {
        let options = LFM3_TEXT.loop_options(24);
        assert_eq!(options.max_turns, 24);
        assert_eq!(options.max_calls, 3);
        assert!(options.plan_first);
        assert_eq!(options.max_plan_steps, 8);

        assert_eq!(LFM3_TEXT.loop_options(0).max_turns, 64);
        assert_eq!(LFM3_TEXT.loop_options(999).max_turns, 64);
    }

    #[test]
    fn the_profile_names_both_sides_of_the_model() {
        let report = LFM3_TEXT.report();
        assert_eq!(report["name"], "LFM3-1.7B");
        assert!(report["strengths"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert!(report["weaknesses"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert_eq!(report["limits"]["max_calls_per_turn"], 3);
    }

    #[test]
    fn the_text_profile_uses_the_model_card_sampling() {
        assert_eq!(LFM3_TEXT.sampling.temperature, 0.1);
        assert_eq!(LFM3_TEXT.sampling.top_k, Some(50));
        assert_eq!(LFM3_TEXT.sampling.repetition_penalty, Some(1.1));
        const { assert!(LFM3_TEXT.agentic) };
    }

    #[test]
    fn observers_and_the_classifier_never_plan() {
        for profile in [LFM3_VL, LFM3_AUDIO, D1] {
            assert!(
                !profile.agentic,
                "{} must not run the job loop",
                profile.name
            );
            let options = profile.loop_options(64);
            assert_eq!(options.max_turns, 1);
            assert_eq!(options.max_calls, 1);
            assert_eq!(options.replay, 0);
            assert!(!options.plan_first);
            assert_eq!(options.max_plan_steps, 0);
            assert!(profile.helpers.len() >= 3);
            assert!(profile.weaknesses.len() >= 3);
        }
    }

    #[test]
    fn every_envelope_reports_the_helper_that_covers_its_weakness() {
        let reports = ModelProfile::all_reports();
        let reports = reports.as_array().expect("an array of envelopes");
        assert_eq!(reports.len(), 4);
        assert!(reports.iter().all(|report| report["helpers"]
            .as_array()
            .is_some_and(|items| !items.is_empty())));
        assert!(reports.iter().any(|report| report["name"] == "LFM3-VL-2B"));
        assert!(reports.iter().any(|report| report["name"] == "d1-2B"));
    }
}
