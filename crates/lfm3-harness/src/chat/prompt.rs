//! Prompt, tool schema, and transcript shaping for the cowork chat loop.

use serde_json::{json, Value};

use crate::{
    model::{ChatMessage, ChatRole, MessageContent, ToolCall, ToolDefinition},
    ChatMessageRecord,
};

/// The fixed, local-only behavior contract for every chat turn.
pub const SYSTEM_PROMPT: &str = "\
You are LFM3 Workspace, a local cowork assistant running entirely on this machine. \
You help one person think and get work done in their workspace. You are not a generic \
internet assistant: you cannot browse, download, run commands, or reach any network.

Rules:
- Use the tools to inspect or change workspace files. Never claim you read, created, \
or edited a file unless a tool result confirms it.
- Every write and edit needs explicit user approval. Call the tool with your intended \
change; the user sees a review card and decides.
- Prefer small, reversible changes. Show paths relative to the workspace.
- If a request is ambiguous or destructive, ask one short clarifying question first.
- Do not print tool arguments or raw tool JSON as prose. Answer the person directly.
- Keep answers concise and concrete. Use Markdown for structure when it helps.
- If a tool fails, say what failed and what you need to continue. Do not invent success.

Working with a folder:
- Start with list_files and search_files, then read_file the files that matter. Do not \
summarize a file you have not opened in this turn.
- search_files finds names and text across the workspace. related_files compares one \
file's distinctive words with every other text file; use it when asked what is related.
- For a report, name the output path, gather evidence first, then call write_file with a \
complete Markdown report. Cite paths and concrete observations; never invent facts.
- Images can be inspected with look and audio can be transcribed with transcribe when \
those local models are installed. Use the exact workspace-relative path.

Voice:
- Spoken replies are produced by the interface, not by a tool. Write the answer as text; \
the person can play it with the local voice control.";

/// Tool capabilities available to one chat turn. The engine grants exactly
/// these; a missing local model is never advertised.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChatCapabilities {
    pub vision: bool,
    pub audio: bool,
}

/// System message including the workspace boundary the tools enforce.
#[must_use]
pub fn system_message(workspace: &str) -> ChatMessage {
    ChatMessage::system(format!(
        "{SYSTEM_PROMPT}\n\nWorkspace root: {workspace}\n\
         Paths are relative to this root; `..` and absolute paths are rejected."
    ))
}

/// System message plus the decision model's routing hint for this turn.
#[must_use]
pub fn system_message_with_route(workspace: &str, route: &str) -> ChatMessage {
    ChatMessage::system(format!(
        "{SYSTEM_PROMPT}\n\nWorkspace root: {workspace}\n\
         Paths are relative to this root; `..` and absolute paths are rejected.\n\n\
         Decision-model route for this turn: {route}"
    ))
}

/// The tools a chat turn may call. The engine grants exactly these to a
/// workspace-confined registry; no shell, network, or arbitrary process tool
/// exists here.
#[must_use]
pub fn tool_definitions(capabilities: ChatCapabilities) -> Vec<ToolDefinition> {
    let mut tools = vec![
        ToolDefinition::function(
            "list_files",
            "List files and directories inside the workspace. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative directory path. Defaults to the workspace root."
                    }
                },
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "read_file",
            "Read one UTF-8 text file inside the workspace. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative file path."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "search_files",
            "Search workspace file names and text for words. Read-only. Use it to locate \
a file, a topic, or every file that mentions something.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to find."},
                    "path": {"type": "string", "description": "Relative directory to search. Defaults to the workspace root."},
                    "mode": {"type": "string", "enum": ["name", "content", "both"], "description": "Match names, text, or both. Defaults to both."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 40, "description": "Maximum files to report. Defaults to 20."}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "related_files",
            "Find other workspace files that share distinctive terms with one source file. \
Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative source file path."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 30, "description": "Maximum related files to report. Defaults to 10."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "write_file",
            "Create or replace a UTF-8 text file. Requires user approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative file path."},
                    "content": {"type": "string", "description": "Complete new file content."}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "edit_file",
            "Replace one exact occurrence of text in a file. Requires user approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative file path."},
                    "old_text": {"type": "string", "description": "Exact text to replace; must occur once."},
                    "new_text": {"type": "string", "description": "Replacement text."}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        ),
    ];
    if capabilities.vision {
        tools.push(ToolDefinition::function(
            "look",
            "Inspect one image with the local vision model. Read-only. Use a workspace-relative \
path to a PNG, JPEG, WebP, or GIF.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative image path."},
                    "instruction": {"type": "string", "description": "What to determine from the image."}
                },
                "required": ["path", "instruction"],
                "additionalProperties": false
            }),
        ));
    }
    if capabilities.audio {
        tools.push(ToolDefinition::function(
            "transcribe",
            "Transcribe one audio file with the local audio model. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative audio path."},
                    "language": {"type": "string", "description": "Optional language hint."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ));
    }
    tools
}

