//! Shared application state for the API server.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};
use dhan_live_trader_common::instrument_types::IndexConstituencyMap;
use dhan_live_trader_core::pipeline::top_movers::SharedTopMoversSnapshot;

/// Shared handle to the index constituency map (Arc<RwLock<Option<...>>>).
pub type SharedConstituencyMap = Arc<RwLock<Option<IndexConstituencyMap>>>;

/// Shared handle to system health status for the `/health` endpoint.
pub type SharedHealthStatus = Arc<SystemHealthStatus>;

/// Subsystem health tracking — updated by background tasks, read by `/health`.
///
/// All fields are atomic for lock-free O(1) reads on the API hot path.
pub struct SystemHealthStatus {
    /// Number of active WebSocket connections (0-5).
    websocket_connections: AtomicU64,
    /// Whether the tick pipeline is actively processing.
    pipeline_active: AtomicBool,
    /// Whether QuestDB was reachable at last check.
    questdb_reachable: AtomicBool,
    /// Whether the auth token is currently valid.
    token_valid: AtomicBool,
    /// Boot timestamp (epoch seconds) — 0 if not yet booted.
    boot_epoch_secs: AtomicU64,
}

impl SystemHealthStatus {
    /// Creates a new health status with all subsystems in unknown/down state.
    pub fn new() -> Self {
        Self {
            websocket_connections: AtomicU64::new(0),
            pipeline_active: AtomicBool::new(false),
            questdb_reachable: AtomicBool::new(false),
            token_valid: AtomicBool::new(false),
            boot_epoch_secs: AtomicU64::new(0),
        }
    }

    /// Updates the WebSocket connection count.
    pub fn set_websocket_connections(&self, count: u64) {
        self.websocket_connections.store(count, Ordering::Relaxed);
    }

    /// Returns current WebSocket connection count.
    pub fn websocket_connections(&self) -> u64 {
        self.websocket_connections.load(Ordering::Relaxed)
    }

    /// Marks the tick pipeline as active/inactive.
    pub fn set_pipeline_active(&self, active: bool) {
        self.pipeline_active.store(active, Ordering::Relaxed);
    }

    /// Returns whether the tick pipeline is active.
    pub fn pipeline_active(&self) -> bool {
        self.pipeline_active.load(Ordering::Relaxed)
    }

    /// Marks QuestDB as reachable/unreachable.
    pub fn set_questdb_reachable(&self, reachable: bool) {
        self.questdb_reachable.store(reachable, Ordering::Relaxed);
    }

    /// Returns whether QuestDB is reachable.
    pub fn questdb_reachable(&self) -> bool {
        self.questdb_reachable.load(Ordering::Relaxed)
    }

    /// Marks the auth token as valid/invalid.
    pub fn set_token_valid(&self, valid: bool) {
        self.token_valid.store(valid, Ordering::Relaxed);
    }

    /// Returns whether the auth token is valid.
    pub fn token_valid(&self) -> bool {
        self.token_valid.load(Ordering::Relaxed)
    }

    /// Sets the boot timestamp.
    pub fn set_boot_epoch_secs(&self, epoch_secs: u64) {
        self.boot_epoch_secs.store(epoch_secs, Ordering::Relaxed);
    }

    /// Returns the boot timestamp (0 if not yet booted).
    pub fn boot_epoch_secs(&self) -> u64 {
        self.boot_epoch_secs.load(Ordering::Relaxed)
    }

    /// Derives overall system status from subsystem states.
    pub fn overall_status(&self) -> &'static str {
        if !self.token_valid() {
            return "degraded";
        }
        if self.websocket_connections() == 0 {
            return "degraded";
        }
        if !self.questdb_reachable() {
            return "degraded";
        }
        "healthy"
    }
}

impl Default for SystemHealthStatus {
    fn default() -> Self {
        Self::new()
    }
}

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
    /// Shared top movers snapshot from the tick pipeline.
    top_movers_snapshot: SharedTopMoversSnapshot,
    /// Shared index constituency map.
    constituency_map: SharedConstituencyMap,
    /// Subsystem health status for the `/health` endpoint.
    health_status: SharedHealthStatus,
}

