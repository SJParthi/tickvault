//! WebSocket connection management for Dhan Live Market Feed.
//!
//! Manages the full lifecycle: connect → authenticate → subscribe → ping →
//! read binary frames → reconnect on failure.
//!
//! # Modules
//! - `types` — Domain types: ConnectionState, DisconnectCode, WebSocketError
//! - `subscription_builder` — JSON subscription message builder (max 100/msg)
//! - `connection` — Single WebSocket connection lifecycle
//! - `connection_pool` — Pool of up to 5 connections with instrument distribution
//!
//! # Boot Sequence Position
//! Auth → **WebSocket** → Parse → Route → Indicators

pub mod connection;
pub mod connection_pool;
pub mod depth_connection;
pub mod order_update_connection;
pub mod subscription_builder;
pub mod tls;
pub mod types;

pub use connection::WebSocketConnection;
pub use connection_pool::WebSocketConnectionPool;
pub use depth_connection::run_twenty_depth_connection;
pub use order_update_connection::run_order_update_connection;
pub use subscription_builder::build_subscription_messages;
pub use types::{ConnectionId, ConnectionState, DisconnectCode, WebSocketError};
