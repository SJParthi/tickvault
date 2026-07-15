//! WebSocket connection management — Dhan ORDER-UPDATE only since PR-C2.
//!
//! PR-C2 (2026-07-13, operator retirement directive — see
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment"): the Dhan
//! live main-feed WS machinery is DELETED — `connection` (single main-feed
//! connection lifecycle), `connection_pool`, `subscription_builder`
//! (JSON subscribe batching), `pool_watchdog`, and `rate_limit_cooldown`
//! (the 429 cross-restart advisory) all retired with the lane. Groww is
//! the sole live market-data feed (its own native client under
//! `crate::feed::groww`); re-introducing a Dhan market-data WS requires a
//! fresh dated operator quote in the scope-lock rule file first.
//!
//! # Surviving modules
//! - `types` — Domain types: ConnectionState, DisconnectCode, WebSocketError
//!   (the parser + order-update WS + WAL consumers still use them)
//! - `order_update_connection` — the functional-dormant order-update WS
//!   (Q4-i; spawned by `dhan_rest_stack`)
//! - `activity_watchdog` / `market_hours_gate` / `tls` — shared plumbing the
//!   order-update connection uses
// PR #4 (2026-05-19): `depth_connection` module DELETED.

pub mod activity_watchdog;
pub mod market_hours_gate;
pub mod order_update_connection;
pub mod tls;
pub mod types;

// PR #4 (2026-05-19): depth_connection re-exports retired.
pub use order_update_connection::run_order_update_connection;
pub use types::{ConnectionId, ConnectionState, DisconnectCode, WebSocketError};
