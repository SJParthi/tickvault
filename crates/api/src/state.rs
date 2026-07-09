//! Shared application state for the API server.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Timeout for QuestDB HTTP queries from API handlers (seconds).
const QUESTDB_HTTP_CLIENT_TIMEOUT_SECS: u64 = 10;

/// TTL for the cached `/api/stats` response body (2026-07-09 audit
/// hardening). The `/dashboard` page polls every ~5s, so a 5s TTL means at
/// most one 5-query QuestDB pass per poll interval regardless of how many
/// internet clients hammer the endpoint.
const STATS_CACHE_TTL_SECS: u64 = 5;

/// TTL for cached `/api/quote/{security_id}` 200 bodies. "Latest tick"
/// honestly becomes "latest tick, ≤1s old" — documented envelope change.
const QUOTE_CACHE_TTL_SECS: u64 = 1;

/// TTL for the cached `/api/board/data` response body (2026-07-09
/// follow-up — closes the #1458 HIGH residual). The `/board` page polls
/// every ~3s, so a 2s TTL keeps a single tab's data fresh (poll interval
/// exceeds the TTL). Attack cost: one 3-query QuestDB pass per 2s in the
/// healthy regime; with QuestDB black-holed (computes ~3s > TTL, no
/// single-flight) the honest bound is the LIMITER — ≤5 computes/s (see the
/// `board_data` handler docs).
const BOARD_CACHE_TTL_SECS: u64 = 2;

use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};
use tickvault_common::feed_health::FeedHealthRegistry;

// PR #6a (2026-05-19): SharedConstituencyMap + IndexConstituencyMap import RETIRED
// (4-IDX_I LOCKED_UNIVERSE — NSE index composition not tracked).

/// Shared handle to system health status for the `/health` endpoint.
pub type SharedHealthStatus = Arc<SystemHealthStatus>;

/// Subsystem health tracking — updated by background tasks, read by `/health`.
///
/// All fields are atomic for lock-free O(1) reads on the API hot path.
pub struct SystemHealthStatus {
    /// Number of active Live Market Feed WebSocket connections (0-5).
    websocket_connections: AtomicU64,
    /// Whether the order update WebSocket is connected.
    order_update_connected: AtomicBool,
    /// Whether the tick pipeline is actively processing.
    pipeline_active: AtomicBool,
    /// Whether QuestDB was reachable at last check (RAW single-probe signal).
    /// Read by `/health`, `overall_status`, and observability. A single
    /// momentary blip flips this — that is correct for observability.
    questdb_reachable: AtomicBool,
    /// DAMPED QuestDB-reachability signal that feeds the pool-watchdog's 429
    /// ride-out EXIT decision ONLY. Unlike the raw `questdb_reachable` above,
    /// this flips `false` only after N *consecutive* failed probes (see
    /// `damp_questdb_exit_signal` in `crates/app/src/main.rs`), so a single
    /// blip (GC pause / transient HTTP hiccup / a 2s probe-timeout under load)
    /// during a live in-market 429 storm no longer forces a `process::exit(2)`
    /// → 775-SID cold re-subscribe → next 429 restart/429 loop. A genuine
    /// SUSTAINED outage still flips it after N ticks, so the exit gate is never
    /// worse than today for a real dead DB. Inits `true` — a watchdog that has
    /// never probed must not pre-force an exit.
    questdb_reachable_for_exit_decision: AtomicBool,
    /// Whether the auth token is currently valid.
    token_valid: AtomicBool,
    /// Seconds remaining until token expiry (0 = expired or unknown).
    token_remaining_secs: AtomicU64,
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
            order_update_connected: AtomicBool::new(false),
            pipeline_active: AtomicBool::new(false),
            questdb_reachable: AtomicBool::new(false),
            // Inits `true`: before the first probe the exit gate must NOT read
            // "down" (that would pre-force a genuine-fatal exit). The gate has
            // other predicates (token valid, no non-reconnectable code) so this
            // default never rides out an absent pool on its own.
            questdb_reachable_for_exit_decision: AtomicBool::new(true),
            token_valid: AtomicBool::new(false),
            token_remaining_secs: AtomicU64::new(0),
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
    ///
    /// Also emits the `tv_pipeline_active` Prometheus gauge (0 = stopped,
    /// 1 = running) so the System Overview dashboard's "Pipeline Status"
    /// tile reflects the live flag instead of "No data". Without this
    /// emission the Grafana panel queries a metric that never gets
    /// scraped → tile reads RED "STOPPED" forever even when the tick
    /// processor is consuming ticks.
    pub fn set_pipeline_active(&self, active: bool) {
        self.pipeline_active.store(active, Ordering::Relaxed);
        metrics::gauge!("tv_pipeline_active").set(if active { 1.0 } else { 0.0 });
    }

