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
//!
//! # 2026-08-09 revival — connection-layer PURE LOGIC (this round)
//! The operator authorized a 16-connection Dhan live feed on 2026-08-09 (see
//! `websocket-connection-scope-lock.md`, "2026-08-09 (SAME DAY, SECOND QUOTE)
//! — 16 CONNECTIONS + depth-20/depth-200 AUTHORIZED"). The DECISION half of
//! the deleted connection layer is rebuilt first — pure, allocation-free and
//! exhaustively unit-testable — before anything opens a socket:
//! - `reconnect_ladder` — the 0ms/1s/2s/5s/15s/30s-cap ladder plus the
//!   deterministic per-connection jitter that stops sixteen simultaneously
//!   dropped sockets from reconnecting in lockstep
//! - `idle_watchdog` — the 27-second monotonic-clock idle-reconnect decision
//! - `pool_budget` — per-endpoint-type and global 16-connection caps,
//!   fail-closed, because Dhan does not reject an over-limit socket: it
//!   silently disconnects the OLDEST one with code 805
//!
//! These three modules open no sockets, spawn no tasks, and are NOT yet wired
//! into the boot sequence.
// PR #4 (2026-05-19): `depth_connection` module DELETED.

pub mod activity_watchdog;
// 2026-08-09 revival, transport round: the ONE `DhanFeedSocket` implementation
// — tokio-tungstenite over TLS with an EXPLICIT frame cap (the library default
// is a 64 MiB message, which sixteen sockets could turn into a gigabyte of
// attacker-controlled allocation). Contains no policy: every reconnect, park
// and resubscribe decision stays in `pool_supervisor`.
pub mod connection;
pub mod idle_watchdog;
pub mod market_hours_gate;
pub mod order_update_connection;
pub mod pool_budget;
// 2026-08-09 revival, wiring round: the connection-pool SUPERVISOR — the pure
// per-connection state machine (reconnect policy, disconnect classification,
// subscribe guard, thundering-herd stagger) plus the thin async shell that
// executes it. Opens no socket itself: the transport is a trait
// (`DhanFeedSocket`) so every decision branch is unit-testable without a
// network. Spawned by `tickvault_app::dhan_feed_stack`, DEFAULT-OFF.
pub mod pool_supervisor;
pub mod reconnect_ladder;
// Rebuilt 2026-08-11 with the operator-approved 16-connection revival
// (websocket-connection-scope-lock.md, the two 2026-08-09 sections). Deleted
// in the 2026-07-17 stage-3 sweep alongside the lane it served.
pub mod subscription_builder;
pub mod tls;
pub mod types;

// PR #4 (2026-05-19): depth_connection re-exports retired.
pub use order_update_connection::run_order_update_connection;
pub use types::{ConnectionId, ConnectionState, DisconnectCode, WebSocketError};
