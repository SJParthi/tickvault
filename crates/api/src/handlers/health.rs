//! Health check endpoint with subsystem status.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::SharedAppState;

/// Health check response with subsystem status.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub subsystems: SubsystemStatus,
}

/// Per-subsystem health status.
#[derive(Serialize)]
pub struct SubsystemStatus {
    pub websocket: SubsystemInfo,
    pub questdb: SubsystemInfo,
    pub token: SubsystemInfo,
    pub pipeline: SubsystemInfo,
    pub tick_persistence: SubsystemInfo,
}

/// Individual subsystem status info.
#[derive(Serialize)]
pub struct SubsystemInfo {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// GET /health — returns 200 OK with subsystem status.
pub async fn health_check(State(state): State<SharedAppState>) -> Json<HealthResponse> {
    let health = state.health_status();

    let ws_count = health.websocket_connections();
    let websocket = SubsystemInfo {
        status: if ws_count > 0 {
            "connected"
        } else {
            "disconnected"
        },
        detail: Some(format!("{ws_count} connections")),
    };

    let questdb = SubsystemInfo {
        status: if health.questdb_reachable() {
            "reachable"
        } else {
            "unreachable"
        },
        detail: None,
    };

    let token = SubsystemInfo {
        status: if health.token_valid() {
            "valid"
        } else {
            "invalid"
        },
        detail: None,
    };

    let pipeline = SubsystemInfo {
        status: if health.pipeline_active() {
            "active"
        } else {
            "inactive"
        },
        detail: None,
    };

    let tick_buf = health.tick_buffer_size();
    let tick_spill = health.ticks_spilled();
    let tick_persistence = SubsystemInfo {
        status: if health.tick_persistence_connected() {
            "connected"
        } else if tick_buf > 0 || tick_spill > 0 {
            "buffering"
        } else {
            "unavailable"
        },
        detail: if tick_buf > 0 || tick_spill > 0 {
            Some(format!("buffer: {tick_buf}, spilled: {tick_spill}"))
        } else {
            None
        },
    };

    let overall = health.overall_status();

    Json(HealthResponse {
        status: overall,
        version: env!("CARGO_PKG_VERSION"),
        subsystems: SubsystemStatus {
            websocket,
            questdb,
            token,
            pipeline,
            tick_persistence,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SharedHealthStatus, SystemHealthStatus};
    use std::sync::Arc;

    use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

    fn make_test_state(health: SharedHealthStatus) -> SharedAppState {
        SharedAppState::new(
            QuestDbConfig {
                host: "test".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
                sandbox_base_url: String::new(),
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp".to_string(),
                csv_cache_filename: "test.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            health,
        )
    }

    #[tokio::test]
    async fn test_health_check_returns_subsystem_status() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_websocket_connections(3);
        health.set_questdb_reachable(true);
        health.set_token_valid(true);
        health.set_pipeline_active(true);
        health.set_tick_persistence_connected(true);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.status, "healthy");
        assert!(!response.version.is_empty());
        assert_eq!(response.subsystems.websocket.status, "connected");
        assert_eq!(response.subsystems.questdb.status, "reachable");
        assert_eq!(response.subsystems.token.status, "valid");
        assert_eq!(response.subsystems.pipeline.status, "active");
        assert_eq!(response.subsystems.tick_persistence.status, "connected");
    }

    #[tokio::test]
    async fn test_health_check_degraded_when_ws_disconnected() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_questdb_reachable(true);
        health.set_token_valid(true);
        // websocket stays at 0

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.status, "degraded");
        assert_eq!(response.subsystems.websocket.status, "disconnected");
    }

    #[tokio::test]
    async fn test_health_check_healthy() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_websocket_connections(5);
        health.set_questdb_reachable(true);
        health.set_token_valid(true);
        health.set_pipeline_active(true);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.status, "healthy");
    }

    #[tokio::test]
    async fn test_health_check_degraded() {
        let health = Arc::new(SystemHealthStatus::new());
        // All subsystems down (default)

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.status, "degraded");
        assert_eq!(response.subsystems.websocket.status, "disconnected");
        assert_eq!(response.subsystems.questdb.status, "unreachable");
        assert_eq!(response.subsystems.token.status, "invalid");
        assert_eq!(response.subsystems.pipeline.status, "inactive");
        assert_eq!(response.subsystems.tick_persistence.status, "unavailable");
    }

    #[test]
    fn test_health_response_serialization() {
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
                tick_persistence: SubsystemInfo {
                    status: "connected",
                    detail: None,
                },
            },
        };
        let json = serde_json::to_string(&resp).expect("serialization should succeed");
        assert!(json.contains("\"status\":\"healthy\""));
        assert!(json.contains("\"version\":\"0.1.0\""));
        assert!(json.contains("\"websocket\""));
        assert!(json.contains("\"questdb\""));
        assert!(json.contains("\"tick_persistence\""));
    }

    // -------------------------------------------------------------------
    // SubsystemInfo: skip_serializing_if for detail: None
    // -------------------------------------------------------------------

    #[test]
    fn test_subsystem_info_detail_none_omitted_from_json() {
        let info = SubsystemInfo {
            status: "reachable",
            detail: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(
            !json.contains("detail"),
            "detail: None should be omitted via skip_serializing_if"
        );
    }

    #[test]
    fn test_subsystem_info_detail_some_included_in_json() {
        let info = SubsystemInfo {
            status: "connected",
            detail: Some("5 connections".to_string()),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"detail\":\"5 connections\""));
    }

    // -------------------------------------------------------------------
    // HealthResponse: version comes from CARGO_PKG_VERSION
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_version_matches_cargo_pkg_version() {
        let health = Arc::new(SystemHealthStatus::new());
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(
            response.version,
            env!("CARGO_PKG_VERSION"),
            "version must match CARGO_PKG_VERSION"
        );
    }

    // -------------------------------------------------------------------
    // HealthResponse: all subsystems down
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_all_down_subsystem_details() {
        let health = Arc::new(SystemHealthStatus::new());
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        // websocket detail should show "0 connections"
        assert_eq!(
            response.subsystems.websocket.detail,
            Some("0 connections".to_string())
        );
        // Other subsystems should have detail: None
        assert!(response.subsystems.questdb.detail.is_none());
        assert!(response.subsystems.token.detail.is_none());
        assert!(response.subsystems.pipeline.detail.is_none());
        assert!(response.subsystems.tick_persistence.detail.is_none());
    }

    // -------------------------------------------------------------------
    // HealthResponse: websocket connection count in detail field
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_websocket_detail_shows_count() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_websocket_connections(5);
        health.set_token_valid(true);
        health.set_questdb_reachable(true);
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(
            response.subsystems.websocket.detail,
            Some("5 connections".to_string())
        );
    }

    // -------------------------------------------------------------------
    // HealthResponse: tick_persistence status
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_tick_persistence_connected() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_tick_persistence_connected(true);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.tick_persistence.status, "connected");
    }

    #[tokio::test]
    async fn test_health_check_tick_persistence_unavailable_by_default() {
        let health = Arc::new(SystemHealthStatus::new());

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.tick_persistence.status, "unavailable");
    }
}
