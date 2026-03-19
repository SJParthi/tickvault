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
}
