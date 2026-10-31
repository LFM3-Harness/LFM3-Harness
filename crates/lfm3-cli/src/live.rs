//! Real local text-model access for headless acceptance runs.
//!
//! The harness either owns a pinned llama-server process and terminates it on
//! exit, or reuses an explicitly external loopback endpoint. Weights are never
//! downloaded; a missing binary or GGUF is a clear setup error.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use lfm3_harness::{
    model::{
        ChatRequest, ChatResponse, Decision, DecisionRequest, LaunchSpec, ModelFuture,
        TranscribeRequest, Transcript,
    },
    CancellationChild, CancellationContext, Config, LlamaClient, ModelGateway, ModelKey,
    ModelLease, ModelManager, ModelManagerConfig, ModelSpec, ProcessSupervisor,
};
use serde_json::json;

use crate::doctor;

const MODEL_MEMORY_HEADROOM: u64 = 512 * 1024 * 1024;
const MANAGER_MEMORY_HEADROOM: u64 = 2 * 1024 * 1024 * 1024;

/// A connected text model plus ownership of any harness-owned server.
pub struct LiveModel {
    gateway: Arc<dyn ModelGateway>,
    manager: Option<ModelManager>,
    pub identity: String,
    pub owned: bool,
    pub launch: Option<LaunchSpec>,
}

impl LiveModel {
    pub fn gateway(&self) -> Arc<dyn ModelGateway> {
        Arc::clone(&self.gateway)
    }

    /// The shared model manager, when this process owns the servers. Tools
    /// such as `look` and `transcribe` acquire their own leases through it.
    #[must_use]
    pub fn manager(&self) -> Option<&ModelManager> {
        self.manager.as_ref()
    }

    /// Terminates an owned server. External endpoints are never touched.
    pub fn shutdown(&self) -> Result<(), String> {
        match &self.manager {
            Some(manager) => manager.shutdown().map_err(|error| error.to_string()),
            None => Ok(()),
        }
    }
}

pub async fn connect(
    config: &Config,
    external: bool,
    startup_timeout: Duration,
) -> Result<LiveModel, String> {
    if external {
        let client = LlamaClient::new(config.text_url.clone(), config.text_model.clone())
            .map_err(|error| error.to_string())?;
        let cancel = CancellationContext::new(startup_timeout);
        client
            .verify_identity(&cancel.child())
            .await
            .map_err(|error| format!("external text endpoint is not ready: {error}"))?;
        return Ok(LiveModel {
            gateway: Arc::new(client),
            manager: None,
            identity: config.text_model.clone(),
            owned: false,
            launch: None,
        });
    }

    let server = config
        .llama
        .server
        .clone()
        .or_else(doctor::default_server_path)
        .ok_or_else(|| {
            format!(
                "no llama-server binary found; set LFM3_LLAMA_SERVER to the pinned b{} build",
                doctor::PINNED_BUILD
            )
        })?;
    let runtime = doctor::probe_runtime(config);
    if runtime.get("pinned").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!(
            "{} is not the pinned llama.cpp b{} / v{} build",
            server.display(),
            doctor::PINNED_BUILD,
            doctor::PINNED_RELEASE
        ));
    }

    let (weights_root, weights) = doctor::scan_weights();
    let gguf = doctor::find_weight(&weights_root, &weights, "LFM3-1.7B")
        .ok_or_else(|| format!("no LFM3-1.7B GGUF under {}", weights_root.display()))?;
    let file_bytes = std::fs::metadata(&gguf)
        .map_err(|error| format!("stat {}: {error}", gguf.display()))?
        .len();
    let memory_bytes = file_bytes.saturating_add(MODEL_MEMORY_HEADROOM);

    let launch = LaunchSpec::new(server.clone(), launch_args(&gguf, config));
    let spec = ModelSpec::owned(
        ModelKey::Text,
        config.text_url.clone(),
        config.text_model.clone(),
        memory_bytes,
        launch.clone(),
    );
    // A task may route to vision, audio, or the calibrated classifier, so the
    // manager registers every locally provisioned model. Missing weights are
    // simply absent; nothing is downloaded.
    let (extra, extra_memory) = extra_specs(config, &server, &weights_root, &weights);
    let supervisor = ProcessSupervisor::new().map_err(|error| error.to_string())?;
    let mut specs = vec![spec];
    specs.extend(extra);
    let manager = ModelManager::new(
        ModelManagerConfig {
            total_memory_bytes: memory_bytes
                .saturating_add(extra_memory)
                .saturating_add(MANAGER_MEMORY_HEADROOM),
            startup_timeout,
            health_interval: Duration::from_millis(250),
        },
        specs,
        supervisor,
    )
    .map_err(|error| error.to_string())?;
    let cancel = CancellationContext::new(startup_timeout);
    let lease = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .map_err(|error| format!("start {}: {error}", server.display()))?;
    Ok(LiveModel {
        gateway: Arc::new(LeaseGateway { lease }),
        manager: Some(manager),
        identity: config.text_model.clone(),
        owned: true,
        launch: Some(launch),
    })
}