/// Converts durable records into a bounded model transcript.
///
/// The newest messages win when the budget is tight, so the current request
/// and its tool observations are never dropped in favor of old banter.
#[must_use]
pub fn to_model_messages(records: &[ChatMessageRecord], budget_chars: usize) -> Vec<ChatMessage> {
    let mut selected: Vec<ChatMessage> = Vec::new();
    let mut used = 0_usize;
    for record in records.iter().rev() {
        if record.status == "streaming" {
            continue;
        }
        let message = to_model_message(record);
        let size = message_size(&message);
        if !selected.is_empty() && used.saturating_add(size) > budget_chars {
            break;
        }
        used = used.saturating_add(size);
        selected.push(message);
    }
    selected.reverse();
    selected
}

fn to_model_message(record: &ChatMessageRecord) -> ChatMessage {
    match record.role {
        ChatRole::Assistant => {
            let tool_calls = record
                .meta
                .get("tool_calls")
                .and_then(|value| serde_json::from_value::<Vec<ToolCall>>(value.clone()).ok())
                .unwrap_or_default();
            ChatMessage {
                role: ChatRole::Assistant,
                content: MessageContent::Text(record.content.clone()),
                name: None,
                tool_call_id: None,
                tool_calls,
            }
        }
        ChatRole::Tool => {
            let call_id = record
                .meta
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            ChatMessage::tool(call_id, record.content.clone())
        }
        ChatRole::System => ChatMessage::system(record.content.clone()),
        ChatRole::User => ChatMessage::user(record.content.clone()),
    }
}

fn message_size(message: &ChatMessage) -> usize {
    let content = match &message.content {
        MessageContent::Text(text) => text.len(),
        MessageContent::Parts(_) => 512,
    };
    content
        + message
            .tool_calls
            .iter()
            .map(|call| call.function.arguments.len() + call.function.name.len())
            .sum::<usize>()
}

/// Accepts one fenced ```tool block as a fallback for small models whose chat
/// template does not expose native tool calls. The block never executes by
/// itself: it becomes a normal tool request and still needs approval.
#[must_use]
pub fn fallback_tool_call(content: &str) -> Option<(String, ToolCall)> {
    let start = content.find("```tool")?;
    let after = &content[start + "```tool".len()..];
    let body_start = after.find('\n')? + 1;
    let rest = &after[body_start..];
    let end = rest.find("```")?;
    let body = rest[..end].trim();
    let parsed: Value = serde_json::from_str(body).ok()?;
    let name = parsed.get("tool").and_then(Value::as_str)?.to_owned();
    let arguments = parsed.get("arguments")?.clone();
    let call = ToolCall {
        id: format!("fallback-{}", uuid::Uuid::new_v4().simple()),
        kind: "function".to_owned(),
        function: crate::model::FunctionCall {
            name,
            arguments: arguments.to_string(),
        },
    };
    let mut remaining = String::new();
    remaining.push_str(&content[..start]);
    remaining.push_str(&rest[end + 3..]);
    let cleaned = remaining.trim().to_owned();
    Some((cleaned, call))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(seq: i64, role: ChatRole, content: &str, meta: Value) -> ChatMessageRecord {
        ChatMessageRecord {
            id: format!("m{seq}"),
            session_id: "s".to_owned(),
            seq,
            role,
            content: content.to_owned(),
            status: "complete".to_owned(),
            meta,
            created_at: seq,
        }
    }

    #[test]
    fn transcript_keeps_the_newest_messages_within_budget() {
        let records = vec![
            record(1, ChatRole::User, &"a".repeat(400), json!({})),
            record(2, ChatRole::Assistant, &"b".repeat(400), json!({})),
            record(3, ChatRole::User, "latest question", json!({})),
        ];
        let messages = to_model_messages(&records, 100);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].content,
            MessageContent::Text("latest question".to_owned())
        );
    }

    #[test]
    fn tool_records_reconstruct_the_tool_call_id() {
        let records = vec![
            record(
                1,
                ChatRole::Assistant,
                "",
                json!({"tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}
                }]}),
            ),
            record(
                2,
                ChatRole::Tool,
                "file body",
                json!({"tool_call_id": "call-1"}),
            ),
        ];
        let messages = to_model_messages(&records, 10_000);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn fallback_block_becomes_a_tool_call_and_is_removed_from_prose() {
        let content = "I will write that.\n```tool\n{\"tool\":\"write_file\",\"arguments\":{\"path\":\"a.md\",\"content\":\"hi\"}}\n```\nDone.";
        let (cleaned, call) = fallback_tool_call(content).expect("fallback");
        assert_eq!(call.function.name, "write_file");
        assert!(!cleaned.contains("```tool"));
        assert!(cleaned.contains("I will write that."));
        assert!(cleaned.contains("Done."));
    }
}