    /// Returns whether the tick pipeline is active.
    pub fn pipeline_active(&self) -> bool {
        self.pipeline_active.load(Ordering::Relaxed)
    }

    /// Marks QuestDB as reachable/unreachable.
    pub fn set_questdb_reachable(&self, reachable: bool) {
        self.questdb_reachable.store(reachable, Ordering::Relaxed);
    }

    /// Returns whether QuestDB is reachable (RAW single-probe signal).
    pub fn questdb_reachable(&self) -> bool {
        self.questdb_reachable.load(Ordering::Relaxed)
    }

    /// Sets the DAMPED QuestDB-reachability signal used ONLY by the pool
    /// watchdog's 429 ride-out exit decision. The watchdog computes this from
    /// N consecutive failed probes via `damp_questdb_exit_signal`; a single
    /// success resets. Never read by `/health` — keep that on the raw signal.
    pub fn set_questdb_reachable_for_exit_decision(&self, reachable: bool) {
        self.questdb_reachable_for_exit_decision
            .store(reachable, Ordering::Relaxed);
    }

    /// Returns the DAMPED QuestDB-reachability signal for the ride-out exit
    /// gate. `false` only after N consecutive failed probes — so a single blip
    /// does not force `process::exit`, but a sustained outage still does.
    pub fn questdb_reachable_for_exit_decision(&self) -> bool {
        self.questdb_reachable_for_exit_decision
            .load(Ordering::Relaxed)
    }

    /// Marks the auth token as valid/invalid.
    pub fn set_token_valid(&self, valid: bool) {
        self.token_valid.store(valid, Ordering::Relaxed);
    }

    /// Returns whether the auth token is valid.
    pub fn token_valid(&self) -> bool {
        self.token_valid.load(Ordering::Relaxed)
    }

    /// Updates token remaining seconds (for health endpoint).
    // TEST-EXEMPT: trivial AtomicU64 store, tested indirectly by health endpoint tests
    pub fn set_token_remaining_secs(&self, secs: u64) {
        self.token_remaining_secs.store(secs, Ordering::Relaxed);
    }

    /// Returns seconds until token expiry (0 = expired or unknown).
    // TEST-EXEMPT: trivial AtomicU64 load, tested indirectly by health endpoint tests
    pub fn token_remaining_secs(&self) -> u64 {
        self.token_remaining_secs.load(Ordering::Relaxed)
    }

    // Valkey reachability setter/getter DELETED in #O4 (2026-05-24) —
    // Valkey removed from the runtime.

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

// PR #2 (2026-05-18): movers pipeline retired. `top_movers_snapshot`,
// `cascade_fanout`, and `new_with_cascade_fanout()` constructor were
// removed alongside the deleted `/api/movers/v2` route + the
// `top_movers` / `option_movers` core modules.
struct AppStateInner {
    /// QuestDB config for SQL queries (stats endpoint) and instrument persistence.
    questdb_config: QuestDbConfig,
    /// Dhan API config (CSV URLs for manual instrument rebuild).
    dhan_config: DhanConfig,
    /// Instrument config (cache dir, timeout, build window).
    instrument_config: InstrumentConfig,
    /// Concurrency guard: prevents concurrent instrument rebuilds.
    rebuild_in_progress: AtomicBool,
    // PR #6a (2026-05-19): constituency_map field RETIRED (4-IDX_I scope).
    /// Subsystem health status for the `/health` endpoint.
    health_status: SharedHealthStatus,
    /// Shared HTTP client for QuestDB queries (connection pooling + keep-alive).
    /// Reused across all handler invocations instead of creating per-request.
    questdb_http_client: reqwest::Client,
    /// Runtime per-feed enable/disable flags (feed-toggle API). The SAME `Arc`
    /// is shared with the Groww bridge so a `POST /api/feeds/{feed}` toggle is
    /// observed live by the feed lane. Lock-free O(1) reads.
    feed_runtime: Arc<crate::feed_state::FeedRuntimeState>,
    /// Per-feed live-feed health signals (ticks/candles/drops/connected). The
    /// SAME `Arc` the feed lanes update, so `GET /api/feeds/health` reports the
    /// truthful live verdict. Lock-free O(1). The 4/5-arg constructors create a
    /// fresh empty registry (tests); production injects the boot-shared one.
    feed_health: Arc<FeedHealthRegistry>,
    /// TTL cache for the `/api/stats` response body (2026-07-09 audit
    /// hardening — public-funnel DoS/DB-load surface). Single slot, 5s TTL.
    stats_cache: crate::response_cache::SingleSlotTtlCache,
    /// TTL cache for `/api/quote/{security_id}` 200 bodies. Per-SID, 1s TTL,
    /// hard entry cap; the handler caches ONLY 200 responses so garbage
    /// attacker-chosen security_ids can never grow the map.
    quote_cache: crate::response_cache::BoundedTtlCache,
    /// TTL cache for the `/api/board/data` response body (2026-07-09
    /// follow-up). Single slot, 2s TTL — see [`BOARD_CACHE_TTL_SECS`].
    board_cache: crate::response_cache::SingleSlotTtlCache,
}

impl SharedAppState {
    /// Creates new shared state with a DEFAULT [`crate::feed_state::FeedRuntimeState`]
    /// (Dhan ON, Groww OFF). Production uses [`Self::new_with_feed_runtime`] to
    /// inject the config-seeded, bridge-shared instance; this 4-arg constructor is
    /// retained unchanged for the existing call sites (tests).
    pub fn new(
        questdb_config: QuestDbConfig,
        dhan_config: DhanConfig,
        instrument_config: InstrumentConfig,
        health_status: SharedHealthStatus,
    ) -> Self {
        Self::new_with_feed_runtime(
            questdb_config,
            dhan_config,
            instrument_config,
            health_status,
            Arc::new(crate::feed_state::FeedRuntimeState::default()),
        )
    }

