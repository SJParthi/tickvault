//! Panic safety tests for API handlers.
//!
//! Verifies that API handlers never panic with invalid, extreme, or missing inputs.
//! Critical for a trading system where an unhandled panic = process crash.

use std::sync::{Arc, RwLock};

use axum::Json;
use axum::extract::State;
use dhan_live_trader_api::handlers::health::{HealthResponse, health_check};
use dhan_live_trader_api::handlers::stats::{StatsResponse, get_stats};
use dhan_live_trader_api::handlers::top_movers::{TopMoversResponse, get_top_movers};
use dhan_live_trader_api::state::{SharedAppState, SystemHealthStatus};
use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

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
    }
}

fn test_instrument_config() -> InstrumentConfig {
    InstrumentConfig {
        daily_download_time: "08:55:00".to_string(),
        csv_cache_directory: "/tmp/dlt-cache".to_string(),
        csv_cache_filename: "instruments.csv".to_string(),
        csv_download_timeout_secs: 120,
        build_window_start: "08:25:00".to_string(),
        build_window_end: "08:55:00".to_string(),
    }
}

fn empty_snapshot() -> dhan_live_trader_core::pipeline::top_movers::SharedTopMoversSnapshot {
    Arc::new(RwLock::new(None))
}

fn empty_constituency() -> dhan_live_trader_api::state::SharedConstituencyMap {
    Arc::new(RwLock::new(None))
}

fn make_test_state(questdb: QuestDbConfig) -> SharedAppState {
    SharedAppState::new(
        questdb,
        test_dhan_config(),
        test_instrument_config(),
        empty_snapshot(),
        empty_constituency(),
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
    use dhan_live_trader_api::handlers::health::{SubsystemInfo, SubsystemStatus};
    let resp = HealthResponse {
        status: "healthy",
        version: "0.1.0",
        subsystems: SubsystemStatus {
            websocket: SubsystemInfo {
                status: "connected",
                detail: Some("3 connections".to_string()),
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
        },
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("healthy"));
}

// ---------------------------------------------------------------------------
// Must NOT panic: /api/top-movers with empty/no snapshot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_panic_top_movers_empty_snapshot() {
    let state = make_test_state(test_questdb_config_unreachable());
    let Json(result) = get_top_movers(State(state)).await;
    assert!(!result.available);
    assert!(result.gainers.is_empty());
    assert!(result.losers.is_empty());
    assert!(result.most_active.is_empty());
    assert_eq!(result.total_tracked, 0);
}

#[tokio::test]
async fn no_panic_top_movers_with_populated_snapshot() {
    use dhan_live_trader_core::pipeline::top_movers::{MoverEntry, TopMoversSnapshot};

    let snapshot = TopMoversSnapshot {
        gainers: vec![MoverEntry {
            security_id: u32::MAX,
            exchange_segment_code: 255,
            last_traded_price: f32::MAX,
            change_pct: f32::INFINITY,
            volume: u32::MAX,
        }],
        losers: vec![MoverEntry {
            security_id: 0,
            exchange_segment_code: 0,
            last_traded_price: 0.0,
            change_pct: f32::NEG_INFINITY,
            volume: 0,
        }],
        most_active: vec![MoverEntry {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: f32::NAN,
            change_pct: f32::NAN,
            volume: 1,
        }],
        total_tracked: usize::MAX,
    };

    let shared = Arc::new(RwLock::new(Some(snapshot)));
    let state = SharedAppState::new(
        test_questdb_config_unreachable(),
        test_dhan_config(),
        test_instrument_config(),
        shared,
        empty_constituency(),
        Arc::new(SystemHealthStatus::new()),
    );

    let Json(result) = get_top_movers(State(state)).await;
    assert!(result.available);
    assert_eq!(result.gainers.len(), 1);
    assert_eq!(result.losers.len(), 1);
    assert_eq!(result.most_active.len(), 1);
}

// ---------------------------------------------------------------------------
// Must NOT panic: /api/stats with unreachable QuestDB
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_panic_stats_unreachable_questdb() {
    let state = make_test_state(test_questdb_config_unreachable());
    let Json(result) = get_stats(State(state)).await;
    assert!(!result.questdb_reachable);
    assert_eq!(result.tables, 0);
    assert_eq!(result.underlyings, 0);
    assert_eq!(result.derivatives, 0);
    assert_eq!(result.subscribed_indices, 0);
    assert_eq!(result.ticks, 0);
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
    let Json(result) = get_stats(State(state)).await;
    // Port 0 is invalid — should not panic, just fail gracefully
    assert!(!result.questdb_reachable);
}

// ---------------------------------------------------------------------------
// Must NOT panic: StatsResponse and TopMoversResponse serialization
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

#[test]
fn no_panic_top_movers_response_serialization_empty() {
    let resp = TopMoversResponse {
        available: false,
        gainers: Vec::new(),
        losers: Vec::new(),
        most_active: Vec::new(),
        total_tracked: 0,
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("\"available\":false"));
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
    let _ = state.top_movers_snapshot();
    let _ = state.constituency_map();
}

#[test]
fn no_panic_shared_app_state_clone() {
    let state = make_test_state(test_questdb_config_unreachable());
    let cloned = state.clone();
    assert_eq!(cloned.questdb_config().host, "127.0.0.1");
}