fn launch_args(gguf: &Path, config: &Config) -> Vec<String> {
    let host = config.text_url.host_str().unwrap_or("127.0.0.1");
    let port = config.text_url.port_or_known_default().unwrap_or(8080);
    let mut args = base_launch_args(host, port, gguf, 4096);
    args.push("--alias".to_owned());
    args.push(config.text_model.clone());
    args
}

fn base_launch_args(host: &str, port: u16, weight: &Path, context: u32) -> Vec<String> {
    vec![
        "-m".to_owned(),
        weight.display().to_string(),
        "--host".to_owned(),
        host.to_owned(),
        "--port".to_owned(),
        port.to_string(),
        "--jinja".to_owned(),
        "-c".to_owned(),
        context.to_string(),
        "-ngl".to_owned(),
        "0".to_owned(),
    ]
}

/// Builds launch specs for the sense and judgement models that are present.
fn extra_specs(
    config: &Config,
    server: &Path,
    weights_root: &Path,
    weights: &[serde_json::Value],
) -> (Vec<ModelSpec>, u64) {
    let mut specs = Vec::new();
    let mut total = 0_u64;
    for (key, token, url, model, projector, batch) in [
        (
            ModelKey::Vision,
            "LFM3-VL-2B",
            &config.vision_url,
            config.vision_model.as_str(),
            true,
            false,
        ),
        (
            ModelKey::Audio,
            "LFM3-Audio-2B",
            &config.audio_url,
            config.audio_model.as_str(),
            true,
            false,
        ),
        (
            ModelKey::D1,
            "d1-2B",
            &config.d1_url,
            config.d1_model.as_str(),
            false,
            true,
        ),
    ] {
        let Some(weight) = doctor::find_weight(weights_root, weights, token) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&weight) else {
            continue;
        };
        let host = url.host_str().unwrap_or("127.0.0.1");
        let port = url.port_or_known_default().unwrap_or(8080);
        let mut args = base_launch_args(host, port, &weight, 8192);
        if projector {
            let Some(projector) = projector_beside(weights_root, weights, token) else {
                continue;
            };
            args.push("--mmproj".to_owned());
            args.push(projector.display().to_string());
        }
        if batch {
            args.push("-b".to_owned());
            args.push("8192".to_owned());
            args.push("-ub".to_owned());
            args.push("8192".to_owned());
        }
        args.push("--alias".to_owned());
        args.push(model.to_owned());
        let memory = metadata.len().saturating_add(MODEL_MEMORY_HEADROOM);
        specs.push(ModelSpec::owned(
            key,
            url.clone(),
            model,
            memory,
            LaunchSpec::new(server.to_path_buf(), args),
        ));
        total = total.saturating_add(memory);
    }
    (specs, total)
}

fn projector_beside(root: &Path, weights: &[serde_json::Value], token: &str) -> Option<PathBuf> {
    for weight in weights {
        let Some(path) = weight.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let lower = path.to_ascii_lowercase();
        if lower.contains(&token.to_ascii_lowercase()) && lower.contains("mmproj") {
            return Some(root.join(path));
        }
    }
    None
}

/// Serialized inference through a held model lease.
struct LeaseGateway {
    lease: ModelLease,
}

impl ModelGateway for LeaseGateway {
    fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, ChatResponse> {
        Box::pin(async move { self.lease.chat(request, &cancel).await })
    }

    fn transcribe(
        &self,
        request: TranscribeRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, Transcript> {
        Box::pin(async move { self.lease.transcribe(request, &cancel).await })
    }

    fn decide(
        &self,
        request: DecisionRequest,
        cancel: CancellationChild,
    ) -> ModelFuture<'_, Decision> {
        Box::pin(async move { self.lease.decide(request, &cancel).await })
    }
}

/// A bounded description of a run's model for reports.
pub fn describe(model: &LiveModel) -> serde_json::Value {
    json!({
        "identity": model.identity,
        "owned": model.owned,
        "launch": model.launch.as_ref().map(|launch| json!({
            "executable": launch.executable.display().to_string(),
            "args": launch.args,
        })),
    })
}

/// Reports the resolved text weight used for a run.
pub fn text_weight() -> Option<PathBuf> {
    let (root, weights) = doctor::scan_weights();
    doctor::find_weight(&root, &weights, "LFM3-1.7B")
}
