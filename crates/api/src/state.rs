//! Shared application state for the API server.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Timeout for QuestDB HTTP queries from API handlers (seconds).
const QUESTDB_HTTP_CLIENT_TIMEOUT_SECS: u64 = 10;

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
    /// Number of active Live Market Feed WebSocket connections (0-5).
    websocket_connections: AtomicU64,
    /// Number of active 20-level depth connections (0-4).
    depth_20_connections: AtomicU64,
    /// Number of active 200-level depth connections (0-4).
    depth_200_connections: AtomicU64,
    /// Whether the order update WebSocket is connected.
    order_update_connected: AtomicBool,
    /// Whether the tick pipeline is actively processing.
    pipeline_active: AtomicBool,
    /// Whether QuestDB was reachable at last check.
    questdb_reachable: AtomicBool,
    /// Whether the auth token is currently valid.
    token_valid: AtomicBool,
    /// Whether the tick persistence writer is connected to QuestDB.
    tick_persistence_connected: AtomicBool,
    /// Number of ticks currently buffered in the ring buffer (waiting for QuestDB).
    tick_buffer_size: AtomicU64,
    /// Total ticks spilled to disk (ring buffer overflow).
    ticks_spilled: AtomicU64,
    /// Boot timestamp (epoch seconds) — 0 if not yet booted.
    boot_epoch_secs: AtomicU64,
}

