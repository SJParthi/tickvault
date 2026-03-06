//! WebSocket connection pool for Dhan Live Market Feed.
//!
//! Always creates the maximum allowed number of WebSocket connections
//! (default 5, capped at `MAX_WEBSOCKET_CONNECTIONS`), distributing
//! instruments round-robin across all connections. Empty connections
//! stay alive for Phase 2 dynamic rebalancing.
//!
//! Dhan limit: 5,000 instruments per connection, 25,000 total.
//! Each connection runs independently on its own tokio task.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::info;

use dhan_live_trader_common::config::{DhanConfig, WebSocketConfig};
use dhan_live_trader_common::constants::{
    MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, MAX_WEBSOCKET_CONNECTIONS,
};
use dhan_live_trader_common::types::FeedMode;

use crate::auth::TokenHandle;
use crate::websocket::connection::WebSocketConnection;
use crate::websocket::types::{ConnectionHealth, InstrumentSubscription, WebSocketError};

// ---------------------------------------------------------------------------
// Connection Pool
// ---------------------------------------------------------------------------

/// Pool of WebSocket connections distributing instruments across connections.
///
/// Always creates the maximum allowed number of connections (default 5),
/// distributing instruments round-robin. Empty connections stay alive for
/// Phase 2 dynamic rebalancing. Respects per-connection and total capacity.
pub struct WebSocketConnectionPool {
    /// Active connections (up to 5).
    connections: Vec<Arc<WebSocketConnection>>,

    /// Receiver for raw binary frames from all connections.
    frame_receiver: mpsc::Receiver<Vec<u8>>,

    /// Stagger delay between connection spawns (milliseconds). 0 = no stagger.
    connection_stagger_ms: u64,
}

impl std::fmt::Debug for WebSocketConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketConnectionPool")
            .field("connection_count", &self.connections.len())
            .finish()
    }
}

impl WebSocketConnectionPool {
    /// Creates a new connection pool and distributes instruments across connections.
    ///
    /// # Arguments
    /// * `token_handle` — Shared atomic token for O(1) reads.
    /// * `client_id` — Dhan client ID (from SSM).
    /// * `dhan_config` — Dhan API configuration.
    /// * `ws_config` — WebSocket keep-alive and reconnection config.
    /// * `instruments` — Full list of instruments to subscribe.
    /// * `feed_mode` — Desired feed granularity.
    ///
    /// # Errors
    /// Returns `CapacityExceeded` if instruments exceed dynamic capacity
    /// (`max_per_conn × num_connections`, default 5,000 × 5 = 25,000).
    pub fn new(
        token_handle: TokenHandle,
        client_id: String,
        dhan_config: DhanConfig,
        ws_config: WebSocketConfig,
        instruments: Vec<InstrumentSubscription>,
        feed_mode: FeedMode,
    ) -> Result<Self, WebSocketError> {
        let total = instruments.len();

        // Compute connection parameters first — needed for dynamic capacity check.
        let max_per_conn = dhan_config
            .max_instruments_per_connection
            .min(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION);
        let num_connections = dhan_config
            .max_websocket_connections
            .min(MAX_WEBSOCKET_CONNECTIONS);

        // Dynamic capacity: effective limit depends on config, not hardcoded 25K.
        // If max_per_conn=2000, num_connections=5 → effective capacity = 10,000.
        let effective_capacity = max_per_conn * num_connections;
        if total > effective_capacity {
            return Err(WebSocketError::CapacityExceeded {
                requested: total,
                capacity: effective_capacity,
            });
        }

        // Shared channel: all connections send frames to a single receiver.
        // Buffer size: enough for burst traffic without blocking senders.
        let (frame_sender, frame_receiver) = mpsc::channel(65536);

        // O(1) EXEMPT: begin — pool constructor runs once at startup, not per tick
        // Distribute instruments round-robin across connections.
        let mut connection_instruments: Vec<Vec<InstrumentSubscription>> =
            (0..num_connections).map(|_| Vec::new()).collect();

        for (idx, instrument) in instruments.into_iter().enumerate() {
            connection_instruments[idx % num_connections].push(instrument);
        }

        let connections: Vec<Arc<WebSocketConnection>> = connection_instruments
            .into_iter()
            .enumerate()
            .map(|(id, assigned_instruments)| {
                Arc::new(WebSocketConnection::new(
                    id as u8,
                    token_handle.clone(),
                    client_id.clone(),
                    dhan_config.clone(),
                    ws_config.clone(),
                    assigned_instruments,
                    feed_mode,
                    frame_sender.clone(),
                ))
            })
            .collect();
        // O(1) EXEMPT: end

        info!(
            num_connections = connections.len(),
            total_instruments = total,
            "WebSocket connection pool created"
        );

        Ok(Self {
            connections,
            frame_receiver,
            connection_stagger_ms: ws_config.connection_stagger_ms,
        })
    }

