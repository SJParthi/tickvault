//! Shared application state for the API server.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

/// Shared state available to all API handlers via axum's `State` extractor.
#[derive(Clone)]
pub struct SharedAppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    /// QuestDB config for SQL queries (stats endpoint) and instrument persistence.
    questdb_config: QuestDbConfig,
    /// Dhan API config (CSV URLs for manual instrument rebuild).
    dhan_config: DhanConfig,
    /// Instrument config (cache dir, timeout, build window).
    instrument_config: InstrumentConfig,
    /// Concurrency guard: prevents concurrent instrument rebuilds.
    rebuild_in_progress: AtomicBool,
}

impl SharedAppState {
    /// Creates new shared state.
    pub fn new(
        questdb_config: QuestDbConfig,
        dhan_config: DhanConfig,
        instrument_config: InstrumentConfig,
    ) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                questdb_config,
                dhan_config,
                instrument_config,
                rebuild_in_progress: AtomicBool::new(false),
            }),
        }
    }

    /// Returns the QuestDB config for making SQL queries.
    pub fn questdb_config(&self) -> &QuestDbConfig {
        &self.inner.questdb_config
    }

    /// Returns the Dhan API config (CSV URLs).
    pub fn dhan_config(&self) -> &DhanConfig {
        &self.inner.dhan_config
    }

    /// Returns the instrument config (cache dir, timeout, build window).
    pub fn instrument_config(&self) -> &InstrumentConfig {
        &self.inner.instrument_config
    }

    /// Returns the rebuild-in-progress atomic flag.
    pub fn rebuild_in_progress(&self) -> &AtomicBool {
        &self.inner.rebuild_in_progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

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

    #[test]
    fn test_shared_app_state_new_and_questdb_config() {
        let config = QuestDbConfig {
            host: "test-host".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let state = SharedAppState::new(config, test_dhan_config(), test_instrument_config());
        assert_eq!(state.questdb_config().host, "test-host");
        assert_eq!(state.questdb_config().ilp_port, 9009);
        assert_eq!(state.questdb_config().http_port, 9000);
    }

    #[test]
    fn test_shared_app_state_clone_shares_data() {
        let config = QuestDbConfig {
            host: "clone-test".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let state1 = SharedAppState::new(config, test_dhan_config(), test_instrument_config());
        let state2 = state1.clone();
        assert_eq!(state2.questdb_config().host, "clone-test");
    }
}
