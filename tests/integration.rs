//! Integration tests exercising the public crate API and HTTP routing.

use ghc_proxy::config;
use ghc_proxy::store::{RequestRecord, RequestStore};

#[test]
fn config_dir_is_named_ghc_tunnel() {
    let dir = config::config_dir();
    assert!(dir.to_string_lossy().contains("ghc-tunnel"));
}

#[test]
fn rendered_config_round_trips_with_overrides() {
    let cfg = config::Config {
        address: "0.0.0.0".into(),
        port: 9000,
        debug: true,
        account_type: "business".into(),
        system_prompt_add: vec!["be concise".into()],
        redirect_anthropic: true,
        ..config::Config::default()
    };
    let yaml = config::render_config_yaml(&cfg);
    let parsed: config::Config =
        serde_norway::from_str(&yaml).expect("rendered yaml should parse back");
    assert_eq!(parsed.port, 9000);
    assert_eq!(parsed.address, "0.0.0.0");
    assert!(parsed.debug);
    assert_eq!(parsed.account_type, "business");
    assert_eq!(parsed.system_prompt_add, vec!["be concise".to_string()]);
    assert!(parsed.redirect_anthropic);
    // Mappings with bracketed keys must survive the round trip.
    assert_eq!(
        parsed
            .model_mappings
            .exact
            .get("4-7[1m]")
            .map(String::as_str),
        Some(config::DEFAULT_OPUS)
    );
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

#[tokio::test]
async fn copilot_headers_mimic_latest_client() {
    let cfg = config::Config::default();
    let state = ghc_proxy::state::AppState::new(cfg, "dummy-token".into());

    let h = state.copilot_headers(false).await;

    // Identity headers the upstream Copilot API expects from the VS Code client.
    assert_eq!(h.get("Copilot-Integration-Id").unwrap(), "vscode-chat");
    assert_eq!(
        h.get("Editor-Version").unwrap(),
        &format!("vscode/{}", config::VSCODE_VERSION)
    );
    assert_eq!(
        h.get("Editor-Plugin-Version").unwrap(),
        &format!("copilot-chat/{}", config::COPILOT_VERSION)
    );
    assert_eq!(
        h.get("User-Agent").unwrap(),
        &format!("GitHubCopilotChat/{}", config::COPILOT_VERSION)
    );
    assert_eq!(h.get("X-GitHub-Api-Version").unwrap(), config::API_VERSION);
    assert_eq!(h.get("OpenAI-Intent").unwrap(), "conversation-panel");
    assert_eq!(h.get("X-Interaction-Type").unwrap(), "conversation-panel");
    assert_eq!(
        h.get("X-VSCode-User-Agent-Library-Version").unwrap(),
        "electron-fetch"
    );

    // X-Request-Id and X-Agent-Task-Id must be present and share the same value.
    let request_id = h.get("X-Request-Id").unwrap();
    assert_eq!(h.get("X-Agent-Task-Id").unwrap(), request_id);
    assert!(!request_id.is_empty());

    // Disguise headers that the genuine Copilot client sends.
    assert_eq!(h.get("openai-organization").unwrap(), "github-copilot");
    let machine_id = h.get("vscode-machineid").unwrap().to_str().unwrap();
    assert_eq!(machine_id.len(), 64);
    assert!(machine_id.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!h.get("vscode-sessionid").unwrap().is_empty());

    // Vision header is only present when requested.
    assert!(h.get("Copilot-Vision-Request").is_none());
    let hv = state.copilot_headers(true).await;
    assert_eq!(hv.get("Copilot-Vision-Request").unwrap(), "true");
}

#[test]
fn usage_summary_extracts_quota_snapshots() {
    use serde_json::json;
    let raw = json!({
        "copilot_plan": "individual",
        "quota_reset_date": "2026-07-01",
        "quota_snapshots": {
            "chat": {
                "entitlement": 0,
                "remaining": 0,
                "percent_remaining": 100.0,
                "unlimited": true
            },
            "premium_interactions": {
                "entitlement": 300,
                "remaining": 270,
                "percent_remaining": 90.0,
                "unlimited": false
            }
        }
    });
    let summary = ghc_proxy::state::summarize_usage(&raw);
    assert_eq!(summary["plan"], "individual");
    assert_eq!(summary["quota_reset_date"], "2026-07-01");
    assert_eq!(summary["quotas"]["chat"]["unlimited"], true);
    assert_eq!(
        summary["quotas"]["premium_interactions"]["remaining"],
        270.0
    );
    assert_eq!(
        summary["quotas"]["premium_interactions"]["percent_remaining"],
        90.0
    );
    // The original payload is preserved verbatim under `raw`.
    assert_eq!(summary["raw"], raw);
}