    /// Spawns all connections as independent tokio tasks with staggered startup.
    ///
    /// Each connection manages its own lifecycle (connect, subscribe, ping,
    /// read, reconnect). Connections are spawned with a configurable delay
    /// between each to avoid hitting Dhan's server simultaneously.
    /// Returns task handles for monitoring/cancellation.
    // O(1) EXEMPT: begin — spawn runs once per session
    pub async fn spawn_all(&self) -> Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>> {
        let stagger = std::time::Duration::from_millis(self.connection_stagger_ms);
        let mut handles = Vec::with_capacity(self.connections.len());

        for (idx, conn) in self.connections.iter().enumerate() {
            if idx > 0 && !stagger.is_zero() {
                info!(
                    connection_id = idx,
                    stagger_ms = self.connection_stagger_ms,
                    "Waiting before spawning next WebSocket connection"
                );
                tokio::time::sleep(stagger).await;
            }

            let conn = Arc::clone(conn);
            handles.push(tokio::spawn(async move { conn.run().await }));

            info!(
                connection_id = idx,
                total = self.connections.len(),
                "Spawned WebSocket connection"
            );
        }

        handles
    }
    // O(1) EXEMPT: end

    /// Returns the frame receiver for downstream binary frame processing.
    ///
    /// The caller owns this receiver and reads raw binary frames from
    /// all active connections. Each frame is a complete Dhan binary packet.
    pub fn take_frame_receiver(&mut self) -> mpsc::Receiver<Vec<u8>> {
        // Replace with a dummy channel — receiver can only be taken once.
        let (_, dummy) = mpsc::channel(1);
        std::mem::replace(&mut self.frame_receiver, dummy)
    }

    /// Returns health snapshots for all connections.
    pub fn health(&self) -> Vec<ConnectionHealth> {
        self.connections.iter().map(|conn| conn.health()).collect() // O(1) EXEMPT: monitoring, not per tick
    }