impl SharedAppState {
    /// Creates new shared state.
    pub fn new(
        questdb_config: QuestDbConfig,
        dhan_config: DhanConfig,
        instrument_config: InstrumentConfig,
        top_movers_snapshot: SharedTopMoversSnapshot,
        constituency_map: SharedConstituencyMap,
        health_status: SharedHealthStatus,
    ) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                questdb_config,
                dhan_config,
                instrument_config,
                rebuild_in_progress: AtomicBool::new(false),
                top_movers_snapshot,
                constituency_map,
                health_status,
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

    /// Returns the shared top movers snapshot handle.
    pub fn top_movers_snapshot(&self) -> &SharedTopMoversSnapshot {
        &self.inner.top_movers_snapshot
    }

    /// Returns the shared index constituency map handle.
    pub fn constituency_map(&self) -> &SharedConstituencyMap {
        &self.inner.constituency_map
    }

    /// Returns the shared system health status handle.
    pub fn health_status(&self) -> &SharedHealthStatus {
        &self.inner.health_status
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

    fn empty_snapshot() -> SharedTopMoversSnapshot {
        std::sync::Arc::new(std::sync::RwLock::new(None))
    }

    fn empty_constituency() -> SharedConstituencyMap {
        std::sync::Arc::new(std::sync::RwLock::new(None))
    }

    fn test_health_status() -> SharedHealthStatus {
        Arc::new(SystemHealthStatus::new())
    }

    #[test]
    fn test_shared_app_state_new_and_questdb_config() {
        let config = QuestDbConfig {
            host: "test-host".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let state = SharedAppState::new(
            config,
            test_dhan_config(),
            test_instrument_config(),
            empty_snapshot(),
            empty_constituency(),
            test_health_status(),
        );
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
        let state1 = SharedAppState::new(
            config,
            test_dhan_config(),
            test_instrument_config(),
            empty_snapshot(),
            empty_constituency(),
            test_health_status(),
        );
        let state2 = state1.clone();
        assert_eq!(state2.questdb_config().host, "clone-test");
    }

    #[test]
    fn test_system_health_status_default_is_degraded() {
        let health = SystemHealthStatus::new();
        assert_eq!(health.overall_status(), "degraded");
        assert_eq!(health.websocket_connections(), 0);
        assert!(!health.pipeline_active());
        assert!(!health.questdb_reachable());
        assert!(!health.token_valid());
        assert_eq!(health.boot_epoch_secs(), 0);
    }

    #[test]
    fn test_system_health_status_healthy_when_all_up() {
        let health = SystemHealthStatus::new();
        health.set_websocket_connections(3);
        health.set_pipeline_active(true);
        health.set_questdb_reachable(true);
        health.set_token_valid(true);
        health.set_boot_epoch_secs(1_772_073_900);

        assert_eq!(health.overall_status(), "healthy");
        assert_eq!(health.websocket_connections(), 3);
        assert!(health.pipeline_active());
        assert!(health.questdb_reachable());
        assert!(health.token_valid());
        assert_eq!(health.boot_epoch_secs(), 1_772_073_900);
    }

    #[test]
    fn test_system_health_status_degraded_when_ws_disconnected() {
        let health = SystemHealthStatus::new();
        health.set_token_valid(true);
        health.set_questdb_reachable(true);
        // websocket_connections stays 0
        assert_eq!(health.overall_status(), "degraded");
    }

    #[test]
    fn test_system_health_status_degraded_when_token_invalid() {
        let health = SystemHealthStatus::new();
        health.set_websocket_connections(3);
        health.set_questdb_reachable(true);
        // token_valid stays false
        assert_eq!(health.overall_status(), "degraded");
    }

    #[test]
    fn test_system_health_status_degraded_when_questdb_down() {
        let health = SystemHealthStatus::new();
        health.set_websocket_connections(3);
        health.set_token_valid(true);
        // questdb_reachable stays false
        assert_eq!(health.overall_status(), "degraded");
    }
}
