//! Content-addressed evidence shared by tools and verified checks.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use crate::store::sha256_hex;

/// Identity of one byte sequence produced or observed by the harness.
///
/// A digest is evidence of content, not of trust: checks compare these values
/// against bytes they read themselves.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContentDigest {
    pub sha256: String,
    pub size: u64,
    pub media_type: String,
}

impl ContentDigest {
    #[must_use]
    pub fn of(bytes: &[u8], media_type: impl Into<String>) -> Self {
        Self {
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
            media_type: media_type.into(),
        }
    }

    pub fn of_file(path: &Path) -> io::Result<(Self, Vec<u8>)> {
        let bytes = fs::read(path)?;
        let digest = Self::of(&bytes, media_type_for(path));
        Ok((digest, bytes))
    }

    /// True when the digest describes exactly these bytes.
    #[must_use]
    pub fn matches(&self, bytes: &[u8]) -> bool {
        self.size == bytes.len() as u64 && self.sha256 == sha256_hex(bytes)
    }
}

/// Media type inferred from an extension; never from model-supplied text.
#[must_use]
pub fn media_type_for(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "txt" | "md" | "log" => "text/plain",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}
