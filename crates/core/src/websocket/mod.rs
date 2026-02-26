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
pub mod subscription_builder;
pub mod types;

pub use connection::WebSocketConnection;
pub use connection_pool::WebSocketConnectionPool;
pub use subscription_builder::build_subscription_messages;
pub use types::{ConnectionId, ConnectionState, DisconnectCode, WebSocketError};
