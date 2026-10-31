//! Memory-reserved model leases over one sequential inference gate.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use url::Url;

use super::{
    client::{LlamaClient, ModelError},
    process,
    types::{
        ChatRequest, ChatResponse, Decision, DecisionRequest, LaunchSpec, ModelKey,
        TranscribeRequest, Transcript,
    },
};
use crate::{is_loopback, CancellationChild, HarnessError, ProcessSupervisor};

#[derive(Clone, Copy, Debug)]
pub struct ModelManagerConfig {
    pub total_memory_bytes: u64,
    pub startup_timeout: Duration,
    pub health_interval: Duration,
}

impl Default for ModelManagerConfig {
    fn default() -> Self {
        Self {
            total_memory_bytes: 8 * 1024 * 1024 * 1024,
            startup_timeout: Duration::from_secs(30),
            health_interval: Duration::from_millis(150),
        }
    }
}

/// One locally available model and the fixed way the harness addresses it.
#[derive(Clone, Debug)]
pub struct ModelSpec {
    pub key: ModelKey,
    pub endpoint: Url,
    pub identity: String,
    pub memory_bytes: u64,
    /// `None` means an externally managed server that the harness never
    /// terminates.
    pub launch: Option<LaunchSpec>,
}

impl ModelSpec {
    #[must_use]
    pub fn external(
        key: ModelKey,
        endpoint: Url,
        identity: impl Into<String>,
        memory_bytes: u64,
    ) -> Self {
        Self {
            key,
            endpoint,
            identity: identity.into(),
            memory_bytes,
            launch: None,
        }
    }

    #[must_use]
    pub fn owned(
        key: ModelKey,
        endpoint: Url,
        identity: impl Into<String>,
        memory_bytes: u64,
        launch: LaunchSpec,
    ) -> Self {
        Self {
            key,
            endpoint,
            identity: identity.into(),
            memory_bytes,
            launch: Some(launch),
        }
    }

    #[must_use]
    pub const fn is_external(&self) -> bool {
        self.launch.is_none()
    }
}

/// Shared model scheduler. Cheap to clone; all clones share one state.
#[derive(Clone)]
pub struct ModelManager {
    inner: Arc<Inner>,
}

struct Inner {
    config: ModelManagerConfig,
    specs: BTreeMap<ModelKey, ModelSpec>,
    supervisor: ProcessSupervisor,
    state: Mutex<State>,
    loader: tokio::sync::Mutex<()>,
    inference: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    loaded: BTreeMap<ModelKey, Loaded>,
    leases: BTreeMap<ModelKey, usize>,
    reserved: u64,
    closed: bool,
}

struct Loaded {
    client: LlamaClient,
    memory_bytes: u64,
    /// Present only for harness-owned servers.
    child: Option<crate::OwnedChild>,
}

