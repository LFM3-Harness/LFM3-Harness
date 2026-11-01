//! Explicit, user-initiated model downloads.
//!
//! The harness never downloads on its own. A person clicks Download for one
//! role; the exact pinned GGUF files are fetched from LiquidAI's public
//! Hugging Face repositories over HTTPS, streamed to a `.part` file, checked
//! against the published byte length and SHA-256 digest, and only then moved
//! into the models folder. Nothing about the machine is uploaded.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use lfm3_harness::ModelKey;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Live download progress. Must match `DOWNLOAD_EVENT_CHANNEL` in the UI bridge.
pub const DOWNLOAD_EVENT_CHANNEL: &str = "lfm3:download-event";

/// Where the pinned weight files come from. Only this host is ever contacted.
pub const SOURCE_HOST: &str = "https://huggingface.co";
/// The public collection that owns every LFM3 and d1 weight.
pub const SOURCE_OWNER: &str = "LiquidAI";
/// The license shown next to the download button.
pub const LICENSE: &str = "LFM Open License (see the model card)";

/// One immutable file in a model repository.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// One role the harness can run, and the files that make it usable.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogModel {
    pub key: ModelKey,
    pub title: &'static str,
    pub repo: &'static str,
    pub files: Vec<CatalogFile>,
}

impl CatalogModel {
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }

    #[must_use]
    pub fn file_url(&self, file: &CatalogFile) -> String {
        format!(
            "{SOURCE_HOST}/{SOURCE_OWNER}/{}/resolve/main/{}?download=true",
            self.repo, file.name
        )
    }

    #[must_use]
    pub fn source(&self) -> String {
        format!("{SOURCE_OWNER}/{}", self.repo)
    }
}

/// The four roles, pinned to the quantizations this harness qualifies.
/// Sizes and digests come from the repository LFS metadata.
#[must_use]
pub fn catalog() -> Vec<CatalogModel> {
    vec![
        CatalogModel {
            key: ModelKey::Text,
            title: "Text and agent",
            repo: "LFM3-1.7B-GGUF",
            files: vec![CatalogFile {
                name: "LFM3-1.7B-Q4_K_M.gguf",
                bytes: 1_099_627_808,
                sha256: "3793e1e6e2266919d6e015287221b41100175a98eddaba92e111aed3b05c93c4",
            }],
        },
        CatalogModel {
            key: ModelKey::Vision,
            title: "Vision",
            repo: "LFM3-VL-2B-GGUF",
            files: vec![
                CatalogFile {
                    name: "LFM3-VL-2B-QAD-Q4_0.gguf",
                    bytes: 1_049_615_648,
                    sha256: "621c455376cb96a20891697deb9975c6fb47dd5ea98ed3ad1f3b7e8f24f0b1e6",
                },
                CatalogFile {
                    name: "mmproj-LFM3-VL-2B-Q8_0.gguf",
                    bytes: 203_031_872,
                    sha256: "c5c5b58d544a18da919172d2d0368090ea1a4a351b6feb7f8a326a264a5918ab",
                },
            ],
        },
        CatalogModel {
            key: ModelKey::Audio,
            title: "Voice in and out",
            repo: "LFM3-Audio-2B-GGUF",
            files: vec![
                CatalogFile {
                    name: "LFM3-Audio-2B-Q4_K_M.gguf",
                    bytes: 1_099_626_016,
                    sha256: "9a773289bd321ed879cd3a76545f6aca506e403851a45fb315420aee45625be2",
                },
                CatalogFile {
                    name: "mmproj-LFM3-Audio-2B-Q8_0.gguf",
                    bytes: 303_811_840,
                    sha256: "2b6648b1fd48af16aa0877b0592b144d15070783673e038a379fdbc51d40e365",
                },
            ],
        },
        CatalogModel {
            key: ModelKey::D1,
            title: "Decision",
            repo: "d1-2B-GGUF",
            files: vec![CatalogFile {
                name: "d1-2B-Q4_K_M.gguf",
                bytes: 1_099_629_888,
                sha256: "adca5789ad30ae6027e8e936749bb9f19673097492ab26fa1c5b69643ca50b4b",
            }],
        },
    ]
}

