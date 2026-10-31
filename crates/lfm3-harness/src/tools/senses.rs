//! Sense adapters. They can only ask the shared `ModelManager`; they never
//! construct a client or bypass a lease.
//!
//! The vision and audio models are observers, not planners. Each call returns
//! two clearly separated things: facts the harness measured itself (format,
//! bytes, dimensions, duration) and the model's observation. The text model can
//! then reason over evidence without trusting the observer to know a file's
//! properties or to certify a conclusion.

use std::time::Instant;

use serde::Deserialize;

use super::{
    check_cancelled, clip, finish, io_error, parse_args, Access, ToolError, ToolRegistry,
    ToolRequest, ToolStatus, MAX_OUTPUT_CHARS,
};
use crate::agent::profile::{LFM3_AUDIO, LFM3_VL};
use crate::model::{
    ChatMessage, ChatRequest, ContentPart, ImageUrl, ModelError, ModelKey, TranscribeRequest,
};
use crate::verify::{media_type_for, ContentDigest};
use crate::CancellationChild;

/// Largest image handed to the vision model in one call.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Largest recording handed to the audio model in one call.
const MAX_AUDIO_BYTES: u64 = 64 * 1024 * 1024;

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
    let metadata = std::fs::metadata(&path).map_err(|error| io_error(&args.path, error))?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: format!(
                "the image is {} bytes; one look call accepts at most {MAX_IMAGE_BYTES}",
                metadata.len()
            ),
        });
    }
    let bytes = std::fs::read(&path).map_err(|error| io_error(&args.path, error))?;
    let media_type = media_type_for(&path);
    if !media_type.starts_with("image/") {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: "look expects a PNG, JPEG, WebP, or GIF image".to_owned(),
        });
    }
    let facts = image_facts(&args.path, media_type, &bytes);
    let digest = ContentDigest::of(&bytes, media_type);
    let data_url = format!("data:{media_type};base64,{}", encode_base64(&bytes));

    let chat = ChatRequest::new(vec![
        ChatMessage::system(format!(
            "You are a bounded visual observer for a verification harness. {} \
             Describe only what is visibly present in the image. Never invent text, controls, \
             states, or file properties. If the image does not show what the instruction asks, \
             say so. Answer with short, checkable observations.",
            LFM3_VL.guidance
        )),
        ChatMessage::user_parts(vec![
            ContentPart::Text {
                text: args.instruction,
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl { url: data_url },
            },
        ]),
    ])
    .with_sampling(LFM3_VL.sampling)
    .with_max_tokens(LFM3_VL.max_output_tokens);

    check_cancelled(cancel)?;
    let lease = models.acquire(ModelKey::Vision, cancel).await?;
    let response = lease.chat(chat, cancel).await?;
    let observation = response.content().unwrap_or("").trim().to_owned();
    if observation.is_empty() {
        return Err(ToolError::Model(ModelError::Protocol(
            "vision returned no observation".to_owned(),
        )));
    }
    let output = format!(
        "{facts}\nobservation ({model}):\n{observation}",
        model = LFM3_VL.name
    );
    Ok(finish(
        request,
        started,
        ToolStatus::Succeeded,
        None,
        clip(output, MAX_OUTPUT_CHARS),
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
    let metadata = std::fs::metadata(&path).map_err(|error| io_error(&args.path, error))?;
    if metadata.len() > MAX_AUDIO_BYTES {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: format!(
                "the recording is {} bytes; one transcribe call accepts at most {MAX_AUDIO_BYTES}. \
                 Split it into shorter files.",
                metadata.len()
            ),
        });
    }
    let (digest, bytes) =
        ContentDigest::of_file(&path).map_err(|error| io_error(&args.path, error))?;
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
    let facts = audio_facts(&args.path, media_type, &bytes, transcript.duration_seconds);
    let output = format!(
        "{facts}\ntranscript ({model}, verbatim; no speaker labels or timestamps):\n{text}",
        model = LFM3_AUDIO.name,
        text = transcript.text.trim()
    );
    Ok(finish(
        request,
        started,
        ToolStatus::Succeeded,
        None,
        clip(output, MAX_OUTPUT_CHARS),
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

/// Measured image facts, so the observer never has to guess a file property.
fn image_facts(raw_path: &str, media_type: &str, bytes: &[u8]) -> String {
    let dimensions = image_dimensions(media_type, bytes)
        .map(|(width, height)| format!("{width}x{height}"))
        .unwrap_or_else(|| "unknown".to_owned());
    format!(
        "harness facts (measured): file={raw_path} format={media_type} bytes={} dimensions={dimensions}",
        bytes.len()
    )
}

/// Measured recording facts. Duration is reported only when a container
/// exposes it; the harness never guesses from a file size.
fn audio_facts(
    raw_path: &str,
    media_type: &str,
    bytes: &[u8],
    server_duration: Option<f64>,
) -> String {
    let wav = wav_facts(bytes);
    let duration = wav
        .as_ref()
        .map(|facts| format!("{:.2}s", facts.duration_seconds))
        .or_else(|| {
            server_duration
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| format!("{value:.2}s"))
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let mut facts = format!(
        "harness facts (measured): file={raw_path} format={media_type} bytes={} duration={duration}",
        bytes.len()
    );
    if let Some(wav) = wav {
        facts.push_str(&format!(
            " sample_rate={} channels={} bits={}",
            wav.sample_rate, wav.channels, wav.bits_per_sample
        ));
    }
    facts
}

fn image_dimensions(media_type: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    match media_type {
        "image/png" => png_dimensions(bytes),
        "image/jpeg" => jpeg_dimensions(bytes),
        "image/gif" => gif_dimensions(bytes),
        "image/webp" => webp_dimensions(bytes),
        _ => None,
    }
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.len() < 24 || bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return None;
    }
    let width = u32::from(u16::from_le_bytes(bytes[6..8].try_into().ok()?));
    let height = u32::from(u16::from_le_bytes(bytes[8..10].try_into().ok()?));
    (width > 0 && height > 0).then_some((width, height))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xFF {
            offset += 1;
            continue;
        }
        let mut marker_offset = offset;
        while marker_offset < bytes.len() && bytes[marker_offset] == 0xFF {
            marker_offset += 1;
        }
        let marker = *bytes.get(marker_offset)?;
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            offset = marker_offset + 1;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            return None;
        }
        let length_offset = marker_offset + 1;
        if length_offset + 2 > bytes.len() {
            return None;
        }
        let length = usize::from(u16::from_be_bytes(
            bytes[length_offset..length_offset + 2].try_into().ok()?,
        ));
        if length < 2 {
            return None;
        }
        if matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        ) {
            let start = length_offset + 2;
            if start + 5 > bytes.len() {
                return None;
            }
            let height = u32::from(u16::from_be_bytes(
                bytes[start + 1..start + 3].try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                bytes[start + 3..start + 5].try_into().ok()?,
            ));
            return (width > 0 && height > 0).then_some((width, height));
        }
        offset = length_offset.checked_add(length)?;
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        b"VP8X" => {
            let width = read_u24(&bytes[24..27]) + 1;
            let height = read_u24(&bytes[27..30]) + 1;
            (width > 0 && height > 0).then_some((width, height))
        }
        b"VP8 " => {
            if bytes.len() < 30 || bytes[23..26] != [0x9D, 0x01, 0x2A] {
                return None;
            }
            let width = u32::from(u16::from_le_bytes(bytes[26..28].try_into().ok()?) & 0x3FFF);
            let height = u32::from(u16::from_le_bytes(bytes[28..30].try_into().ok()?) & 0x3FFF);
            (width > 0 && height > 0).then_some((width, height))
        }
        b"VP8L" => {
            if bytes.len() < 25 || bytes[20] != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
            let width = 1 + (bits & 0x3FFF);
            let height = 1 + ((bits >> 14) & 0x3FFF);
            (width > 0 && height > 0).then_some((width, height))
        }
        _ => None,
    }
}

