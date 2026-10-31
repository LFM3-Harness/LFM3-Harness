//! Content-addressed artifact files written atomically.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{ArtifactId, HarnessError};

/// A stored artifact plus the identity of its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub sha256: String,
    pub size: u64,
    pub media_type: String,
}

/// Owns the artifact directory inside trusted engine storage.
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Location derived only from the content hash; no caller-chosen names.
    #[must_use]
    pub fn path_for(&self, sha256: &str) -> PathBuf {
        let shard = sha256.get(..2).unwrap_or("00");
        self.root.join(shard).join(sha256)
    }

    /// Writes through a temporary file, flushes, then renames into place.
    pub fn write(&self, sha256: &str, bytes: &[u8]) -> Result<PathBuf, HarnessError> {
        let path = self.path_for(sha256);
        if path.is_file() {
            return Ok(path);
        }
        let directory = path
            .parent()
            .ok_or_else(|| HarnessError::Storage("artifact path has no parent".to_owned()))?;
        fs::create_dir_all(directory).map_err(|error| {
            HarnessError::Storage(format!("create {}: {error}", directory.display()))
        })?;
        let temporary = directory.join(format!(".{sha256}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|error| {
                    HarnessError::Storage(format!("create {}: {error}", temporary.display()))
                })?;
            file.write_all(bytes).map_err(|error| {
                HarnessError::Storage(format!("write {}: {error}", temporary.display()))
            })?;
            file.sync_all().map_err(|error| {
                HarnessError::Storage(format!("flush {}: {error}", temporary.display()))
            })?;
            drop(file);
            fs::rename(&temporary, &path).map_err(|error| {
                HarnessError::Storage(format!("rename into {}: {error}", path.display()))
            })?;
            Ok(path.clone())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn read(&self, sha256: &str) -> Result<Vec<u8>, HarnessError> {
        let path = self.path_for(sha256);
        fs::read(&path)
            .map_err(|error| HarnessError::Storage(format!("read {}: {error}", path.display())))
    }
}

/// Lowercase SHA-256 hex digest of the supplied bytes.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