/// Finds one catalog entry by role.
#[must_use]
pub fn catalog_model(key: ModelKey) -> Option<CatalogModel> {
    catalog().into_iter().find(|model| model.key == key)
}

/// Destination directory for one repository inside the models folder.
#[must_use]
pub fn model_directory(models_dir: &Path, model: &CatalogModel) -> PathBuf {
    models_dir.join(model.repo)
}

/// The final path of one downloaded file.
#[must_use]
pub fn file_path(models_dir: &Path, model: &CatalogModel, file: &CatalogFile) -> PathBuf {
    model_directory(models_dir, model).join(file.name)
}

/// A live progress notification. The desktop shell forwards these to the UI.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownloadEvent {
    Started {
        key: ModelKey,
        total_bytes: u64,
    },
    Progress {
        key: ModelKey,
        file: String,
        received: u64,
        file_total: u64,
        overall_received: u64,
        overall_total: u64,
    },
    FileDone {
        key: ModelKey,
        file: String,
    },
    Done {
        key: ModelKey,
    },
    Failed {
        key: ModelKey,
        message: String,
    },
    Cancelled {
        key: ModelKey,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("the download was cancelled")]
    Cancelled,
    #[error("could not reach the model source: {0}")]
    Http(String),
    #[error("could not write the model file: {0}")]
    Io(String),
    #[error("the downloaded file did not match the published model: {0}")]
    Integrity(String),
}

impl DownloadError {
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// The one live download: its role and its cancellation flag.
type ActiveDownload = (ModelKey, Arc<AtomicBool>);

/// Tracks the one download this process permits at a time.
#[derive(Clone, Default)]
pub struct DownloadManager {
    inner: Arc<Mutex<Option<ActiveDownload>>>,
}

impl DownloadManager {
    /// Claims the download slot. A second request is refused rather than
    /// silently queued behind a multi-gigabyte transfer.
    pub fn begin(&self, key: ModelKey) -> Result<Arc<AtomicBool>, String> {
        let mut slot = self
            .inner
            .lock()
            .map_err(|_| "the download state is unavailable".to_owned())?;
        if let Some((active, _)) = slot.as_ref() {
            return Err(format!(
                "a {active} model download is already running; wait for it or cancel it"
            ));
        }
        let flag = Arc::new(AtomicBool::new(false));
        *slot = Some((key, Arc::clone(&flag)));
        Ok(flag)
    }