fn read_u24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

struct WavFacts {
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    duration_seconds: f64,
}

/// Reads duration and format facts from a RIFF/WAVE header. Other containers
/// return `None`; the harness does not estimate their duration.
fn wav_facts(bytes: &[u8]) -> Option<WavFacts> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut offset = 12usize;
    let mut channels = None;
    let mut sample_rate = None;
    let mut byte_rate = None;
    let mut bits_per_sample = None;
    let mut data_size = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        let body = offset + 8;
        match id {
            b"fmt " if body + 16 <= bytes.len() => {
                channels = Some(u16::from_le_bytes(
                    bytes[body + 2..body + 4].try_into().ok()?,
                ));
                sample_rate = Some(u32::from_le_bytes(
                    bytes[body + 4..body + 8].try_into().ok()?,
                ));
                byte_rate = Some(u32::from_le_bytes(
                    bytes[body + 8..body + 12].try_into().ok()?,
                ));
                bits_per_sample = Some(u16::from_le_bytes(
                    bytes[body + 14..body + 16].try_into().ok()?,
                ));
            }
            b"data" => {
                data_size = Some(size.min(bytes.len().saturating_sub(body)));
            }
            _ => {}
        }
        let next = body
            .checked_add(size)
            .and_then(|next| next.checked_add(size & 1));
        let Some(next) = next else { break };
        if next <= offset {
            break;
        }
        offset = next;
    }
    let byte_rate = byte_rate.filter(|rate| *rate > 0)?;
    let data_size = data_size?;
    let channels = channels?;
    let sample_rate = sample_rate?;
    let bits_per_sample = bits_per_sample?;
    Some(WavFacts {
        sample_rate,
        channels,
        bits_per_sample,
        duration_seconds: data_size as f64 / f64::from(byte_rate),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn gif(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes
    }

    fn wav(sample_rate: u32, channels: u16, bits: u16, samples: u32) -> Vec<u8> {
        let block_align = channels * (bits / 8);
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = samples * u32::from(block_align);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.resize(bytes.len() + data_size as usize, 0);
        bytes
    }

    #[test]
    fn image_facts_are_measured_not_guessed() {
        let bytes = png(800, 600);
        let facts = image_facts("shot.png", "image/png", &bytes);
        assert!(facts.contains(&format!("bytes={}", bytes.len())), "{facts}");
        assert!(facts.contains("dimensions=800x600"), "{facts}");
        assert_eq!(gif_dimensions(&gif(320, 200)), Some((320, 200)));
    }

    #[test]
    fn an_unreadable_header_reports_unknown_dimensions() {
        let facts = image_facts("odd.webp", "image/webp", b"not really a webp");
        assert!(facts.contains("dimensions=unknown"), "{facts}");
    }

    #[test]
    fn wav_facts_carry_duration_and_format() {
        let bytes = wav(16_000, 1, 16, 16_000);
        let facts = audio_facts("note.wav", "audio/wav", &bytes, None);
        assert!(facts.contains("duration=1.00s"), "{facts}");
        assert!(facts.contains("sample_rate=16000"), "{facts}");
        assert!(facts.contains("channels=1"), "{facts}");
    }

    #[test]
    fn an_opaque_container_reports_no_invented_duration() {
        let facts = audio_facts("note.mp3", "audio/mpeg", b"ID3", None);
        assert!(facts.contains("duration=unknown"), "{facts}");
        assert!(!facts.contains("sample_rate"));
    }
}
