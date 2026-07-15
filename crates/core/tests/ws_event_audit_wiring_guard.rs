//! Source-scan ratchet: the WS-event audit producer wiring must stay intact.
//!
//! Operator directive 2026-06-12: every WebSocket connect/disconnect/reconnect/
//! sleep must be durably tracked in `ws_event_audit`. This guard fails the
//! build if a future edit silently drops the audit emission at the lifecycle
//! sites.
//!
//! PR-C2 re-point (2026-07-13): the Dhan live main-feed WS + its pool were
//! DELETED with the lane (operator retirement directive —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment"), so the
//! original main-feed assertions (`record_disconnect` choke point,
//! `WsType::MainFeed` stamping, `connection_pool` audit-channel wiring)
//! retired with the machinery they pinned. The SAME contract still applies
//! to the surviving Dhan WebSocket — the functional-dormant ORDER-UPDATE
//! connection (`ws-event-audit-error-codes.md` §1) — so this guard now pins
//! its lifecycle audit wiring instead. (The Groww bridge's audit wiring is
//! pinned app-side.)

use std::fs;
use std::path::PathBuf;

fn read_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_order_update_disconnect_paths_stamp_audit_rows() {
    // Both the clean-close and error-disconnect arms must stamp a row, and the
    // kind must mirror the in-market vs off-hours Telegram routing.
    let src = read_src("src/websocket/order_update_connection.rs");
    assert!(
        src.contains("WsEventKind::Disconnected")
            && src.contains("WsEventKind::DisconnectedOffHours"),
        "the order-update disconnect audit kind must distinguish in-market vs off-hours."
    );
}

#[test]
fn test_order_update_initial_connect_is_tracked() {
    // 2026-07-05 §1.1 item 3: the order-update WS stamps its initial
    // `Connected` row — not just reconnects — so "every connect" is honestly
    // true for the surviving Dhan WebSocket too.
    let src = read_src("src/websocket/order_update_connection.rs");
    assert!(
        src.contains("WsEventKind::Connected"),
        "the order-update initial connect must stamp a Connected ws_event_audit row."
    );
}

#[test]
fn test_order_update_reconnect_and_sleep_sites_stamp_audit_rows() {
    let src = read_src("src/websocket/order_update_connection.rs");
    for kind in [
        "WsEventKind::Reconnected",
        "WsEventKind::SleepEntered",
        "WsEventKind::SleepResumed",
    ] {
        assert!(
            src.contains(kind),
            "the order-update WS lifecycle wiring must stamp an audit row for {kind}."
        );
    }
    // The emit helper exists and stamps this connection's ws_type.
    assert!(
        src.contains("fn emit_order_update_ws_audit") && src.contains("WsType::OrderUpdate"),
        "emit_order_update_ws_audit must stamp WsType::OrderUpdate."
    );
}
