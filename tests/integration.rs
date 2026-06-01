//! Integration tests exercising the public crate API and HTTP routing.

use ghc_proxy::config;
use ghc_proxy::store::{RequestRecord, RequestStore};

#[test]
fn config_dir_is_named_ghc_tunnel() {
    let dir = config::config_dir();
    assert!(dir.to_string_lossy().contains("ghc-tunnel"));
}

#[test]
fn default_config_has_model_mappings_and_base_url() {
    let cfg = config::Config::default();
    assert!(!cfg.model_mappings.exact.is_empty());
    assert_eq!(cfg.copilot_base_url(), "https://api.githubcopilot.com");

    let biz = config::Config {
        account_type: "business".into(),
        ..config::Config::default()
    };
    assert_eq!(
        biz.copilot_base_url(),
        "https://api.business.githubcopilot.com"
    );
}

#[test]
fn default_config_yaml_parses_back() {
    let yaml = config::default_config_yaml();
    let cfg: config::Config = serde_norway::from_str(&yaml).expect("default yaml should parse");
    assert_eq!(cfg.port, config::DEFAULT_PORT);
    assert_eq!(cfg.address, config::DEFAULT_ADDRESS);
    assert_eq!(
        cfg.model_mappings.exact.get("opus").map(String::as_str),
        Some(config::DEFAULT_OPUS)
    );
}

#[test]
fn store_tracks_stats_and_recent() {
    let s = RequestStore::new(10);
    s.add(RequestRecord {
        id: "1".into(),
        timestamp: "t".into(),
        endpoint: "/v1/chat/completions".into(),
        model: "gpt-4o".into(),
        translated_model: None,
        status_code: 200,
        request_size: 100,
        response_size: 200,
        input_tokens: 5,
        output_tokens: 7,
        duration: 0.1,
    });
    let stats = s.stats();
    assert_eq!(stats.request_count, 1);
    assert_eq!(stats.total_input_tokens, 5);
    assert_eq!(stats.total_output_tokens, 7);
    assert_eq!(stats.bytes_received, 100);
    assert_eq!(stats.bytes_sent, 200);
    let (items, total) = s.recent(10, 0);
    assert_eq!(total, 1);
    assert_eq!(items.len(), 1);
}

#[tokio::test]
async fn router_builds_and_serves_dashboard() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let cfg = config::Config::default();
    let state = std::sync::Arc::new(ghc_proxy::state::AppState::new(cfg, "dummy-token".into()));
    let app = ghc_proxy::server::router(state);

    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
