//! Sense adapters. They can only ask the shared `ModelManager`; they never
//! construct a client or bypass a lease.

use std::time::Instant;

use serde::Deserialize;

use super::{
    check_cancelled, clip, finish, io_error, parse_args, Access, ToolError, ToolRegistry,
    ToolRequest, ToolStatus, MAX_OUTPUT_CHARS,
};
use crate::model::{
    ChatMessage, ChatRequest, ContentPart, ImageUrl, ModelError, ModelKey, TranscribeRequest,
};
use crate::verify::{media_type_for, ContentDigest};
use crate::CancellationChild;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LookArgs {
    path: String,
    instruction: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscribeArgs {
    path: String,
    #[serde(default)]
    language: Option<String>,
}

pub(crate) async fn look(
    registry: &ToolRegistry,
    request: &ToolRequest,
    cancel: &CancellationChild,
    started: Instant,
) -> Result<super::ToolResult, ToolError> {
    check_cancelled(cancel)?;
    let models = registry
        .models()
        .cloned()
        .ok_or_else(|| ToolError::NotConfigured {
            tool: "look",
            reason: "no shared model manager is attached".to_owned(),
        })?;
    let args: LookArgs = parse_args(request)?;
    let path = registry.resolve(&args.path, Access::Read)?;
    let bytes = std::fs::read(&path).map_err(|error| io_error(&args.path, error))?;
    let media_type = media_type_for(&path);
    if !media_type.starts_with("image/") {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: "look expects a PNG, JPEG, WebP, or GIF image".to_owned(),
        });
    }
    let digest = ContentDigest::of(&bytes, media_type);
    let data_url = format!("data:{media_type};base64,{}", encode_base64(&bytes));

    let chat = ChatRequest::new(vec![
        ChatMessage::system(
            "You are a bounded visual observer for a verification harness. \
             Describe only what is visibly present in the image. Never invent text, \
             controls, or states. Answer the instruction with short, checkable observations.",
        ),
        ChatMessage::user_parts(vec![
            ContentPart::Text {
                text: args.instruction,
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl { url: data_url },
            },
        ]),
    ])
    .with_temperature(0.0)
    .with_max_tokens(512);

    check_cancelled(cancel)?;
    let lease = models.acquire(ModelKey::Vision, cancel).await?;
    let response = lease.chat(chat, cancel).await?;
    let observation = response.content().unwrap_or("").trim().to_owned();
    if observation.is_empty() {
        return Err(ToolError::Model(ModelError::Protocol(
            "vision returned no observation".to_owned(),
        )));
    }
    Ok(finish(
        request,
        started,
        ToolStatus::Succeeded,
        None,
        clip(observation, MAX_OUTPUT_CHARS),
        vec![digest],
    ))
}

pub(crate) async fn transcribe(
    registry: &ToolRegistry,
    request: &ToolRequest,
    cancel: &CancellationChild,
    started: Instant,
) -> Result<super::ToolResult, ToolError> {
    check_cancelled(cancel)?;
    let models = registry
        .models()
        .cloned()
        .ok_or_else(|| ToolError::NotConfigured {
            tool: "transcribe",
            reason: "no shared model manager is attached".to_owned(),
        })?;
    let args: TranscribeArgs = parse_args(request)?;
    let path = registry.resolve(&args.path, Access::Read)?;
    let (digest, _) = ContentDigest::of_file(&path).map_err(|error| io_error(&args.path, error))?;
    let media_type = media_type_for(&path);
    if !media_type.starts_with("audio/") {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: "transcribe expects a WAV, MP3, FLAC, OGG, or M4A file".to_owned(),
        });
    }

    check_cancelled(cancel)?;
    let lease = models.acquire(ModelKey::Audio, cancel).await?;
    let transcript = lease
        .transcribe(
            TranscribeRequest {
                path,
                language: args.language,
                prompt: None,
            },
            cancel,
        )
        .await?;
    if transcript.text.trim().is_empty() {
        return Err(ToolError::Model(ModelError::Protocol(
            "the transcription was empty".to_owned(),
        )));
    }
    Ok(finish(
        request,
        started,
        ToolStatus::Succeeded,
        None,
        clip(transcript.text, MAX_OUTPUT_CHARS),
        vec![digest],
    ))
}

pub(crate) async fn speak(
    _registry: &ToolRegistry,
    _request: &ToolRequest,
    _cancel: &CancellationChild,
    _started: Instant,
) -> Result<super::ToolResult, ToolError> {
    Err(ToolError::Unsupported {
        tool: "speak",
        reason:
            "local TTS is not qualified on the pinned runtime; voice output stays human-review-only"
                .to_owned(),
    })
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