    /// Requests cancellation of the named role, if it is the active one.
    pub fn cancel(&self, key: ModelKey) -> bool {
        let Ok(slot) = self.inner.lock() else {
            return false;
        };
        match slot.as_ref() {
            Some((active, flag)) if *active == key => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    /// Releases the slot after success, failure, or cancellation.
    pub fn finish(&self, key: ModelKey) {
        if let Ok(mut slot) = self.inner.lock() {
            if slot.as_ref().is_some_and(|(active, _)| *active == key) {
                *slot = None;
            }
        }
    }

    #[must_use]
    pub fn active(&self) -> Option<ModelKey> {
        self.inner
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|(key, _)| *key))
    }
}

/// Downloads one role into `models_dir`, verifying every file.
///
/// Existing complete files are left alone, so a retry after a partial failure
/// only fetches what is missing.
pub async fn download_model(
    models_dir: &Path,
    model: &CatalogModel,
    cancel: &AtomicBool,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
) -> Result<Vec<PathBuf>, DownloadError> {
    let total = model.total_bytes();
    on_event(DownloadEvent::Started {
        key: model.key,
        total_bytes: total,
    });
    let client = reqwest::Client::builder()
        .user_agent(concat!("lfm3-harness/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| DownloadError::Http(error.to_string()))?;
    let mut written: u64 = 0;
    let mut paths = Vec::with_capacity(model.files.len());
    for file in &model.files {
        if cancel.load(Ordering::SeqCst) {
            return Err(DownloadError::Cancelled);
        }
        let destination = file_path(models_dir, model, file);
        let base = written;
        fetch_file(
            &client,
            &model.file_url(file),
            &destination,
            file,
            cancel,
            &|received, file_total| {
                on_event(DownloadEvent::Progress {
                    key: model.key,
                    file: file.name.to_owned(),
                    received,
                    file_total,
                    overall_received: base + received,
                    overall_total: total,
                });
            },
        )
        .await?;
        written = written.saturating_add(file.bytes);
        on_event(DownloadEvent::FileDone {
            key: model.key,
            file: file.name.to_owned(),
        });
        paths.push(destination);
    }
    Ok(paths)
}

async fn fetch_file(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    file: &CatalogFile,
    cancel: &AtomicBool,
    on_progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), DownloadError> {
    if destination.is_file() {
        let metadata = tokio::fs::metadata(destination)
            .await
            .map_err(|error| DownloadError::Io(error.to_string()))?;
        if metadata.len() == file.bytes {
            on_progress(file.bytes, file.bytes);
            return Ok(());
        }
    }
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| DownloadError::Io(error.to_string()))?;
    }
    let partial = destination.with_file_name(format!(
        "{}.part",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("model.gguf")
    ));
    let _ = tokio::fs::remove_file(&partial).await;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| DownloadError::Http(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(DownloadError::Http(format!(
            "the model source answered HTTP {status}"
        )));
    }
    if let Some(length) = response.content_length() {
        if length != file.bytes {
            return Err(DownloadError::Integrity(format!(
                "{} is {length} bytes at the source, not the pinned {} bytes",
                file.name, file.bytes
            )));
        }
    }

    let mut stream = response.bytes_stream();
    let mut handle = tokio::fs::File::create(&partial)
        .await
        .map_err(|error| DownloadError::Io(error.to_string()))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut last_report = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            drop(handle);
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(DownloadError::Cancelled);
        }
        let chunk = chunk.map_err(|error| DownloadError::Http(error.to_string()))?;
        received = received.saturating_add(chunk.len() as u64);
        if received > file.bytes {
            drop(handle);
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(DownloadError::Integrity(format!(
                "{} exceeded its pinned size",
                file.name
            )));
        }
        hasher.update(&chunk);
        handle
            .write_all(&chunk)
            .await
            .map_err(|error| DownloadError::Io(error.to_string()))?;
        if last_report.elapsed() >= Duration::from_millis(200) {
            on_progress(received, file.bytes);
            last_report = Instant::now();
        }
    }
    handle
        .flush()
        .await
        .map_err(|error| DownloadError::Io(error.to_string()))?;
    drop(handle);
    if received != file.bytes {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(DownloadError::Integrity(format!(
            "{} downloaded {received} bytes, not the pinned {} bytes",
            file.name, file.bytes
        )));
    }
    let digest = hex(&hasher.finalize());
    if digest != file.sha256 {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(DownloadError::Integrity(format!(
            "{} has SHA-256 {digest}, not the published digest",
            file.name
        )));
    }
    on_progress(received, file.bytes);
    tokio::fs::rename(&partial, destination)
        .await
        .map_err(|error| DownloadError::Io(error.to_string()))?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// True only when the models folder already contains the pinned file.