impl SystemHealthStatus {
    /// Creates a new health status with all subsystems in unknown/down state.
    pub fn new() -> Self {
        Self {
            websocket_connections: AtomicU64::new(0),
            depth_20_connections: AtomicU64::new(0),
            depth_200_connections: AtomicU64::new(0),
            order_update_connected: AtomicBool::new(false),
            pipeline_active: AtomicBool::new(false),
            questdb_reachable: AtomicBool::new(false),
            token_valid: AtomicBool::new(false),
            tick_persistence_connected: AtomicBool::new(false),
            tick_buffer_size: AtomicU64::new(0),
            ticks_spilled: AtomicU64::new(0),
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

    /// Updates the 20-level depth connection count.
    // TEST-EXEMPT: trivial AtomicU64 store, tested indirectly by health endpoint tests
    pub fn set_depth_20_connections(&self, count: u64) {
        self.depth_20_connections.store(count, Ordering::Relaxed);
    }

    /// Returns current 20-level depth connection count.
    // TEST-EXEMPT: trivial AtomicU64 load, tested indirectly by health endpoint tests
    pub fn depth_20_connections(&self) -> u64 {
        self.depth_20_connections.load(Ordering::Relaxed)
    }

    /// Updates the 200-level depth connection count.
    // TEST-EXEMPT: trivial AtomicU64 store, tested indirectly by health endpoint tests
    pub fn set_depth_200_connections(&self, count: u64) {
        self.depth_200_connections.store(count, Ordering::Relaxed);
    }

    /// Returns current 200-level depth connection count.
    // TEST-EXEMPT: trivial AtomicU64 load, tested indirectly by health endpoint tests
    pub fn depth_200_connections(&self) -> u64 {
        self.depth_200_connections.load(Ordering::Relaxed)
    }

    /// Marks order update WebSocket as connected/disconnected.
    // TEST-EXEMPT: trivial AtomicBool store, tested indirectly by health endpoint tests
    pub fn set_order_update_connected(&self, connected: bool) {
        self.order_update_connected
            .store(connected, Ordering::Relaxed);
    }

    /// Returns whether order update WebSocket is connected.
    // TEST-EXEMPT: trivial AtomicBool load, tested indirectly by health endpoint tests
    pub fn order_update_connected(&self) -> bool {
        self.order_update_connected.load(Ordering::Relaxed)
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

    /// Marks tick persistence as connected/disconnected.
    // TEST-EXEMPT: trivial AtomicBool store, tested indirectly by health endpoint tests
    pub fn set_tick_persistence_connected(&self, connected: bool) {
        self.tick_persistence_connected
            .store(connected, Ordering::Relaxed);
    }

    /// Returns whether tick persistence is connected to QuestDB.
    // TEST-EXEMPT: trivial AtomicBool load, tested indirectly by health endpoint tests
    pub fn tick_persistence_connected(&self) -> bool {
        self.tick_persistence_connected.load(Ordering::Relaxed)
    }

    /// Updates tick buffer size (ring buffer count).
    // TEST-EXEMPT: trivial AtomicU64 store, tested indirectly by health endpoint tests
    pub fn set_tick_buffer_size(&self, size: u64) {
        self.tick_buffer_size.store(size, Ordering::Relaxed);
    }

    /// Returns tick buffer size.
    // TEST-EXEMPT: trivial AtomicU64 load, tested indirectly by health endpoint tests
    pub fn tick_buffer_size(&self) -> u64 {
        self.tick_buffer_size.load(Ordering::Relaxed)
    }

    /// Updates total ticks spilled to disk.
    // TEST-EXEMPT: trivial AtomicU64 store, tested indirectly by health endpoint tests
    pub fn set_ticks_spilled(&self, count: u64) {
        self.ticks_spilled.store(count, Ordering::Relaxed);
    }

    /// Returns total ticks spilled to disk.
    // TEST-EXEMPT: trivial AtomicU64 load, tested indirectly by health endpoint tests
    pub fn ticks_spilled(&self) -> u64 {
        self.ticks_spilled.load(Ordering::Relaxed)
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
    /// Shared HTTP client for QuestDB queries (connection pooling + keep-alive).
    /// Reused across all handler invocations instead of creating per-request.
    questdb_http_client: reqwest::Client,
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
        // Single shared HTTP client for all QuestDB queries (connection pooling).
        let questdb_http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(
                QUESTDB_HTTP_CLIENT_TIMEOUT_SECS,
            ))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_default();

        Self {
            inner: Arc::new(AppStateInner {
                questdb_config,
                dhan_config,
                instrument_config,
                rebuild_in_progress: AtomicBool::new(false),
                top_movers_snapshot,
                constituency_map,
                health_status,
                questdb_http_client,
            }),
        }
    }

    /// Returns the QuestDB config for making SQL queries.
    pub fn questdb_config(&self) -> &QuestDbConfig {
        &self.inner.questdb_config
    }

    /// Returns the shared HTTP client for QuestDB queries.
    /// Reuses connections via keep-alive instead of creating per-request.
    pub fn questdb_http_client(&self) -> &reqwest::Client {
        &self.inner.questdb_http_client
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
            sandbox_base_url: String::new(),
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
        // Shared HTTP client is created and accessible
        let _client = state.questdb_http_client();
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
        assert_eq!(health.depth_20_connections(), 0);
        assert_eq!(health.depth_200_connections(), 0);
        assert!(!health.order_update_connected());
        assert!(!health.pipeline_active());
        assert!(!health.questdb_reachable());
        assert!(!health.token_valid());
        assert!(!health.tick_persistence_connected());
        assert_eq!(health.boot_epoch_secs(), 0);
    }

    #[test]
    fn test_depth_20_connections_set_and_get() {
        let health = SystemHealthStatus::new();
        assert_eq!(health.depth_20_connections(), 0);
        health.set_depth_20_connections(4);
        assert_eq!(health.depth_20_connections(), 4);
    }

    #[test]
    fn test_depth_200_connections_set_and_get() {
        let health = SystemHealthStatus::new();
        assert_eq!(health.depth_200_connections(), 0);
        health.set_depth_200_connections(4);
        assert_eq!(health.depth_200_connections(), 4);
    }

    #[test]
    fn test_order_update_connected_set_and_get() {
        let health = SystemHealthStatus::new();
        assert!(!health.order_update_connected());
        health.set_order_update_connected(true);
        assert!(health.order_update_connected());
        health.set_order_update_connected(false);
        assert!(!health.order_update_connected());
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

    // -------------------------------------------------------------------
    // Default impl — must behave identically to new()
    // -------------------------------------------------------------------

    #[test]
    fn test_system_health_status_default_matches_new() {
        let from_new = SystemHealthStatus::new();
        let from_default = SystemHealthStatus::default();

        assert_eq!(
            from_new.websocket_connections(),
            from_default.websocket_connections()
        );
        assert_eq!(
            from_new.depth_20_connections(),
            from_default.depth_20_connections()
        );
        assert_eq!(
            from_new.depth_200_connections(),
            from_default.depth_200_connections()
        );
        assert_eq!(
            from_new.order_update_connected(),
            from_default.order_update_connected()
        );
        assert_eq!(from_new.pipeline_active(), from_default.pipeline_active());
        assert_eq!(
            from_new.questdb_reachable(),
            from_default.questdb_reachable()
        );
        assert_eq!(from_new.token_valid(), from_default.token_valid());
        assert_eq!(
            from_new.tick_persistence_connected(),
            from_default.tick_persistence_connected()
        );
        assert_eq!(from_new.boot_epoch_secs(), from_default.boot_epoch_secs());
        assert_eq!(from_new.overall_status(), from_default.overall_status());
    }

    // -------------------------------------------------------------------
    // SharedAppState accessor coverage
    // -------------------------------------------------------------------

    #[test]
    fn test_shared_app_state_dhan_config_accessor() {
        let state = SharedAppState::new(
            QuestDbConfig {
                host: "test".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            test_dhan_config(),
            test_instrument_config(),
            empty_snapshot(),
            empty_constituency(),
            test_health_status(),
        );
        assert_eq!(state.dhan_config().websocket_url, "wss://api-feed.dhan.co");
        assert_eq!(state.dhan_config().max_websocket_connections, 5);
    }

    #[test]
    fn test_shared_app_state_instrument_config_accessor() {
        let state = SharedAppState::new(
            QuestDbConfig {
                host: "test".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            test_dhan_config(),
            test_instrument_config(),
            empty_snapshot(),
            empty_constituency(),
            test_health_status(),
        );
        assert_eq!(state.instrument_config().csv_download_timeout_secs, 120);
    }

    #[test]
    fn test_shared_app_state_rebuild_in_progress_accessor() {
        let state = SharedAppState::new(
            QuestDbConfig {
                host: "test".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            test_dhan_config(),
            test_instrument_config(),
            empty_snapshot(),
            empty_constituency(),
            test_health_status(),
        );
        // Initially false
        assert!(!state.rebuild_in_progress().load(Ordering::SeqCst));
        // Can set
        state.rebuild_in_progress().store(true, Ordering::SeqCst);
        assert!(state.rebuild_in_progress().load(Ordering::SeqCst));
    }

    #[test]
    fn test_shared_app_state_health_status_accessor() {
        let health = test_health_status();
        health.set_token_valid(true);
        health.set_websocket_connections(2);
        health.set_questdb_reachable(true);

        let state = SharedAppState::new(
            QuestDbConfig {
                host: "test".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            test_dhan_config(),
            test_instrument_config(),
            empty_snapshot(),
            empty_constituency(),
            health,
        );
        assert_eq!(state.health_status().overall_status(), "healthy");
        assert_eq!(state.health_status().websocket_connections(), 2);
    }

    #[test]
    fn test_shared_app_state_top_movers_snapshot_accessor() {
        let state = SharedAppState::new(
            QuestDbConfig {
                host: "test".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            test_dhan_config(),
            test_instrument_config(),
            empty_snapshot(),
            empty_constituency(),
            test_health_status(),
        );
        let snapshot = state.top_movers_snapshot();
        assert!(snapshot.read().unwrap().is_none());
    }

    #[test]
    fn test_shared_app_state_constituency_map_accessor() {
        let state = SharedAppState::new(
            QuestDbConfig {
                host: "test".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            test_dhan_config(),
            test_instrument_config(),
            empty_snapshot(),
            empty_constituency(),
            test_health_status(),
        );
        let map = state.constituency_map();
        assert!(map.read().unwrap().is_none());
    }

    // -------------------------------------------------------------------
    // SystemHealthStatus: toggle operations
    // -------------------------------------------------------------------

    #[test]
    fn test_system_health_status_toggle_pipeline_active() {
        let health = SystemHealthStatus::new();
        assert!(!health.pipeline_active());
        health.set_pipeline_active(true);
        assert!(health.pipeline_active());
        health.set_pipeline_active(false);
        assert!(!health.pipeline_active());
    }

    #[test]
    fn test_system_health_status_boot_epoch_secs_set_and_get() {
        let health = SystemHealthStatus::default();
        assert_eq!(health.boot_epoch_secs(), 0);
        health.set_boot_epoch_secs(1_700_000_000);
        assert_eq!(health.boot_epoch_secs(), 1_700_000_000);
    }

    // -------------------------------------------------------------------
    // SystemHealthStatus::default() field-by-field verification
    // -------------------------------------------------------------------

    #[test]
    fn test_system_health_status_default_all_fields_zero_or_false() {
        let health = SystemHealthStatus::default();
        assert_eq!(health.websocket_connections(), 0);
        assert_eq!(health.depth_20_connections(), 0);
        assert_eq!(health.depth_200_connections(), 0);
        assert!(!health.order_update_connected());
        assert!(!health.pipeline_active());
        assert!(!health.questdb_reachable());
        assert!(!health.token_valid());
        assert!(!health.tick_persistence_connected());
        assert_eq!(health.boot_epoch_secs(), 0);
    }

    #[test]
    fn test_system_health_status_default_overall_status_is_degraded() {
        let health = SystemHealthStatus::default();
        assert_eq!(health.overall_status(), "degraded");
    }

    // -------------------------------------------------------------------
    // overall_status priority: token checked first, then ws, then questdb
    // -------------------------------------------------------------------

    #[test]
    fn test_overall_status_token_invalid_is_degraded_regardless() {
        let health = SystemHealthStatus::default();
        health.set_websocket_connections(5);
        health.set_questdb_reachable(true);
        // token_valid stays false
        assert_eq!(health.overall_status(), "degraded");
    }

    #[test]
    fn test_overall_status_ws_zero_is_degraded_even_with_valid_token() {
        let health = SystemHealthStatus::default();
        health.set_token_valid(true);
        health.set_questdb_reachable(true);
        // websocket_connections stays 0
        assert_eq!(health.overall_status(), "degraded");
    }

    #[test]
    fn test_overall_status_questdb_down_is_degraded_even_with_token_and_ws() {
        let health = SystemHealthStatus::default();
        health.set_token_valid(true);
        health.set_websocket_connections(3);
        // questdb_reachable stays false
        assert_eq!(health.overall_status(), "degraded");
    }

    #[test]
    fn test_overall_status_all_conditions_met_is_healthy() {
        let health = SystemHealthStatus::default();
        health.set_token_valid(true);
        health.set_websocket_connections(1); // minimum 1 is enough
        health.set_questdb_reachable(true);
        assert_eq!(health.overall_status(), "healthy");
    }

    // -------------------------------------------------------------------
    // SystemHealthStatus: set_websocket_connections to various values
    // -------------------------------------------------------------------

    #[test]
    fn test_system_health_status_websocket_connections_range() {
        let health = SystemHealthStatus::default();
        for count in [0, 1, 2, 3, 4, 5] {
            health.set_websocket_connections(count);
            assert_eq!(health.websocket_connections(), count);
        }
    }

    // -------------------------------------------------------------------
    // SystemHealthStatus: pipeline_active does not affect overall_status
    // -------------------------------------------------------------------

    #[test]
    fn test_pipeline_active_does_not_affect_overall_status() {
        let health = SystemHealthStatus::default();
        health.set_token_valid(true);
        health.set_websocket_connections(3);
        health.set_questdb_reachable(true);
        // pipeline_active is false but status should still be healthy
        assert!(!health.pipeline_active());
        assert_eq!(health.overall_status(), "healthy");
    }

    // -------------------------------------------------------------------
    // SystemHealthStatus: transition from healthy back to degraded
    // -------------------------------------------------------------------

    #[test]
    fn test_system_health_status_healthy_to_degraded_on_token_invalidation() {
        let health = SystemHealthStatus::default();
        health.set_token_valid(true);
        health.set_websocket_connections(3);
        health.set_questdb_reachable(true);
        assert_eq!(health.overall_status(), "healthy");

        // Invalidate token → degraded
        health.set_token_valid(false);
        assert_eq!(health.overall_status(), "degraded");
    }

    #[test]
    fn test_system_health_status_healthy_to_degraded_on_ws_disconnect() {
        let health = SystemHealthStatus::default();
        health.set_token_valid(true);
        health.set_websocket_connections(3);
        health.set_questdb_reachable(true);
        assert_eq!(health.overall_status(), "healthy");

        // All WS connections lost → degraded
        health.set_websocket_connections(0);
        assert_eq!(health.overall_status(), "degraded");
    }

    #[test]
    fn test_system_health_status_healthy_to_degraded_on_questdb_down() {
        let health = SystemHealthStatus::default();
        health.set_token_valid(true);
        health.set_websocket_connections(3);
        health.set_questdb_reachable(true);
        assert_eq!(health.overall_status(), "healthy");

        // QuestDB goes down → degraded
        health.set_questdb_reachable(false);
        assert_eq!(health.overall_status(), "degraded");
    }

    // -------------------------------------------------------------------
    // SystemHealthStatus::default() via Default trait object
    // -------------------------------------------------------------------

    #[test]
    fn test_system_health_status_default_via_default_trait() {
        let health: SystemHealthStatus = Default::default();
        assert_eq!(health.websocket_connections(), 0);
        assert_eq!(health.depth_20_connections(), 0);
        assert_eq!(health.depth_200_connections(), 0);
        assert!(!health.order_update_connected());
        assert!(!health.pipeline_active());
        assert!(!health.questdb_reachable());
        assert!(!health.token_valid());
        assert!(!health.tick_persistence_connected());
        assert_eq!(health.boot_epoch_secs(), 0);
        assert_eq!(health.overall_status(), "degraded");
    }

    // -------------------------------------------------------------------
    // SystemHealthStatus: boot_epoch_secs can be overwritten
    // -------------------------------------------------------------------

    #[test]
    fn test_system_health_status_toggle_tick_persistence_connected() {
        let health = SystemHealthStatus::new();
        assert!(!health.tick_persistence_connected());
        health.set_tick_persistence_connected(true);
        assert!(health.tick_persistence_connected());
        health.set_tick_persistence_connected(false);
        assert!(!health.tick_persistence_connected());
    }

    #[test]
    fn test_system_health_status_boot_epoch_secs_overwrite() {
        let health = SystemHealthStatus::default();
        health.set_boot_epoch_secs(1_000_000);
        assert_eq!(health.boot_epoch_secs(), 1_000_000);
        health.set_boot_epoch_secs(2_000_000);
        assert_eq!(health.boot_epoch_secs(), 2_000_000);
    }
}