    /// Creates new shared state, injecting the shared [`crate::feed_state::FeedRuntimeState`]
    /// `Arc` (the SAME instance passed to the Groww bridge) so the feed-toggle API
    /// and the feed lanes observe one source of truth.
    pub fn new_with_feed_runtime(
        questdb_config: QuestDbConfig,
        dhan_config: DhanConfig,
        instrument_config: InstrumentConfig,
        health_status: SharedHealthStatus,
        feed_runtime: Arc<crate::feed_state::FeedRuntimeState>,
    ) -> Self {
        Self::new_with_feed_runtime_and_health(
            questdb_config,
            dhan_config,
            instrument_config,
            health_status,
            feed_runtime,
            Arc::new(FeedHealthRegistry::new()),
        )
    }

    /// Creates new shared state, also injecting the boot-shared per-feed
    /// [`FeedHealthRegistry`] `Arc` (the SAME instance the feed lanes update) so
    /// `GET /api/feeds/health` reports each feed's truthful live verdict.
    pub fn new_with_feed_runtime_and_health(
        questdb_config: QuestDbConfig,
        dhan_config: DhanConfig,
        instrument_config: InstrumentConfig,
        health_status: SharedHealthStatus,
        feed_runtime: Arc<crate::feed_state::FeedRuntimeState>,
        feed_health: Arc<FeedHealthRegistry>,
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
                health_status,
                questdb_http_client,
                feed_runtime,
                feed_health,
                stats_cache: crate::response_cache::SingleSlotTtlCache::new(
                    std::time::Duration::from_secs(STATS_CACHE_TTL_SECS),
                ),
                quote_cache: crate::response_cache::BoundedTtlCache::new(
                    std::time::Duration::from_secs(QUOTE_CACHE_TTL_SECS),
                    crate::response_cache::QUOTE_CACHE_MAX_ENTRIES,
                ),
                board_cache: crate::response_cache::SingleSlotTtlCache::new(
                    std::time::Duration::from_secs(BOARD_CACHE_TTL_SECS),
                ),
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

    // PR #6a (2026-05-19): constituency_map accessor RETIRED (4-IDX_I scope).

    /// Returns the shared system health status handle.
    pub fn health_status(&self) -> &SharedHealthStatus {
        &self.inner.health_status
    }

    /// Returns the shared runtime per-feed enable/disable state (feed-toggle API).
    pub fn feed_runtime(&self) -> &Arc<crate::feed_state::FeedRuntimeState> {
        &self.inner.feed_runtime
    }

    /// Returns the shared per-feed live-feed health registry (the lanes update it;
    /// `GET /api/feeds/health` reads it for the truthful per-feed verdict).
    pub fn feed_health(&self) -> &Arc<FeedHealthRegistry> {
        &self.inner.feed_health
    }

    /// Returns the `/api/stats` TTL response cache (2026-07-09 hardening).
    // TEST-EXEMPT: trivial accessor; exercised by stats-handler cache tests.
    pub fn stats_cache(&self) -> &crate::response_cache::SingleSlotTtlCache {
        &self.inner.stats_cache
    }

    /// Returns the `/api/quote/{security_id}` TTL response cache
    /// (2026-07-09 hardening; only-200 bodies, bounded).
    // TEST-EXEMPT: trivial accessor; exercised by quote-handler cache tests.
    pub fn quote_cache(&self) -> &crate::response_cache::BoundedTtlCache {
        &self.inner.quote_cache
    }

    /// Returns the `/api/board/data` TTL response cache (2026-07-09
    /// follow-up — closes the #1458 HIGH residual).
    // TEST-EXEMPT: trivial accessor; exercised by the board-handler cache test.
    pub fn board_cache(&self) -> &crate::response_cache::SingleSlotTtlCache {
        &self.inner.board_cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

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

    // PR #2 (2026-05-18): empty_snapshot() helper retired alongside
    // the deleted SharedTopMoversSnapshot type.

    // PR #6a (2026-05-19): empty_constituency() helper retired.

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
            test_health_status(),
        );
        assert_eq!(state.questdb_config().host, "test-host");
        assert_eq!(state.questdb_config().ilp_port, 9009);
        assert_eq!(state.questdb_config().http_port, 9000);
        // Shared HTTP client is created and accessible
        let _client = state.questdb_http_client();
    }

