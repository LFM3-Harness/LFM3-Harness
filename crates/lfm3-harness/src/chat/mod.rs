//! Chat-first cowork state and prompts.
//!
//! A chat turn is a bounded, local, tool-using conversation. It is not a run:
//! no conversational claim can pass a mission, and every file mutation is a
//! separate user-approved action recorded in its own table.

pub mod decisions;
pub mod prompt;
pub mod router;
pub mod store;

pub use decisions::{
    check_evidence, check_write_gate, DecisionBackend, DecisionOutcome, DecisionPoint,
    HarnessDecision, EVIDENCE_GATE_FLOOR_BPS, WRITE_GATE_FLOOR_BPS,
};
pub use prompt::{
    fallback_tool_call, system_message, system_message_with_route, to_model_messages,
    tool_definitions, ChatCapabilities, SYSTEM_PROMPT,
};
pub use router::{route_request, ChatIntent, ChatRoute, RouteRole, RouteSource};
pub use store::{ChatActionRecord, ChatMessageRecord, ChatSession, ChatSessionView, ChatStore};
