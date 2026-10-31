mod support;

use std::time::Duration;

use lfm3_harness::{
    CancellationContext, ChatMessage, ChatRequest, LaunchSpec, LlamaClient, ModelError, ModelKey,
    ModelManager, ModelManagerConfig, ModelSpec, ProcessSupervisor,
};
use support::{text_reply, FakeConfig, FakeLlamaServer};
use url::Url;

const TEXT_MODEL: &str = "lfm3-1.7b";
const VISION_MODEL: &str = "lfm3-vl-2b";

fn url(raw: &str) -> Url {
    Url::parse(raw).expect("valid loopback url")
}

fn manager(specs: Vec<ModelSpec>, total_memory_bytes: u64, startup: Duration) -> ModelManager {
    ModelManager::new(
        ModelManagerConfig {
            total_memory_bytes,
            startup_timeout: startup,
            health_interval: Duration::from_millis(50),
        },
        specs,
        ProcessSupervisor::new().expect("job object"),
    )
    .expect("manager")
}

#[tokio::test]
async fn leases_reserve_memory_and_block_eviction() {
    let text = FakeLlamaServer::start(FakeConfig::chat(&[TEXT_MODEL], text_reply("ok"))).await;
    let vision = FakeLlamaServer::start(FakeConfig::chat(&[VISION_MODEL], text_reply("ok"))).await;
    let manager = manager(
        vec![
            ModelSpec::external(ModelKey::Text, url(text.url()), TEXT_MODEL, 600),
            ModelSpec::external(ModelKey::Vision, url(vision.url()), VISION_MODEL, 600),
        ],
        1000,
        Duration::from_secs(2),
    );
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let lease = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .expect("text lease");
    assert_eq!(manager.available_memory(), 400);

    let error = manager
        .acquire(ModelKey::Vision, &cancel.child())
        .await
        .err()
        .expect("a leased model cannot be evicted");
    match error {
        ModelError::MemoryExhausted {
            model,
            required,
            available,
        } => {
            assert_eq!(model, "vision");
            assert_eq!(required, 600);
            assert_eq!(available, 400);
        }
        other => panic!("expected MemoryExhausted, got {other:?}"),
    }
    assert_eq!(manager.active_models(), vec![ModelKey::Text]);

    drop(lease);
    let lease = manager
        .acquire(ModelKey::Vision, &cancel.child())
        .await
        .expect("released text model may be evicted");
    assert_eq!(manager.active_models(), vec![ModelKey::Vision]);
    assert_eq!(manager.available_memory(), 400);
    drop(lease);
}

#[tokio::test]
async fn repeated_acquire_shares_one_lease_count() {
    let server = FakeLlamaServer::start(FakeConfig::chat(&[TEXT_MODEL], text_reply("ok"))).await;
    let manager = manager(
        vec![ModelSpec::external(
            ModelKey::Text,
            url(server.url()),
            TEXT_MODEL,
            600,
        )],
        1000,
        Duration::from_secs(2),
    );
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let first = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .expect("first lease");
    let second = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .expect("second lease");
    assert_eq!(first.client().model(), TEXT_MODEL);
    assert_eq!(manager.available_memory(), 400);

    drop(first);
    assert_eq!(
        manager.available_memory(),
        400,
        "releasing one lease keeps the model reserved"
    );
    drop(second);
    assert_eq!(
        manager.available_memory(),
        400,
        "an unleased model stays loaded until something needs its memory"
    );
    assert_eq!(manager.active_models(), vec![ModelKey::Text]);
}

#[tokio::test]
async fn identity_mismatch_never_yields_a_lease() {
    let server =
        FakeLlamaServer::start(FakeConfig::chat(&["some-other-model"], text_reply("ok"))).await;
    let manager = manager(
        vec![ModelSpec::external(
            ModelKey::Text,
            url(server.url()),
            TEXT_MODEL,
            600,
        )],
        1000,
        Duration::from_secs(2),
    );
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let error = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .err()
        .expect("the identity does not match");
    assert!(
        matches!(error, ModelError::IdentityMismatch { ref expected, ref actual }
            if expected == TEXT_MODEL && actual == &vec!["some-other-model".to_owned()]),
        "{error:?}"
    );
    assert_eq!(manager.available_memory(), 1000);
    assert!(manager.active_models().is_empty());
}