impl ModelManager {
    pub fn new(
        config: ModelManagerConfig,
        specs: Vec<ModelSpec>,
        supervisor: ProcessSupervisor,
    ) -> Result<Self, ModelError> {
        if config.total_memory_bytes == 0 {
            return Err(ModelError::Configuration(
                "the model memory budget must be positive".to_owned(),
            ));
        }
        let mut by_key = BTreeMap::new();
        for spec in specs {
            let key = spec.key;
            if spec.identity.trim().is_empty() {
                return Err(ModelError::Configuration(format!(
                    "model {} needs a server identity",
                    spec.key
                )));
            }
            if !is_loopback(&spec.endpoint) {
                return Err(ModelError::RemoteEndpoint(spec.endpoint.to_string()));
            }
            if spec.memory_bytes == 0 {
                return Err(ModelError::Configuration(format!(
                    "model {} must declare a positive memory reservation",
                    spec.key
                )));
            }
            if by_key.insert(key, spec).is_some() {
                return Err(ModelError::Configuration(format!(
                    "model {key} is registered twice"
                )));
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                specs: by_key,
                supervisor,
                state: Mutex::new(State::default()),
                loader: tokio::sync::Mutex::new(()),
                inference: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Loads (or reuses) the model and returns a lease. The lease keeps the
    /// reservation alive; eviction only targets unleased models.
    pub async fn acquire(
        &self,
        key: ModelKey,
        cancel: &CancellationChild,
    ) -> Result<ModelLease, ModelError> {
        let inner = Arc::clone(&self.inner);
        let _loader = cancel
            .run(inner.loader.lock())
            .await
            .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?;

        {
            let mut state = self.lock()?;
            if state.closed {
                return Err(ModelError::Shutdown);
            }
            if let Some(client) = state.loaded.get(&key).map(|loaded| loaded.client.clone()) {
                *state.leases.entry(key).or_insert(0) += 1;
                return Ok(ModelLease {
                    inner: Arc::clone(&self.inner),
                    key,
                    client,
                });
            }
        }

        let spec = self
            .inner
            .specs
            .get(&key)
            .cloned()
            .ok_or_else(|| ModelError::UnknownModel(key.to_string()))?;

        let mut child = match &spec.launch {
            Some(launch) => Some(
                process::spawn(&self.inner.supervisor, launch)
                    .map_err(|error| ModelError::Startup(error.to_string()))?,
            ),
            None => None,
        };
        let client = LlamaClient::new(spec.endpoint.clone(), spec.identity.clone())?;
        let ready = process::wait_until_ready(
            &client,
            cancel,
            self.inner.config.startup_timeout,
            self.inner.config.health_interval,
        )
        .await;
        if let Err(error) = ready {
            stop_child(child);
            return Err(error);
        }

        let mut evicted = Vec::new();
        {
            let mut state = self.lock()?;
            if state.closed {
                drop(state);
                stop_child(child);
                return Err(ModelError::Shutdown);
            }
            while state.reserved.saturating_add(spec.memory_bytes)
                > self.inner.config.total_memory_bytes
            {
                let victim = state.loaded.keys().copied().find(|candidate| {
                    *candidate != key && state.leases.get(candidate).copied().unwrap_or(0) == 0
                });
                let Some(victim) = victim else {
                    let available = self
                        .inner
                        .config
                        .total_memory_bytes
                        .saturating_sub(state.reserved);
                    drop(state);
                    stop_child(child);
                    return Err(ModelError::MemoryExhausted {
                        model: key.to_string(),
                        required: spec.memory_bytes,
                        available,
                    });
                };
                if let Some(loaded) = state.loaded.remove(&victim) {
                    state.reserved = state.reserved.saturating_sub(loaded.memory_bytes);
                    evicted.push(loaded);
                }
            }
            state.reserved = state.reserved.saturating_add(spec.memory_bytes);
            state.leases.insert(key, 1);
            state.loaded.insert(
                key,
                Loaded {
                    client: client.clone(),
                    memory_bytes: spec.memory_bytes,
                    child: child.take(),
                },
            );
        }
        for loaded in evicted {
            stop_child(loaded.child);
        }

        Ok(ModelLease {
            inner: Arc::clone(&self.inner),
            key,
            client,
        })
    }

    #[must_use]
    pub fn available_memory(&self) -> u64 {
        self.lock().map_or(0, |state| {
            self.inner
                .config
                .total_memory_bytes
                .saturating_sub(state.reserved)
        })
    }

    #[must_use]
    pub fn active_models(&self) -> Vec<ModelKey> {
        self.lock()
            .map(|state| state.loaded.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Every model this manager can start, whether loaded right now or not.
    #[must_use]
    pub fn available_models(&self) -> Vec<ModelKey> {
        self.inner.specs.keys().copied().collect()
    }

    #[must_use]
    pub fn has_model(&self, key: ModelKey) -> bool {
        self.inner.specs.contains_key(&key)
    }

    /// Releases one idle model, terminating a harness-owned server. A leased
    /// model is never evicted out from under an active run.
    pub fn evict(&self, key: ModelKey) -> Result<bool, ModelError> {
        let mut state = self.lock()?;
        if state.leases.get(&key).copied().unwrap_or(0) > 0 {
            return Err(ModelError::Leased(key.to_string()));
        }
        let Some(loaded) = state.loaded.remove(&key) else {
            return Ok(false);
        };
        state.reserved = state.reserved.saturating_sub(loaded.memory_bytes);
        drop(state);
        stop_child(loaded.child);
        Ok(true)
    }

    /// Terminates harness-owned servers. External servers are never touched.
    pub fn shutdown(&self) -> Result<(), HarnessError> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| HarnessError::Process("model manager state is poisoned".to_owned()))?;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        state.reserved = 0;
        let loaded = std::mem::take(&mut state.loaded);
        drop(state);
        for model in loaded.into_values() {
            stop_child(model.child);
        }
        Ok(())
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.lock().map(|state| state.closed).unwrap_or(true)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>, ModelError> {
        self.inner
            .state
            .lock()
            .map_err(|_| ModelError::Configuration("model manager state is poisoned".to_owned()))
    }
}

/// A held reservation. Dropping it releases the lease, not the model.
pub struct ModelLease {
    inner: Arc<Inner>,
    key: ModelKey,
    client: LlamaClient,
}

impl ModelLease {
    #[must_use]
    pub const fn key(&self) -> ModelKey {
        self.key
    }

    #[must_use]
    pub fn client(&self) -> &LlamaClient {
        &self.client
    }

    /// All inference across the manager is serialized through this gate.
    pub async fn chat(
        &self,
        request: ChatRequest,
        cancel: &CancellationChild,
    ) -> Result<ChatResponse, ModelError> {
        let _gate = cancel
            .run(self.inner.inference.lock())
            .await
            .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?;
        self.client.chat(request, cancel).await
    }

    /// Serialized streaming completion. The inference gate stays held until
    /// the stream ends, so two turns can never interleave on one server.
    pub async fn chat_stream<F>(
        &self,
        request: ChatRequest,
        cancel: &CancellationChild,
        on_delta: &mut F,
    ) -> Result<ChatResponse, ModelError>
    where
        F: FnMut(&str) + Send + ?Sized,
    {
        let _gate = cancel
            .run(self.inner.inference.lock())
            .await
            .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?;
        self.client.chat_stream(request, cancel, on_delta).await
    }

    pub async fn transcribe(
        &self,
        request: TranscribeRequest,
        cancel: &CancellationChild,
    ) -> Result<Transcript, ModelError> {
        let _gate = cancel
            .run(self.inner.inference.lock())
            .await
            .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?;
        self.client.transcribe(request, cancel).await
    }

    pub async fn decide(
        &self,
        request: DecisionRequest,
        cancel: &CancellationChild,
    ) -> Result<Decision, ModelError> {
        let _gate = cancel
            .run(self.inner.inference.lock())
            .await
            .map_err(|cancelled| ModelError::Cancelled(cancelled.reason))?;
        self.client.decide(request, cancel).await
    }
}

impl Drop for ModelLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.inner.state.lock() {
            if let Some(count) = state.leases.get_mut(&self.key) {
                *count = count.saturating_sub(1);
            }
        }
    }
}

fn stop_child(child: Option<crate::OwnedChild>) {
    if let Some(child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };

    use super::*;
    use crate::{CancellationContext, ProcessSupervisor};

    fn stub_server(identity: &'static str) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub model server");
        let address = listener.local_addr().expect("stub address");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request);
                let body = format!(
                    r#"{{"object":"list","data":[{{"id":"{identity}","object":"model"}}]}}"#
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}/v1").parse().expect("stub url")
    }

    #[tokio::test]
    async fn leased_models_cannot_be_evicted() {
        let endpoint = stub_server("stub-text");
        let manager = ModelManager::new(
            ModelManagerConfig {
                total_memory_bytes: 8 * 1024 * 1024,
                startup_timeout: Duration::from_secs(5),
                health_interval: Duration::from_millis(20),
            },
            vec![ModelSpec::external(
                ModelKey::Text,
                endpoint,
                "stub-text",
                1024,
            )],
            ProcessSupervisor::new().expect("supervisor"),
        )
        .expect("manager");

        let cancel = CancellationContext::new(Duration::from_secs(5));
        let lease = manager
            .acquire(ModelKey::Text, &cancel.child())
            .await
            .expect("acquire stub");
        assert!(matches!(
            manager.evict(ModelKey::Text),
            Err(ModelError::Leased(_))
        ));

        drop(lease);
        assert!(manager.evict(ModelKey::Text).expect("evict idle"));
        assert!(manager.active_models().is_empty());
    }
}
