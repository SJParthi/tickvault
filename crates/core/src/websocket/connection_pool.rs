//! WebSocket connection pool for Dhan Live Market Feed.
//!
//! Distributes instruments across up to 5 WebSocket connections
//! (Dhan limit: 5,000 instruments per connection, 25,000 total).
//! Each connection runs independently on its own tokio task.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::info;

use dhan_live_trader_common::config::{DhanConfig, WebSocketConfig};
use dhan_live_trader_common::constants::{
    MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, MAX_TOTAL_SUBSCRIPTIONS,
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
/// Creates the minimum number of connections needed to cover all instruments,
/// respecting Dhan's 5,000-per-connection and 25,000-total limits.
pub struct WebSocketConnectionPool {
    /// Active connections (up to 5).
    connections: Vec<Arc<WebSocketConnection>>,

    /// Receiver for raw binary frames from all connections.
    frame_receiver: mpsc::Receiver<Vec<u8>>,
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
    /// Returns `CapacityExceeded` if instruments exceed total capacity (25,000).
    pub fn new(
        token_handle: TokenHandle,
        client_id: String,
        dhan_config: DhanConfig,
        ws_config: WebSocketConfig,
        instruments: Vec<InstrumentSubscription>,
        feed_mode: FeedMode,
    ) -> Result<Self, WebSocketError> {
        let total = instruments.len();

        if total > MAX_TOTAL_SUBSCRIPTIONS {
            return Err(WebSocketError::CapacityExceeded {
                requested: total,
                capacity: MAX_TOTAL_SUBSCRIPTIONS,
            });
        }

        let max_per_conn = dhan_config
            .max_instruments_per_connection
            .min(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION);
        let num_connections = if total == 0 {
            1 // Always create at least one connection
        } else {
            total.div_ceil(max_per_conn)
        };

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
        })
    }

    /// Spawns all connections as independent tokio tasks.
    ///
    /// Each connection manages its own lifecycle (connect, subscribe, ping,
    /// read, reconnect). Returns task handles for monitoring/cancellation.
    // O(1) EXEMPT: begin — spawn runs once per session
    pub fn spawn_all(&self) -> Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>> {
        self.connections
            .iter()
            .map(|conn| {
                let conn = Arc::clone(conn);
                tokio::spawn(async move { conn.run().await })
            })
            .collect()
        // O(1) EXEMPT: end
    }

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

    #[test]
    fn test_pool_empty_instruments_creates_one_connection() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 1);
        assert_eq!(pool.total_instruments(), 0);
    }

    #[test]
    fn test_pool_single_connection_under_5000() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(3000),
            FeedMode::Quote,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 1);
        assert_eq!(pool.total_instruments(), 3000);
    }

    #[test]
    fn test_pool_exactly_5000_uses_one_connection() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Full,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 1);
    }

    #[test]
    fn test_pool_5001_uses_two_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(5001),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 2);
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
        match result.unwrap_err() {
            WebSocketError::CapacityExceeded {
                requested,
                capacity,
            } => {
                assert_eq!(requested, 25001);
                assert_eq!(capacity, 25000);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn test_pool_round_robin_distribution() {
        // 10001 instruments across 5000-per-conn = 3 connections
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10001),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 3);

        let healths = pool.health();
        // Round-robin: 3334 + 3334 + 3333
        let total: usize = healths.iter().map(|h| h.subscribed_count).sum();
        assert_eq!(total, 10001);
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
        assert_eq!(pool.connection_count(), 1);
        assert_eq!(pool.total_instruments(), 1);
    }

    #[test]
    fn test_pool_boundary_4999_one_connection() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(4999),
            FeedMode::Quote,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 1);
    }

    #[test]
    fn test_pool_boundary_10000_two_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10000),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 2);
    }

    #[test]
    fn test_pool_boundary_15000_three_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(15000),
            FeedMode::Full,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 3);
    }

    #[test]
    fn test_pool_boundary_20000_four_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(20000),
            FeedMode::Ticker,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 4);
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
        assert!(debug_str.contains("2")); // 5001 instruments = 2 connections
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
        // 5000 / 2000 = 3 connections needed
        assert_eq!(pool.connection_count(), 3);
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
}
