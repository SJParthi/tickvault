//! Panic safety tests for API handlers.
//!
//! Verifies that API handlers never panic with invalid, extreme, or missing inputs.
//! Critical for a trading system where an unhandled panic = process crash.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use tickvault_api::handlers::health::{HealthResponse, health_check};
use tickvault_api::handlers::stats::{StatsResponse, get_stats};
// PR #450 commit 6b (2026-05-03): top_movers handler DELETED — the
// /api/top-movers route is gone (replaced by unified /api/movers in
// commit 4) and the panic-safety tests for the legacy handler are
// removed below.
use tickvault_api::state::{SharedAppState, SystemHealthStatus};
use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_questdb_config_unreachable() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 1,
        pg_port: 1,
        ilp_port: 1,
    }
}

fn test_dhan_config() -> DhanConfig {
    DhanConfig {
        websocket_url: "wss://api-feed.dhan.co".to_string(),
        order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
        rest_api_base_url: "https://api.dhan.co/v2".to_string(),
        auth_base_url: "https://auth.dhan.co".to_string(),
        instrument_csv_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
            .to_string(),
        instrument_csv_fallback_url: "https://images.dhan.co/api-data/api-scrip-master.csv"
            .to_string(),
        max_instruments_per_connection: 5000,
        max_websocket_connections: 5,
        sandbox_base_url: String::new(),
    }
}

fn test_instrument_config() -> InstrumentConfig {
    InstrumentConfig {
        daily_download_time: "08:55:00".to_string(),
        csv_cache_directory: "/tmp/tv-cache".to_string(),
        csv_cache_filename: "instruments.csv".to_string(),
        csv_download_timeout_secs: 120,
        build_window_start: "08:25:00".to_string(),
        build_window_end: "08:55:00".to_string(),
    }
}

// PR #2 (2026-05-18): `empty_snapshot` retired alongside the deleted
// `SharedTopMoversSnapshot` type.

// PR #6a (2026-05-19): empty_constituency() helper retired.

fn make_test_state(questdb: QuestDbConfig) -> SharedAppState {
    SharedAppState::new(
        questdb,
        test_dhan_config(),
        test_instrument_config(),
        Arc::new(SystemHealthStatus::new()),
    )
}

// ---------------------------------------------------------------------------
// Must NOT panic: /health always returns 200
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_panic_health_check_returns_ok() {
    let state = make_test_state(test_questdb_config_unreachable());
    let Json(response) = health_check(State(state)).await;
    assert!(!response.version.is_empty());
    // Default health status has no token/WS → degraded
    assert_eq!(response.status, "degraded");
}

#[test]
fn no_panic_health_response_serialization() {
    use tickvault_api::handlers::health::{SubsystemInfo, SubsystemStatus};
    let resp = HealthResponse {
        status: "healthy",
        version: "0.1.0",
        git_sha: "unknown",
        subsystems: SubsystemStatus {
            websocket: SubsystemInfo {
                status: "connected",
                detail: Some("3 connections".to_string()),
            },
            order_update: SubsystemInfo {
                status: "connected",
                detail: None,
            },
            questdb: SubsystemInfo {
                status: "reachable",
                detail: None,
            },
            token: SubsystemInfo {
                status: "valid",
                detail: None,
            },
            pipeline: SubsystemInfo {
                status: "active",
                detail: None,
            },
            tick_persistence: SubsystemInfo {
                status: "connected",
                detail: None,
            },
        },
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("healthy"));
}

// PR #450 commit 6b (2026-05-03): /api/top-movers panic-safety tests
// DELETED. The legacy handler (TopMoversResponse / get_top_movers) is
// removed — superseded by the unified /api/movers handler in commit 4
// which has its own panic-safety surface in handlers/movers.rs tests.

// ---------------------------------------------------------------------------
// Must NOT panic: /api/stats with unreachable QuestDB
// ---------------------------------------------------------------------------

/// Reads the JSON body of a stats response (2026-07-09 hardening changed
/// `get_stats` to return a `Response` so the TTL cache can serve
/// pre-serialized bodies).
async fn stats_body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("stats body readable");
    serde_json::from_slice(&bytes).expect("stats body is JSON")
}

#[tokio::test]
async fn no_panic_stats_unreachable_questdb() {
    let state = make_test_state(test_questdb_config_unreachable());
    let result = stats_body_json(get_stats(State(state)).await).await;
    assert_eq!(result["questdb_reachable"], serde_json::json!(false));
    assert_eq!(result["tables"], serde_json::json!(0));
    assert_eq!(result["underlyings"], serde_json::json!(0));
    assert_eq!(result["derivatives"], serde_json::json!(0));
    assert_eq!(result["subscribed_indices"], serde_json::json!(0));
    assert_eq!(result["ticks"], serde_json::json!(0));
}

#[tokio::test]
async fn no_panic_stats_with_zero_port() {
    let config = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 0,
        pg_port: 0,
        ilp_port: 0,
    };
    let state = make_test_state(config);
    let result = stats_body_json(get_stats(State(state)).await).await;
    // Port 0 is invalid — should not panic, just fail gracefully
    assert_eq!(result["questdb_reachable"], serde_json::json!(false));
}

// ---------------------------------------------------------------------------
// Must NOT panic: StatsResponse serialization
// (TopMoversResponse serialization test deleted — handler removed
// in PR #450 commit 6b)
// ---------------------------------------------------------------------------

#[test]
fn no_panic_stats_response_serialization_extreme_values() {
    let stats = StatsResponse {
        questdb_reachable: true,
        tables: u64::MAX,
        underlyings: u64::MAX,
        derivatives: u64::MAX,
        subscribed_indices: u64::MAX,
        ticks: u64::MAX,
    };
    let json = serde_json::to_string(&stats).unwrap();
    assert!(!json.is_empty());
}

// ---------------------------------------------------------------------------
// Must NOT panic: SharedAppState construction and accessors
// ---------------------------------------------------------------------------

#[test]
fn no_panic_shared_app_state_construction() {
    let state = make_test_state(test_questdb_config_unreachable());
    // All accessors must not panic
    let _ = state.questdb_config();
    let _ = state.dhan_config();
    let _ = state.instrument_config();
    // PR #6a (2026-05-19): constituency_map() accessor retired.
}

#[test]
fn no_panic_shared_app_state_clone() {
    let state = make_test_state(test_questdb_config_unreachable());
    let cloned = state.clone();
    assert_eq!(cloned.questdb_config().host, "127.0.0.1");
}
