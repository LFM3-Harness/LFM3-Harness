//! End-to-end routing against a native decision endpoint.

mod support;

use std::time::Duration;

use lfm3_harness::{
    chat::{route_request, ChatIntent, RouteSource},
    CancellationContext, ModelKey, ModelManager, ModelManagerConfig, ModelSpec, ProcessSupervisor,
};
use serde_json::json;
use support::{FakeConfig, FakeLlamaServer};
use url::Url;

const D1_MODEL: &str = "d1-2b";

fn manager(url: &str) -> ModelManager {
    ModelManager::new(
        ModelManagerConfig {
            total_memory_bytes: 2_000,
            startup_timeout: Duration::from_secs(5),
            health_interval: Duration::from_millis(20),
        },
        vec![ModelSpec::external(
            ModelKey::D1,
            Url::parse(url).expect("loopback url"),
            D1_MODEL,
            1_000,
        )],
        ProcessSupervisor::new().expect("job object"),
    )
    .expect("manager")
}

fn decision_reply(label: &str, confidence: f64) -> String {
    json!({
        "model": D1_MODEL,
        "answers": {
            "route": {
                "type": "choice",
                "choice": label,
                "probabilities": {label: confidence, "general": 1.0 - confidence},
                "confidence": confidence
            }
        },
        "usage": {"input_tokens": 40, "output_tokens": 0}
    })
    .to_string()
}

#[tokio::test]
async fn a_confident_decision_replaces_a_wrong_keyword_route() {
    let server = FakeLlamaServer::start(FakeConfig::decision(
        &[D1_MODEL],
        decision_reply("summarize", 0.97),
    ))
    .await;
    let manager = manager(server.url());
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let prompt = "Could you take a look at notes/coffee.md and tell me how often the water filter is changed?";
    assert_eq!(
        lfm3_harness::chat::ChatRoute::heuristic(prompt).intent,
        ChatIntent::Vision,
        "the keyword route is expected to be wrong for this fixture"
    );

    let route = route_request(&manager, prompt, &cancel.child()).await;

    assert_eq!(route.intent, ChatIntent::Summarize);
    assert_eq!(route.source, RouteSource::Decision);
    assert_eq!(route.confidence_bps, 9_700);
    assert_eq!(route.model.as_deref(), Some(D1_MODEL));
    assert!(route.reason.contains("97%"), "{}", route.reason);
    assert!(route
        .roles
        .iter()
        .any(|role| role.role == "route" && role.used));

    let requests = server.requests();
    let decision = requests
        .iter()
        .find(|request| request.path.contains("/v1/systemone"))
        .expect("the decision endpoint was called");
    let body: serde_json::Value = serde_json::from_str(&decision.body).expect("json body");
    assert_eq!(body["state"], prompt);
    let criteria = body["questions"]["route"]["criteria"]
        .as_object()
        .expect("criteria object");
    assert_eq!(criteria.len(), 7, "every route label is declared");
    assert!(criteria.contains_key("summarize") && criteria.contains_key("vision"));
    assert!(
        !requests
            .iter()
            .any(|request| request.path.contains("/v1/chat/completions")),
        "routing must use the native decision protocol"
    );
}

#[tokio::test]
async fn general_keyword_prompts_still_consult_the_decision_model() {
    let server = FakeLlamaServer::start(FakeConfig::decision(
        &[D1_MODEL],
        decision_reply("summarize", 0.84),
    ))
    .await;
    let manager = manager(server.url());
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let prompt = "I need a short brief of the incident note before standup.";
    assert_eq!(
        lfm3_harness::chat::ChatRoute::heuristic(prompt).intent,
        ChatIntent::General,
        "no keyword group fires for this fixture"
    );

    let route = route_request(&manager, prompt, &cancel.child()).await;

    assert_eq!(route.intent, ChatIntent::Summarize);
    assert_eq!(route.source, RouteSource::Decision);
    assert!(server
        .requests()
        .iter()
        .any(|request| request.path.contains("/v1/systemone")));
}

#[tokio::test]
async fn a_low_confidence_decision_keeps_the_keyword_route() {
    let server = FakeLlamaServer::start(FakeConfig::decision(
        &[D1_MODEL],
        decision_reply("report", 0.40),
    ))
    .await;
    let manager = manager(server.url());
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let prompt = "What does the refund policy say about shipping fees?";

    let route = route_request(&manager, prompt, &cancel.child()).await;

    assert_eq!(route.intent, ChatIntent::Summarize);
    assert_eq!(route.source, RouteSource::Heuristic);
    assert_eq!(route.confidence_bps, 4_000);
    assert_eq!(route.model.as_deref(), Some(D1_MODEL));
    assert!(route.reason.contains("floor"), "{}", route.reason);
}

#[tokio::test]
async fn a_confident_general_answer_keeps_conversation() {
    let server = FakeLlamaServer::start(FakeConfig::decision(
        &[D1_MODEL],
        decision_reply("general", 0.95),
    ))
    .await;
    let manager = manager(server.url());
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let route = route_request(
        &manager,
        "Hello there, how are you doing today?",
        &cancel.child(),
    )
    .await;

    assert_eq!(route.intent, ChatIntent::General);
    assert_eq!(route.source, RouteSource::Heuristic);
    assert_eq!(route.confidence_bps, 9_500);
    assert_eq!(route.model.as_deref(), Some(D1_MODEL));
    assert!(
        route.reason.contains("conversation"),
        "the reason must explain the General veto, got {:?}",
        route.reason
    );
}

#[tokio::test]
async fn short_greetings_skip_the_decision_model() {
    let server = FakeLlamaServer::start(FakeConfig::decision(
        &[D1_MODEL],
        decision_reply("summarize", 0.99),
    ))
    .await;
    let manager = manager(server.url());
    let cancel = CancellationContext::new(Duration::from_secs(10));

    let route = route_request(&manager, "hello there", &cancel.child()).await;

    assert_eq!(route.intent, ChatIntent::General);
    assert_eq!(route.source, RouteSource::Heuristic);
    assert!(
        !server
            .requests()
            .iter()
            .any(|request| request.path.contains("/v1/systemone")),
        "a tiny greeting must not spend a decision call"
    );
}

#[tokio::test]
async fn an_unsupported_decision_route_falls_back_to_keywords() {
    let server =
        FakeLlamaServer::start(FakeConfig::chat(&[D1_MODEL], support::text_reply("hi"))).await;
    let manager = manager(server.url());
    let cancel = CancellationContext::new(Duration::from_secs(10));
    let prompt = "Find the file that mentions the water filter.";

    let route = route_request(&manager, prompt, &cancel.child()).await;

    assert_eq!(route.intent, ChatIntent::Explore);
    assert_eq!(route.source, RouteSource::Heuristic);
    assert_eq!(route.confidence_bps, 0);
}
