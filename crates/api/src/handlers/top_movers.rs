//! Top movers API endpoint — returns current top gainers, losers, and most active.
//!
//! Reads the latest snapshot from the shared handle published by the tick processor.
//! Cold path — no allocation on the tick processing hot path.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::SharedAppState;
use dhan_live_trader_core::pipeline::top_movers::{MoverEntry, TopMoversSnapshot};

/// Top movers API response.
#[derive(Debug, Serialize)]
pub struct TopMoversResponse {
    /// Whether a snapshot is available (false before first computation).
    pub available: bool,
    /// Top gainers sorted by change_pct descending.
    pub gainers: Vec<MoverEntry>,
    /// Top losers sorted by change_pct ascending.
    pub losers: Vec<MoverEntry>,
    /// Most active by volume descending.
    pub most_active: Vec<MoverEntry>,
    /// Total securities being tracked.
    pub total_tracked: usize,
}

impl From<&TopMoversSnapshot> for TopMoversResponse {
    fn from(snapshot: &TopMoversSnapshot) -> Self {
        Self {
            available: true,
            gainers: snapshot.gainers.clone(),
            losers: snapshot.losers.clone(),
            most_active: snapshot.most_active.clone(),
            total_tracked: snapshot.total_tracked,
        }
    }
}

/// `GET /api/top-movers` — returns the latest top movers snapshot.
pub async fn get_top_movers(State(state): State<SharedAppState>) -> Json<TopMoversResponse> {
    let handle = state.top_movers_snapshot();

    let response = match handle.read() {
        Ok(guard) => match guard.as_ref() {
            Some(snapshot) => TopMoversResponse::from(snapshot),
            None => TopMoversResponse {
                available: false,
                gainers: Vec::new(),
                losers: Vec::new(),
                most_active: Vec::new(),
                total_tracked: 0,
            },
        },
        Err(_) => TopMoversResponse {
            available: false,
            gainers: Vec::new(),
            losers: Vec::new(),
            most_active: Vec::new(),
            total_tracked: 0,
        },
    };

    Json(response)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_response_serialization() {
        let resp = TopMoversResponse {
            available: false,
            gainers: Vec::new(),
            losers: Vec::new(),
            most_active: Vec::new(),
            total_tracked: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"available\":false"));
        assert!(json.contains("\"total_tracked\":0"));
    }

    #[test]
    fn response_with_entries_serialization() {
        let entry = MoverEntry {
            security_id: 42,
            exchange_segment_code: 2,
            last_traded_price: 100.5,
            change_pct: 5.0,
            volume: 10000,
        };
        let resp = TopMoversResponse {
            available: true,
            gainers: vec![entry],
            losers: Vec::new(),
            most_active: vec![entry],
            total_tracked: 100,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"available\":true"));
        assert!(json.contains("\"security_id\":42"));
        assert!(json.contains("\"total_tracked\":100"));
    }

    #[tokio::test]
    async fn test_get_top_movers_empty_snapshot() {
        use crate::state::SharedAppState;
        use axum::extract::State;

        let snapshot = std::sync::Arc::new(std::sync::RwLock::new(None));
        let state = SharedAppState::new(
            dhan_live_trader_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 1,
                ilp_port: 1,
            },
            dhan_live_trader_common::config::DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            dhan_live_trader_common::config::InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            snapshot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        );
        let Json(result) = get_top_movers(State(state)).await;
        assert!(!result.available);
        assert!(result.gainers.is_empty());
        assert!(result.losers.is_empty());
        assert!(result.most_active.is_empty());
        assert_eq!(result.total_tracked, 0);
    }

    #[test]
    fn from_snapshot_conversion() {
        let snapshot = TopMoversSnapshot {
            gainers: vec![MoverEntry {
                security_id: 1,
                exchange_segment_code: 2,
                last_traded_price: 110.0,
                change_pct: 10.0,
                volume: 5000,
            }],
            losers: vec![],
            most_active: vec![],
            total_tracked: 50,
        };
        let resp = TopMoversResponse::from(&snapshot);
        assert!(resp.available);
        assert_eq!(resp.gainers.len(), 1);
        assert_eq!(resp.total_tracked, 50);
    }

    // -------------------------------------------------------------------
    // Poisoned RwLock: handler must not panic, returns unavailable
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_top_movers_poisoned_rwlock_returns_unavailable() {
        use std::sync::{Arc, RwLock};

        let snapshot: dhan_live_trader_core::pipeline::top_movers::SharedTopMoversSnapshot =
            Arc::new(RwLock::new(None));

        // Poison the lock by panicking inside a write guard
        let snapshot_clone = snapshot.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = snapshot_clone.write().unwrap();
            panic!("intentional poison");
        }));
        assert!(result.is_err(), "should have panicked");
        assert!(snapshot.read().is_err(), "lock should be poisoned");

        let state = crate::state::SharedAppState::new(
            dhan_live_trader_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 1,
                ilp_port: 1,
            },
            dhan_live_trader_common::config::DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            dhan_live_trader_common::config::InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            snapshot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        );

        let Json(result) = get_top_movers(State(state)).await;
        assert!(!result.available);
        assert!(result.gainers.is_empty());
        assert_eq!(result.total_tracked, 0);
    }

    // -------------------------------------------------------------------
    // Debug impl coverage
    // -------------------------------------------------------------------

    #[test]
    fn test_top_movers_response_debug_impl() {
        let resp = TopMoversResponse {
            available: true,
            gainers: vec![],
            losers: vec![],
            most_active: vec![],
            total_tracked: 42,
        };
        let debug = format!("{resp:?}");
        assert!(debug.contains("TopMoversResponse"));
        assert!(debug.contains("42"));
    }

    // -------------------------------------------------------------------
    // Handler with populated snapshot — exercises Ok(Some(snapshot)) path
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_top_movers_with_populated_snapshot() {
        use std::sync::{Arc, RwLock};

        let snapshot_data = TopMoversSnapshot {
            gainers: vec![
                MoverEntry {
                    security_id: 100,
                    exchange_segment_code: 1,
                    last_traded_price: 250.0_f32,
                    change_pct: 8.5_f32,
                    volume: 100_000,
                },
                MoverEntry {
                    security_id: 200,
                    exchange_segment_code: 2,
                    last_traded_price: 500.0_f32,
                    change_pct: 5.2_f32,
                    volume: 50_000,
                },
            ],
            losers: vec![MoverEntry {
                security_id: 300,
                exchange_segment_code: 1,
                last_traded_price: 100.0_f32,
                change_pct: -3.1_f32,
                volume: 75_000,
            }],
            most_active: vec![MoverEntry {
                security_id: 400,
                exchange_segment_code: 2,
                last_traded_price: 1000.0_f32,
                change_pct: 1.0_f32,
                volume: 500_000,
            }],
            total_tracked: 200,
        };

        let snapshot: dhan_live_trader_core::pipeline::top_movers::SharedTopMoversSnapshot =
            Arc::new(RwLock::new(Some(snapshot_data)));

        let state = crate::state::SharedAppState::new(
            dhan_live_trader_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 1,
                ilp_port: 1,
            },
            dhan_live_trader_common::config::DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            dhan_live_trader_common::config::InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            snapshot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        );

        let Json(result) = get_top_movers(State(state)).await;
        assert!(result.available);
        assert_eq!(result.gainers.len(), 2);
        assert_eq!(result.losers.len(), 1);
        assert_eq!(result.most_active.len(), 1);
        assert_eq!(result.total_tracked, 200);
        assert_eq!(result.gainers[0].security_id, 100);
        assert!((result.gainers[0].change_pct - 8.5_f32).abs() < f32::EPSILON);
    }

    // -------------------------------------------------------------------
    // TopMoversResponse serialization round-trip with all fields populated
    // -------------------------------------------------------------------

    #[test]
    fn response_serialization_round_trip_json_fields() {
        let resp = TopMoversResponse {
            available: true,
            gainers: vec![MoverEntry {
                security_id: 1,
                exchange_segment_code: 2,
                last_traded_price: 100.0_f32,
                change_pct: 5.0_f32,
                volume: 10000,
            }],
            losers: vec![MoverEntry {
                security_id: 2,
                exchange_segment_code: 1,
                last_traded_price: 50.0_f32,
                change_pct: -3.0_f32,
                volume: 5000,
            }],
            most_active: vec![MoverEntry {
                security_id: 3,
                exchange_segment_code: 2,
                last_traded_price: 200.0_f32,
                change_pct: 0.5_f32,
                volume: 999999,
            }],
            total_tracked: 500,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"gainers\""));
        assert!(json.contains("\"losers\""));
        assert!(json.contains("\"most_active\""));
        assert!(json.contains("\"total_tracked\":500"));
        assert!(json.contains("\"change_pct\""));
        assert!(json.contains("\"volume\":999999"));
    }

    // -------------------------------------------------------------------
    // From<&TopMoversSnapshot>: all fields preserved
    // -------------------------------------------------------------------

    #[test]
    fn from_snapshot_conversion_preserves_all_fields() {
        let gainer = MoverEntry {
            security_id: 10,
            exchange_segment_code: 1,
            last_traded_price: 250.5_f32,
            change_pct: 12.3_f32,
            volume: 100_000,
        };
        let loser = MoverEntry {
            security_id: 20,
            exchange_segment_code: 2,
            last_traded_price: 80.0_f32,
            change_pct: -7.5_f32,
            volume: 50_000,
        };
        let active = MoverEntry {
            security_id: 30,
            exchange_segment_code: 1,
            last_traded_price: 500.0_f32,
            change_pct: 0.1_f32,
            volume: 1_000_000,
        };

        let snapshot = TopMoversSnapshot {
            gainers: vec![gainer],
            losers: vec![loser],
            most_active: vec![active],
            total_tracked: 999,
        };

        let resp = TopMoversResponse::from(&snapshot);
        assert!(resp.available);
        assert_eq!(resp.total_tracked, 999);

        // Gainers
        assert_eq!(resp.gainers.len(), 1);
        assert_eq!(resp.gainers[0].security_id, 10);
        assert_eq!(resp.gainers[0].exchange_segment_code, 1);
        assert!((resp.gainers[0].last_traded_price - 250.5_f32).abs() < f32::EPSILON);
        assert!((resp.gainers[0].change_pct - 12.3_f32).abs() < f32::EPSILON);
        assert_eq!(resp.gainers[0].volume, 100_000);

        // Losers
        assert_eq!(resp.losers.len(), 1);
        assert_eq!(resp.losers[0].security_id, 20);
        assert!((resp.losers[0].change_pct - (-7.5_f32)).abs() < f32::EPSILON);

        // Most active
        assert_eq!(resp.most_active.len(), 1);
        assert_eq!(resp.most_active[0].security_id, 30);
        assert_eq!(resp.most_active[0].volume, 1_000_000);
    }

    #[test]
    fn from_snapshot_conversion_empty_lists() {
        let snapshot = TopMoversSnapshot {
            gainers: vec![],
            losers: vec![],
            most_active: vec![],
            total_tracked: 0,
        };

        let resp = TopMoversResponse::from(&snapshot);
        assert!(resp.available); // available is always true when converted from a snapshot
        assert!(resp.gainers.is_empty());
        assert!(resp.losers.is_empty());
        assert!(resp.most_active.is_empty());
        assert_eq!(resp.total_tracked, 0);
    }

    #[test]
    fn from_snapshot_conversion_multiple_entries() {
        let entries: Vec<MoverEntry> = (0..5)
            .map(|i| MoverEntry {
                security_id: i as u32,
                exchange_segment_code: 1,
                last_traded_price: 100.0_f32 + i as f32,
                change_pct: i as f32,
                volume: (i as u32) * 1000,
            })
            .collect();

        let snapshot = TopMoversSnapshot {
            gainers: entries.clone(),
            losers: entries[..2].to_vec(),
            most_active: entries[..3].to_vec(),
            total_tracked: 500,
        };

        let resp = TopMoversResponse::from(&snapshot);
        assert_eq!(resp.gainers.len(), 5);
        assert_eq!(resp.losers.len(), 2);
        assert_eq!(resp.most_active.len(), 3);
        assert_eq!(resp.total_tracked, 500);
    }

    // -------------------------------------------------------------------
    // From conversion: available is always true regardless of content
    // -------------------------------------------------------------------

    #[test]
    fn from_snapshot_always_sets_available_true() {
        let snapshot = TopMoversSnapshot {
            gainers: vec![],
            losers: vec![],
            most_active: vec![],
            total_tracked: 0,
        };
        let resp = TopMoversResponse::from(&snapshot);
        assert!(resp.available);
    }

    // -------------------------------------------------------------------
    // From conversion: negative change_pct preserved
    // -------------------------------------------------------------------

    #[test]
    fn from_snapshot_preserves_negative_change_pct() {
        let snapshot = TopMoversSnapshot {
            gainers: vec![],
            losers: vec![MoverEntry {
                security_id: 99,
                exchange_segment_code: 1,
                last_traded_price: 45.0_f32,
                change_pct: -15.75_f32,
                volume: 200,
            }],
            most_active: vec![],
            total_tracked: 1,
        };
        let resp = TopMoversResponse::from(&snapshot);
        assert_eq!(resp.losers.len(), 1);
        assert!((resp.losers[0].change_pct - (-15.75_f32)).abs() < f32::EPSILON);
    }

    // -------------------------------------------------------------------
    // From conversion: large total_tracked preserved
    // -------------------------------------------------------------------

    #[test]
    fn from_snapshot_preserves_large_total_tracked() {
        let snapshot = TopMoversSnapshot {
            gainers: vec![],
            losers: vec![],
            most_active: vec![],
            total_tracked: usize::MAX,
        };
        let resp = TopMoversResponse::from(&snapshot);
        assert_eq!(resp.total_tracked, usize::MAX);
    }

    // -------------------------------------------------------------------
    // From conversion: cloned data is independent of source
    // -------------------------------------------------------------------

    #[test]
    fn from_snapshot_clones_data_independently() {
        let entry = MoverEntry {
            security_id: 77,
            exchange_segment_code: 2,
            last_traded_price: 300.0_f32,
            change_pct: 2.5_f32,
            volume: 50_000,
        };
        let snapshot = TopMoversSnapshot {
            gainers: vec![entry],
            losers: vec![],
            most_active: vec![],
            total_tracked: 10,
        };
        let resp = TopMoversResponse::from(&snapshot);
        // Verify the response has its own copy
        assert_eq!(resp.gainers[0].security_id, 77);
        assert_eq!(resp.gainers[0].volume, 50_000);
        // Original snapshot is still intact (not moved)
        assert_eq!(snapshot.gainers[0].security_id, 77);
    }
}