    /// Number of connections in the pool.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Total instruments across all connections.
    pub fn total_instruments(&self) -> usize {
        self.connections
            .iter()
            .map(|conn| conn.health().subscribed_count)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::types::ConnectionState;
    use dhan_live_trader_common::types::ExchangeSegment;

    fn make_test_dhan_config() -> DhanConfig {
        DhanConfig {
            websocket_url: "wss://api-feed.dhan.co".to_string(),
            rest_api_base_url: "https://api.dhan.co/v2".to_string(),
            auth_base_url: "https://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/csv-fallback".to_string(),
            max_instruments_per_connection: 5000,
            max_websocket_connections: 5,
        }
    }

    fn make_test_ws_config() -> WebSocketConfig {
        WebSocketConfig {
            ping_interval_secs: 10,
            pong_timeout_secs: 10,
            max_consecutive_pong_failures: 2,
            reconnect_initial_delay_ms: 500,
            reconnect_max_delay_ms: 30000,
            reconnect_max_attempts: 10,
            subscription_batch_size: 100,
            connection_stagger_ms: 0, // No stagger in tests for speed
        }
    }

    fn make_test_token_handle() -> TokenHandle {
        Arc::new(arc_swap::ArcSwap::new(Arc::new(None)))
    }

    fn make_instruments(count: usize) -> Vec<InstrumentSubscription> {
        (0..count)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, (i as u32) + 1000))
            .collect()
    }

    /// Extract `CapacityExceeded` fields from a `WebSocketError`, panicking if
    /// the variant doesn't match. Consolidates 3 identical panic sites.
    #[track_caller]
    fn unwrap_capacity_exceeded(err: WebSocketError) -> (usize, usize) {
        match err {
            WebSocketError::CapacityExceeded {
                requested,
                capacity,
            } => (requested, capacity),
            other => panic!("expected CapacityExceeded, got {other:?}"),
        }
    }

    #[test]
    fn test_pool_empty_instruments_creates_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 0);
    }

    #[test]
    fn test_pool_under_5000_still_creates_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(3000),
            FeedMode::Quote,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 3000);
    }

    #[test]
    fn test_pool_exactly_5000_creates_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Full,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_5001_creates_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(5001),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 5001);
    }

    #[test]
    fn test_pool_25000_uses_five_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(25000),
            FeedMode::Full,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 25000);
    }

    #[test]
    fn test_pool_exceeds_capacity_returns_error() {
        let result = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(25001),
            FeedMode::Ticker,
        );
        assert!(result.is_err());
        let (requested, capacity) = unwrap_capacity_exceeded(result.unwrap_err());
        assert_eq!(requested, 25001);
        assert_eq!(capacity, 25000);
    }

    #[test]
    fn test_pool_round_robin_distribution() {
        // 10001 instruments distributed round-robin across 5 connections
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10001),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);

        let healths = pool.health();
        // Round-robin across 5: 2001 + 2000 + 2000 + 2000 + 2000
        let total: usize = healths.iter().map(|h| h.subscribed_count).sum();
        assert_eq!(total, 10001);
        // Verify no connection exceeds max_per_conn (5000)
        for h in &healths {
            assert!(h.subscribed_count <= 5000);
        }
    }

    #[test]
    fn test_pool_single_instrument() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(1),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 1);
    }

    #[test]
    fn test_pool_boundary_4999_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(4999),
            FeedMode::Quote,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_boundary_10000_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10000),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_boundary_15000_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(15000),
            FeedMode::Full,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_boundary_20000_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(20000),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_take_frame_receiver_once() {
        let mut pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
        )
        .unwrap();

        // First take succeeds — we get the real receiver
        let _receiver = pool.take_frame_receiver();

        // Second take gets a dummy channel (capacity 1, no senders)
        let mut dummy_receiver = pool.take_frame_receiver();
        // Dummy receiver should return None immediately since no sender exists
        assert!(
            dummy_receiver.try_recv().is_err(),
            "Second take should return empty dummy receiver"
        );
    }

    #[test]
    fn test_pool_debug_impl() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(5001),
            FeedMode::Ticker,
        )
        .unwrap();
        let debug_str = format!("{pool:?}");
        assert!(debug_str.contains("WebSocketConnectionPool"));
        assert!(debug_str.contains("connection_count"));
        assert!(debug_str.contains("5")); // Always 5 connections
    }

    #[test]
    fn test_pool_config_override_smaller_max_per_connection() {
        // Config says 2000 per connection instead of default 5000
        let config = DhanConfig {
            max_instruments_per_connection: 2000,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Full,
        )
        .unwrap();
        // Always max connections (5), instruments distributed round-robin
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 5000);
    }

    #[test]
    fn test_pool_total_instruments_matches_after_distribution() {
        // Test that round-robin doesn't lose or gain instruments
        for count in [
            1, 100, 4999, 5000, 5001, 10000, 10001, 15000, 20000, 24999, 25000,
        ] {
            let pool = WebSocketConnectionPool::new(
                make_test_token_handle(),
                "test-client".to_string(),
                make_test_dhan_config(),
                make_test_ws_config(),
                make_instruments(count),
                FeedMode::Ticker,
            )
            .unwrap();
            assert_eq!(
                pool.total_instruments(),
                count,
                "Mismatch for {count} instruments"
            );
        }
    }

    #[test]
    fn test_pool_health_returns_all_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(12000),
            FeedMode::Full,
        )
        .unwrap();
        let healths = pool.health();
        assert_eq!(healths.len(), pool.connection_count());
        for (idx, health) in healths.iter().enumerate() {
            assert_eq!(health.connection_id, idx as u8);
            assert_eq!(health.state, ConnectionState::Disconnected);
        }
    }

    // --- New tests for always-max-connections + dynamic capacity ---

    #[test]
    fn test_pool_config_max_connections_3() {
        // Config limits to 3 connections
        let config = DhanConfig {
            max_websocket_connections: 3,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 3);
        assert_eq!(pool.total_instruments(), 5000);
    }

    #[test]
    fn test_pool_config_max_per_conn_2000_exceeds() {
        // max_per_conn=2000, max_conns=5 → effective capacity = 10,000
        let config = DhanConfig {
            max_instruments_per_connection: 2000,
            ..make_test_dhan_config()
        };
        let result = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(15000),
            FeedMode::Ticker,
        );
        assert!(result.is_err());
        let (requested, capacity) = unwrap_capacity_exceeded(result.unwrap_err());
        assert_eq!(requested, 15000);
        assert_eq!(capacity, 10000); // 2000 × 5
    }

    #[test]
    fn test_pool_config_max_conns_100_capped_at_5() {
        // Config says 100, but MAX_WEBSOCKET_CONNECTIONS caps at 5
        let config = DhanConfig {
            max_websocket_connections: 100,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    // --- Property-based test ---

    mod proptest_pool {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn test_pool_no_instrument_loss_any_count(count in 0usize..=25000) {
                let instruments = make_instruments(count);
                let pool = WebSocketConnectionPool::new(
                    make_test_token_handle(),
                    "test".to_string(),
                    make_test_dhan_config(),
                    make_test_ws_config(),
                    instruments,
                    FeedMode::Ticker,
                ).unwrap();
                prop_assert_eq!(pool.total_instruments(), count);
                prop_assert_eq!(pool.connection_count(), 5);
            }
        }
    }

    // --- spawn_all() Tests ---

    #[tokio::test]
    async fn test_spawn_all_returns_correct_number_of_handles() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(), // No token → connections will fail
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 0, // Fail immediately
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            make_instruments(100),
            FeedMode::Ticker,
        )
        .unwrap();

        let handles = pool.spawn_all().await;
        assert_eq!(
            handles.len(),
            pool.connection_count(),
            "spawn_all must return one handle per connection"
        );

        // All tasks should complete (with errors, since no token)
        for handle in handles {
            let result = handle.await;
            assert!(result.is_ok(), "JoinHandle must not panic");
            // The inner result is an error (no token → reconnection exhausted)
            assert!(result.unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn test_spawn_all_empty_pool_returns_handles() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 0,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![], // no instruments
            FeedMode::Ticker,
        )
        .unwrap();

        let handles = pool.spawn_all().await;
        assert_eq!(handles.len(), 5, "empty pool still has 5 connections");

        for handle in handles {
            let result = handle.await;
            assert!(result.is_ok(), "JoinHandle must not panic");
        }
    }

    #[tokio::test]
    async fn test_spawn_all_tasks_return_reconnection_exhausted() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 2,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            make_instruments(10),
            FeedMode::Ticker,
        )
        .unwrap();

        let handles = pool.spawn_all().await;

        for handle in handles {
            let join_result = handle.await.unwrap();
            match join_result {
                Err(WebSocketError::ReconnectionExhausted { attempts, .. }) => {
                    assert_eq!(attempts, 2);
                }
                other => panic!("Expected ReconnectionExhausted, got {other:?}"),
            }
        }
    }

    // --- Additional coverage tests ---

    #[test]
    fn test_pool_config_max_connections_1() {
        // Config limits to 1 connection — all instruments on one connection.
        let config = DhanConfig {
            max_websocket_connections: 1,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 1);
        assert_eq!(pool.total_instruments(), 100);

        let healths = pool.health();
        assert_eq!(healths.len(), 1);
        assert_eq!(healths[0].subscribed_count, 100);
        assert_eq!(healths[0].connection_id, 0);
    }

    #[test]
    fn test_pool_config_max_per_conn_1() {
        // max_per_conn=1 means effective capacity = 1 * 5 = 5.
        let config = DhanConfig {
            max_instruments_per_connection: 1,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(5),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 5);
        // Each connection should have exactly 1 instrument
        for h in pool.health() {
            assert_eq!(h.subscribed_count, 1);
        }
    }

    #[test]
    fn test_pool_config_max_per_conn_1_exceeds() {
        // max_per_conn=1, max_conns=5 → capacity=5. 6 instruments should fail.
        let config = DhanConfig {
            max_instruments_per_connection: 1,
            ..make_test_dhan_config()
        };
        let result = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(6),
            FeedMode::Ticker,
        );
        assert!(result.is_err());
        let (requested, capacity) = unwrap_capacity_exceeded(result.unwrap_err());
        assert_eq!(requested, 6);
        assert_eq!(capacity, 5);
    }

    #[test]
    fn test_pool_health_initial_state_all_disconnected() {
        // All connections start in Disconnected state.
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(500),
            FeedMode::Full,
        )
        .unwrap();
        for h in pool.health() {
            assert_eq!(h.state, ConnectionState::Disconnected);
            assert_eq!(h.total_reconnections, 0);
        }
    }

    #[test]
    fn test_pool_health_connection_ids_sequential() {
        // Connection IDs should be 0, 1, 2, 3, 4.
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
        )
        .unwrap();
        let healths = pool.health();
        for (idx, h) in healths.iter().enumerate() {
            assert_eq!(h.connection_id, idx as u8);
        }
    }

    #[test]
    fn test_pool_take_frame_receiver_returns_working_receiver() {
        // The first take should return a receiver connected to the real senders.
        let mut pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10),
            FeedMode::Ticker,
        )
        .unwrap();

        let mut receiver = pool.take_frame_receiver();
        // No data has been sent yet, so try_recv should return Empty (not Disconnected)
        match receiver.try_recv() {
            Err(mpsc::error::TryRecvError::Empty) => {} // expected
            Err(mpsc::error::TryRecvError::Disconnected) => {
                // Also acceptable — senders may not be held open
            }
            Ok(_) => panic!("Should not receive data before connection starts"),
        }
    }

    #[test]
    fn test_pool_debug_format_with_various_sizes() {
        for count in [0, 1, 100, 5000] {
            let config = DhanConfig {
                max_websocket_connections: 3,
                ..make_test_dhan_config()
            };
            let pool = WebSocketConnectionPool::new(
                make_test_token_handle(),
                "test-client".to_string(),
                config,
                make_test_ws_config(),
                make_instruments(count),
                FeedMode::Ticker,
            )
            .unwrap();
            let debug_str = format!("{pool:?}");
            assert!(debug_str.contains("3"));
            assert!(debug_str.contains("WebSocketConnectionPool"));
        }
    }

    #[test]
    fn test_pool_round_robin_even_distribution() {
        // 10 instruments across 5 connections → 2 each.
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10),
            FeedMode::Ticker,
        )
        .unwrap();
        for h in pool.health() {
            assert_eq!(h.subscribed_count, 2);
        }
    }

    #[test]
    fn test_pool_round_robin_uneven_distribution() {
        // 7 instruments across 5 connections → 2, 2, 1, 1, 1.
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(7),
            FeedMode::Ticker,
        )
        .unwrap();
        let healths = pool.health();
        let counts: Vec<usize> = healths.iter().map(|h| h.subscribed_count).collect();
        assert_eq!(counts, vec![2, 2, 1, 1, 1]);
    }

    #[test]
    fn test_pool_config_exceeds_max_websocket_connections_constant() {
        // MAX_WEBSOCKET_CONNECTIONS is 5. Config of 10 should be capped.
        let config = DhanConfig {
            max_websocket_connections: 10,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_config_exceeds_max_instruments_per_connection_constant() {
        // MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION is 5000. Config of 10000 should be capped.
        let config = DhanConfig {
            max_instruments_per_connection: 10000,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(25000),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        // Effective capacity is still 25000 (5000 * 5), not 50000 (10000 * 5)
        assert_eq!(pool.total_instruments(), 25000);
    }

    #[tokio::test]
    async fn test_spawn_all_with_single_connection() {
        let config = DhanConfig {
            max_websocket_connections: 1,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            WebSocketConfig {
                reconnect_max_attempts: 0,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            make_instruments(5),
            FeedMode::Ticker,
        )
        .unwrap();

        let handles = pool.spawn_all().await;
        assert_eq!(handles.len(), 1);

        for handle in handles {
            let result = handle.await;
            assert!(result.is_ok(), "JoinHandle must not panic");
        }
    }

    #[tokio::test]
    async fn test_spawn_all_with_stagger_returns_correct_handles() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 0,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                connection_stagger_ms: 50, // 50ms stagger for fast test
                ..make_test_ws_config()
            },
            make_instruments(10),
            FeedMode::Ticker,
        )
        .unwrap();

        let start = tokio::time::Instant::now();
        let handles = pool.spawn_all().await;
        let elapsed = start.elapsed();

        assert_eq!(handles.len(), 5);
        // 4 gaps of 50ms = 200ms minimum
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "Stagger should cause at least 200ms delay for 5 connections, got {elapsed:?}",
        );

        for handle in handles {
            let result = handle.await;
            assert!(result.is_ok(), "JoinHandle must not panic");
        }
    }

    #[tokio::test]
    async fn test_spawn_all_zero_stagger_is_instant() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 0,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                connection_stagger_ms: 0,
                ..make_test_ws_config()
            },
            make_instruments(10),
            FeedMode::Ticker,
        )
        .unwrap();

        let start = tokio::time::Instant::now();
        let handles = pool.spawn_all().await;
        let elapsed = start.elapsed();

        assert_eq!(handles.len(), 5);
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "Zero stagger should be near-instant, got {elapsed:?}",
        );

        for handle in handles {
            let _ = handle.await;
        }
    }
}
