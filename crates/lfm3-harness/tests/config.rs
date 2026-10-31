use lfm3_harness::{is_loopback, Config, HarnessError, Profile};

#[test]
fn packaged_profile_rejects_remote_model_endpoints() {
    let config = Config {
        text_url: "https://example.com/v1".parse().unwrap(),
        ..Config::default()
    };
    let error = config
        .validate_local(Profile::Packaged)
        .expect_err("remote endpoint must be rejected");
    assert!(matches!(error, HarnessError::RemoteEndpointRejected(_)));
}

#[test]
fn packaged_profile_accepts_loopback_endpoints() {
    let config = Config::default();
    config
        .validate_local(Profile::Packaged)
        .expect("default endpoints are local");
    assert!(is_loopback(&config.text_url));
}

#[test]
fn packaged_profile_rejects_downloads() {
    let config = Config {
        allow_download: true,
        ..Config::default()
    };
    assert!(config.validate_local(Profile::Packaged).is_err());
}
