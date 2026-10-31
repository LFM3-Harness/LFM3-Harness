//! Decision-model routing for one chat turn.
//!
//! The calibrated d1 model chooses the intent; a deterministic keyword route is
//! the fallback when d1 is absent, unavailable, or not confident. Routing never
//! grants authority: it only selects which local capability the turn is likely
//! to need and which tools the text model should see.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{
    model::{DecisionRequest, ModelKey, ModelManager},
    CancellationChild, ToolName,
};

/// What the person is asking the workspace agent to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatIntent {
    General,
    Explore,
    Summarize,
    Related,
    Report,
    Vision,
    Voice,
}

impl ChatIntent {
    pub const ALL: [Self; 7] = [
        Self::General,
        Self::Explore,
        Self::Summarize,
        Self::Related,
        Self::Report,
        Self::Vision,
        Self::Voice,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Explore => "explore",
            Self::Summarize => "summarize",
            Self::Related => "related",
            Self::Report => "report",
            Self::Vision => "vision",
            Self::Voice => "voice",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::General => "conversation, a question, or a small file edit",
            Self::Explore => "find or locate files, or analyse a folder's contents",
            Self::Summarize => "read one or more files and summarise them",
            Self::Related => "find files related to, similar to, or depending on another file",
            Self::Report => "produce a written report or deliverable from workspace material",
            Self::Vision => "inspect an image, photo, picture, screenshot, or diagram",
            Self::Voice => "listen to, transcribe, or discuss an audio recording",
        }
    }

    /// A short, concrete instruction for the acting text model.
    #[must_use]
    pub fn hint(self) -> &'static str {
        match self {
            Self::General => "Answer directly. Use tools only when the request needs workspace evidence.",
            Self::Explore => {
                "Map the relevant folder with list_files and search_files before drawing conclusions."
            }
            Self::Summarize => {
                "Open the named files with read_file, then summarise only what they actually say."
            }
            Self::Related => {
                "Identify the seed file, use related_files, and confirm each lead with read_file."
            }
            Self::Report => {
                "Gather evidence first, then propose a Markdown report and write it with write_file. \
                 The report must cite real paths and observations."
            }
            Self::Vision => {
                "Inspect the named image with look, then answer from the observation."
            }
            Self::Voice => {
                "Transcribe the named audio with transcribe, then answer from the transcript."
            }
        }
    }

    fn label(self) -> &'static str {
        self.as_str()
    }
}

/// How the intent was selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteSource {
    Decision,
    Heuristic,
}

/// One local model role considered for the turn.
#[derive(Clone, Debug, Serialize)]
pub struct RouteRole {
    pub role: &'static str,
    pub model: ModelKey,
    pub capability: &'static str,
    pub used: bool,
}

/// The routing result shown in the UI and injected into the prompt.
#[derive(Clone, Debug, Serialize)]
pub struct ChatRoute {
    pub intent: ChatIntent,
    pub source: RouteSource,
    pub confidence_bps: u16,
    pub reason: String,
    pub model: Option<String>,
    pub roles: Vec<RouteRole>,
}

impl ChatRoute {
    #[must_use]
    pub fn heuristic(message: &str) -> Self {
        let intent = heuristic_intent(message);
        Self {
            intent,
            source: RouteSource::Heuristic,
            confidence_bps: 0,
            reason: format!(
                "keyword route chose {} because the request matches its task shape",
                intent.as_str()
            ),
            model: None,
            roles: roles_for(intent, false),
        }
    }

    /// Tools the route suggests. The engine still intersects this with what is
    /// actually installed and granted.
    #[must_use]
    pub fn tools(&self) -> Vec<ToolName> {
        let mut tools = vec![
            ToolName::ListFiles,
            ToolName::ReadFile,
            ToolName::WriteFile,
            ToolName::EditFile,
            ToolName::SearchFiles,
            ToolName::RelatedFiles,
        ];
        if matches!(self.intent, ChatIntent::Vision | ChatIntent::Report) {
            tools.push(ToolName::Look);
        }
        if matches!(self.intent, ChatIntent::Voice) {
            tools.push(ToolName::Transcribe);
        }
        tools
    }

    /// One-line summary for the prompt and the status line.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{} ({})", self.intent.as_str(), self.intent.hint())
    }
}

