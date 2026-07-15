//! Source-scan ratchet: EVERY WebSocket disconnect + reconnect must be
//! structured-logged — RETIRED IN FULL (PR-C2, 2026-07-13).
//!
//! Operator directive 2026-06-12 ("whenever disconnect/reconnect it should
//! always alert AND everything be logged, tracked, captured") was pinned
//! here against the Dhan main-feed `connection.rs` choke points
//! (`record_disconnect` warn!, the reconnect info!, and the
//! `classify_disconnect_cause` source threading). That machinery was DELETED
//! with the Dhan live-WS lane (operator retirement directive —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §B), so all
//! three ratchets died with the functionality they pinned.
//!
//! The SURVIVING order-update WS keeps its structured disconnect/reconnect
//! logging pinned by its own in-file tests + `ws_event_audit_wiring_guard.rs`
//! (every lifecycle transition stamps a `ws_event_audit` row); the Groww
//! feed's lifecycle logging is pinned app-side (groww bridge / supervisor
//! guards). Re-introducing a Dhan market-data WS requires a fresh dated
//! operator quote in the scope-lock rule file FIRST — and with it, this
//! guard's assertions.