    #[test]
    fn test_new_with_feed_runtime_injects_and_exposes_feed_runtime() {
        use crate::feed_state::{Feed, FeedRuntimeState};
        use tickvault_common::config::FeedsConfig;
        let fr = Arc::new(FeedRuntimeState::from_config(&FeedsConfig {
            dhan_enabled: true,
            groww_enabled: true,
            ..Default::default()
        }));
        let state = SharedAppState::new_with_feed_runtime(
            QuestDbConfig {
                host: "h".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            test_dhan_config(),
            test_instrument_config(),
            test_health_status(),
            Arc::clone(&fr),
        );
        // The accessor returns the SAME shared runtime instance we injected.
        assert!(state.feed_runtime().is_enabled(Feed::Groww));
        fr.set_enabled(Feed::Groww, false);
        assert!(
            !state.feed_runtime().is_enabled(Feed::Groww),
            "AppState shares the one Arc — a flip is observed through it"
        );
    }

    #[test]
    fn test_new_with_feed_runtime_and_health_exposes_feed_health() {
        use tickvault_common::feed::Feed;
        use tickvault_common::feed_health::FeedHealthRegistry;
        let reg = Arc::new(FeedHealthRegistry::new());
        reg.set_connected(Feed::Dhan, true);
        let state = SharedAppState::new_with_feed_runtime_and_health(
            QuestDbConfig {
                host: "h".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            test_dhan_config(),
            test_instrument_config(),
            test_health_status(),
            Arc::new(crate::feed_state::FeedRuntimeState::default()),
            Arc::clone(&reg),
        );
        // feed_health() returns the SAME shared registry — a recorded signal is seen.
        let r = state
            .feed_health()
            .snapshot(Feed::Dhan, true, true, true, 0);
        assert!(
            r.input.connected,
            "AppState shares the one health registry Arc"
        );
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
        assert!(!health.order_update_connected());
        assert!(!health.pipeline_active());
        assert!(!health.questdb_reachable());
        assert!(!health.token_valid());
        assert!(!health.tick_persistence_connected());
        assert_eq!(health.boot_epoch_secs(), 0);
    }

    #[test]
    fn test_questdb_reachable_for_exit_decision_independent_of_raw() {
        let health = SystemHealthStatus::new();
        // Damped exit signal inits `true` (never pre-force an exit before any
        // probe); the raw signal inits `false`.
        assert!(
            health.questdb_reachable_for_exit_decision(),
            "damped exit signal must init true"
        );
        assert!(
            !health.questdb_reachable(),
            "raw signal inits false (unknown until first probe)"
        );

        // A single RAW blip (set raw false) must NOT touch the damped exit
        // signal — `/health` reflects the blip, the exit gate does not.
        health.set_questdb_reachable(false);
        assert!(
            !health.questdb_reachable(),
            "raw signal reflects the blip (observability intact)"
        );
        assert!(
            health.questdb_reachable_for_exit_decision(),
            "damped exit signal is independent of a single raw blip"
        );

        // The two signals move independently in both directions.
        health.set_questdb_reachable(true);
        health.set_questdb_reachable_for_exit_decision(false);
        assert!(health.questdb_reachable(), "raw is now true");
        assert!(
            !health.questdb_reachable_for_exit_decision(),
            "damped exit signal is independently settable to false"
        );
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
            health,
        );
        assert_eq!(state.health_status().overall_status(), "healthy");
        assert_eq!(state.health_status().websocket_connections(), 2);
    }

    // PR #2 (2026-05-18): test_shared_app_state_top_movers_snapshot_accessor
    // removed alongside the deleted accessor and the SharedTopMoversSnapshot
    // type.

    // PR #6a (2026-05-19): test_shared_app_state_constituency_map_accessor RETIRED.

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