/// Asks d1 to classify the request, then falls back to keywords rather than
/// failing the turn. A low-confidence answer is not treated as authority.
pub async fn route_request(
    models: &ModelManager,
    message: &str,
    cancel: &CancellationChild,
) -> ChatRoute {
    let fallback = ChatRoute::heuristic(message);
    if message.trim().chars().count() < 24 || matches!(fallback.intent, ChatIntent::General) {
        return fallback;
    }
    let labels = ChatIntent::ALL
        .into_iter()
        .map(|intent| (intent.label().to_owned(), intent.description().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let request = DecisionRequest::new(
        "Classify one request to a local workspace assistant. Choose the single best \
         task shape. The assistant can list, search, read, summarise, relate, and write \
         workspace files, inspect images, and transcribe audio.",
        message.trim(),
        labels,
    );
    match models.acquire(ModelKey::D1, cancel).await {
        Ok(lease) => match lease.decide(request, cancel).await {
            Ok(decision) => {
                let confidence_bps =
                    (decision.confidence.clamp(0.0, 1.0) * 10_000.0).round() as u16;
                let Some(intent) = ChatIntent::ALL
                    .into_iter()
                    .find(|intent| intent.label() == decision.label)
                else {
                    return fallback;
                };
                if confidence_bps < 5_500 || intent == ChatIntent::General {
                    return ChatRoute {
                        intent: fallback.intent,
                        source: RouteSource::Heuristic,
                        confidence_bps,
                        reason: format!(
                            "the decision model was not confident enough ({confidence_bps} bps); \
                             kept the keyword route"
                        ),
                        model: Some(decision.model),
                        roles: roles_for(fallback.intent, true),
                    };
                }
                ChatRoute {
                    intent,
                    source: RouteSource::Decision,
                    confidence_bps,
                    reason: if decision.rationale.trim().is_empty() {
                        intent.hint().to_owned()
                    } else {
                        decision.rationale
                    },
                    model: Some(decision.model),
                    roles: roles_for(intent, true),
                }
            }
            Err(_) => fallback,
        },
        Err(_) => fallback,
    }
}

fn roles_for(intent: ChatIntent, decision_used: bool) -> Vec<RouteRole> {
    vec![
        RouteRole {
            role: "route",
            model: ModelKey::D1,
            capability: "decision",
            used: decision_used,
        },
        RouteRole {
            role: "answer",
            model: ModelKey::Text,
            capability: "conversation",
            used: true,
        },
        RouteRole {
            role: "observe",
            model: ModelKey::Vision,
            capability: "vision",
            used: matches!(intent, ChatIntent::Vision | ChatIntent::Report),
        },
        RouteRole {
            role: "hear",
            model: ModelKey::Audio,
            capability: "transcription",
            used: matches!(intent, ChatIntent::Voice),
        },
    ]
}

fn heuristic_intent(message: &str) -> ChatIntent {
    let lower = message.to_ascii_lowercase();
    the_first(
        &lower,
        &[
            (
                ChatIntent::Report,
                &[
                    "report",
                    "write-up",
                    "write up",
                    "deliverable",
                    "white paper",
                    "brief me",
                ],
            ),
            (
                ChatIntent::Related,
                &[
                    "related",
                    "similar",
                    "connected to",
                    "cross-reference",
                    "duplicate",
                    "depends on",
                ],
            ),
            (
                ChatIntent::Vision,
                &[
                    "image",
                    "photo",
                    "picture",
                    "screenshot",
                    "diagram",
                    "chart",
                    "figure",
                    "look at",
                ],
            ),
            (
                ChatIntent::Voice,
                &[
                    "audio",
                    "recording",
                    "transcribe",
                    "voice",
                    "listen to",
                    "podcast",
                ],
            ),
            (
                ChatIntent::Summarize,
                &[
                    "summarize",
                    "summarise",
                    "summary",
                    "tl;dr",
                    "key points",
                    "explain this file",
                    "what does",
                ],
            ),
            (
                ChatIntent::Explore,
                &[
                    "folder",
                    "directory",
                    "find",
                    "locate",
                    "where is",
                    "which file",
                    "search",
                    "list",
                    "analyse",
                    "analyze",
                    "inventory",
                    "structure",
                    "project",
                ],
            ),
        ],
    )
    .unwrap_or(ChatIntent::General)
}

fn the_first(text: &str, groups: &[(ChatIntent, &[&str])]) -> Option<ChatIntent> {
    groups
        .iter()
        .find(|(_, words)| words.iter().any(|word| text.contains(word)))
        .map(|(intent, _)| *intent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_analysis_and_report_prefers_the_report_shape() {
        let route =
            ChatRoute::heuristic("Analyse this folder, find related files, and write me a report");
        assert_eq!(route.intent, ChatIntent::Report);
        assert!(route.tools().contains(&ToolName::SearchFiles));
        assert!(route.tools().contains(&ToolName::RelatedFiles));
    }

    #[test]
    fn photo_requests_route_to_vision_with_both_models() {
        let route = ChatRoute::heuristic("What is in this screenshot?");
        assert_eq!(route.intent, ChatIntent::Vision);
        assert!(route.tools().contains(&ToolName::Look));
        assert!(route
            .roles
            .iter()
            .any(|role| role.model == ModelKey::Vision && role.used));
        assert!(route
            .roles
            .iter()
            .any(|role| role.model == ModelKey::Text && role.used));
    }

    #[test]
    fn plain_conversation_stays_general() {
        let route = ChatRoute::heuristic("Hello, how are you?");
        assert_eq!(route.intent, ChatIntent::General);
        assert_eq!(route.source, RouteSource::Heuristic);
    }
}