#[tokio::test]
async fn inference_is_serialized_across_concurrent_callers() {
    let mut config = FakeConfig::chat(&[TEXT_MODEL], text_reply("slow"));
    config.chat_delay = Duration::from_millis(200);
    let server = FakeLlamaServer::start(config).await;
    let manager = manager(
        vec![ModelSpec::external(
            ModelKey::Text,
            url(server.url()),
            TEXT_MODEL,
            600,
        )],
        1000,
        Duration::from_secs(2),
    );
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let lease = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .expect("lease");

    let first_cancel = cancel.child();
    let second_cancel = cancel.child();
    let (first, second) = tokio::join!(
        lease.chat(
            ChatRequest::new(vec![ChatMessage::user("one")]),
            &first_cancel
        ),
        lease.chat(
            ChatRequest::new(vec![ChatMessage::user("two")]),
            &second_cancel
        ),
    );
    first.expect("first chat");
    second.expect("second chat");

    assert_eq!(
        server.max_concurrency(),
        1,
        "the manager must never issue two inferences at once"
    );
    assert_eq!(server.chat_requests().len(), 2);
}

#[tokio::test]
async fn shutdown_preserves_external_servers_and_refuses_new_leases() {
    let server = FakeLlamaServer::start(FakeConfig::chat(&[TEXT_MODEL], text_reply("ok"))).await;
    let manager = manager(
        vec![ModelSpec::external(
            ModelKey::Text,
            url(server.url()),
            TEXT_MODEL,
            600,
        )],
        1000,
        Duration::from_secs(2),
    );
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let lease = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .expect("lease");
    drop(lease);

    manager.shutdown().expect("shutdown");
    manager.shutdown().expect("shutdown is idempotent");
    assert!(manager.is_shutdown());

    let error = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .err()
        .expect("a shut down manager refuses leases");
    assert!(matches!(error, ModelError::Shutdown), "{error:?}");

    LlamaClient::from_endpoint(server.url(), TEXT_MODEL)
        .expect("external client")
        .verify_identity(&cancel.child())
        .await
        .expect("external servers survive manager shutdown");
}

#[cfg(windows)]
#[tokio::test]
async fn owned_server_that_never_becomes_ready_is_killed() {
    let directory = tempfile::tempdir().expect("temp dir");
    let pid_file = directory.path().join("server.pid");
    let script = format!(
        "$PID | Out-File -FilePath '{}' -Encoding ascii; Start-Sleep -Seconds 60",
        pid_file.display()
    );
    let launch = LaunchSpec::new(
        "powershell.exe",
        ["-NoProfile", "-Command", script.as_str()].map(str::to_owned),
    );
    let manager = manager(
        vec![ModelSpec::owned(
            ModelKey::Text,
            url("http://127.0.0.1:9"),
            TEXT_MODEL,
            600,
            launch,
        )],
        1000,
        Duration::from_secs(5),
    );
    let cancel = CancellationContext::new(Duration::from_secs(20));

    let error = manager
        .acquire(ModelKey::Text, &cancel.child())
        .await
        .err()
        .expect("a dead endpoint never becomes ready");
    assert!(matches!(error, ModelError::Startup(_)), "{error:?}");

    let pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("the owned server wrote its pid")
        .trim()
        .parse()
        .expect("numeric pid");
    assert!(
        !lfm3_harness::is_process_alive(pid),
        "owned server {pid} must be terminated after startup fails"
    );
    assert!(manager.active_models().is_empty());
    assert_eq!(manager.available_memory(), 1000);
}