#[must_use]
pub fn installed(models_dir: &Path, model: &CatalogModel) -> bool {
    model.files.iter().all(|file| {
        let path = file_path(models_dir, model, file);
        path.is_file()
            && std::fs::metadata(&path)
                .map(|metadata| metadata.len() == file.bytes)
                .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
    };

    use super::*;

    #[test]
    fn every_role_has_pinned_files_and_one_owner() {
        let catalog = catalog();
        assert_eq!(catalog.len(), 4);
        let keys: BTreeMap<_, _> = catalog
            .iter()
            .map(|model| (model.key, model.repo))
            .collect();
        assert!(keys.contains_key(&ModelKey::Text));
        assert!(keys.contains_key(&ModelKey::Vision));
        assert!(keys.contains_key(&ModelKey::Audio));
        assert!(keys.contains_key(&ModelKey::D1));
        for model in &catalog {
            assert!(!model.files.is_empty());
            for file in &model.files {
                assert!(file.name.ends_with(".gguf"));
                assert!(file.bytes > 0);
                assert_eq!(file.sha256.len(), 64);
            }
        }
        assert_eq!(
            catalog_model(ModelKey::Vision).expect("vision").files.len(),
            2,
            "vision needs its weight and projector"
        );
        assert_eq!(
            catalog_model(ModelKey::Audio).expect("audio").files.len(),
            2,
            "audio needs its weight and projector"
        );
    }

    #[test]
    fn every_file_url_is_pinned_to_the_approved_owner() {
        for model in catalog() {
            for file in &model.files {
                let url = model.file_url(file);
                assert!(
                    url.starts_with("https://huggingface.co/LiquidAI/"),
                    "unexpected source: {url}"
                );
                assert!(url.contains(&format!("/{}/resolve/main/", model.repo)));
                assert!(url.ends_with(&format!("{}?download=true", file.name)));
            }
        }
    }

    #[test]
    fn installed_requires_every_pinned_file_at_its_exact_size() {
        let directory = tempfile::tempdir().expect("temp dir");
        let model = catalog_model(ModelKey::Vision).expect("vision");
        let root = directory.path();
        assert!(!installed(root, &model));
        for file in &model.files {
            let path = file_path(root, &model, file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("folder");
            std::fs::write(&path, vec![0_u8; usize::try_from(file.bytes).unwrap()]).expect("write");
        }
        assert!(installed(root, &model));
        let first = file_path(root, &model, &model.files[0]);
        std::fs::write(&first, b"short").expect("truncate");
        assert!(!installed(root, &model));
    }

    #[test]
    fn only_one_download_runs_at_a_time() {
        let manager = DownloadManager::default();
        assert!(manager.begin(ModelKey::Text).is_ok());
        let error = manager.begin(ModelKey::Audio).expect_err("second refused");
        assert!(error.contains("already running"));
        assert_eq!(manager.active(), Some(ModelKey::Text));
        assert!(manager.cancel(ModelKey::Text));
        manager.finish(ModelKey::Text);
        assert_eq!(manager.active(), None);
    }

    #[test]
    fn cancellation_targets_only_the_active_role() {
        let manager = DownloadManager::default();
        assert!(!manager.cancel(ModelKey::Text), "nothing is running yet");

        let flag = manager.begin(ModelKey::Text).expect("claim download");
        assert!(
            !manager.cancel(ModelKey::Vision),
            "other roles are untouched"
        );
        assert!(!flag.load(Ordering::SeqCst));
        assert!(manager.cancel(ModelKey::Text));
        assert!(flag.load(Ordering::SeqCst));

        manager.finish(ModelKey::Text);
        assert_eq!(manager.active(), None);
        assert!(
            manager.begin(ModelKey::Vision).is_ok(),
            "the slot is free after cancellation and finish"
        );
    }

    #[test]
    fn destination_stays_inside_the_models_folder() {
        let root = Path::new("models");
        let model = catalog_model(ModelKey::Text).expect("text");
        let path = file_path(root, &model, &model.files[0]);
        assert_eq!(
            path,
            PathBuf::from("models/LFM3-1.7B-GGUF/LFM3-1.7B-Q4_K_M.gguf")
        );
    }

    #[tokio::test]
    async fn a_matching_file_is_verified_and_a_bad_digest_is_rejected() {
        let directory = tempfile::tempdir().expect("temp dir");
        let body = b"pinned model bytes";
        let digest = hex(&Sha256::digest(body));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    String::from_utf8_lossy(body),
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let url = format!("http://{address}/model.gguf");
        let good = CatalogFile {
            name: "model.gguf",
            bytes: body.len() as u64,
            sha256: Box::leak(digest.into_boxed_str()),
        };
        let destination = directory.path().join("model.gguf");
        let cancel = AtomicBool::new(false);
        fetch_file(
            &reqwest::Client::new(),
            &url,
            &destination,
            &good,
            &cancel,
            &|_, _| {},
        )
        .await
        .expect("verified download");
        assert_eq!(std::fs::read(&destination).expect("read"), body);

        let bad_digest = "0".repeat(64);
        let bad = CatalogFile {
            name: "model.gguf",
            bytes: body.len() as u64,
            sha256: Box::leak(bad_digest.into_boxed_str()),
        };
        let destination = directory.path().join("second.gguf");
        let error = fetch_file(
            &reqwest::Client::new(),
            &url,
            &destination,
            &bad,
            &cancel,
            &|_, _| {},
        )
        .await
        .expect_err("digest mismatch rejected");
        assert!(matches!(error, DownloadError::Integrity(_)));
        assert!(!destination.exists());
        assert!(!destination.with_file_name("second.gguf.part").exists());
        server.join().expect("server");
    }
}
