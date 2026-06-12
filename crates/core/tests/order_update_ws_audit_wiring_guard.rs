//! Source-scan ratchet: the order-update WebSocket must stamp ws_event_audit
//! rows for every lifecycle event (completes the WS-tracking goal — the
//! order-confirmation line is the OTHER live socket after the main feed).
//!
//! Operator directive 2026-06-12: "everything should be entirely logged
//! monitored tracked captured" for every WebSocket. This guard fails the build
//! if the order-update audit emission is silently dropped.

use std::fs;
use std::path::PathBuf;

fn src() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/websocket/order_update_connection.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_order_update_has_audit_emit_helper_with_order_update_ws_type() {
    let s = src();
    assert!(
        s.contains("fn emit_order_update_ws_audit"),
        "order_update_connection must define emit_order_update_ws_audit."
    );
    assert!(
        s.contains("WsType::OrderUpdate"),
        "the order-update audit row must stamp WsType::OrderUpdate."
    );
}

#[test]
fn test_order_update_stamps_all_four_lifecycle_kinds() {
    let s = src();
    for kind in [
        "WsEventKind::Disconnected",
        "WsEventKind::DisconnectedOffHours",
        "WsEventKind::Reconnected",
        "WsEventKind::SleepEntered",
        "WsEventKind::SleepResumed",
    ] {
        assert!(
            s.contains(kind),
            "the order-update wiring must stamp an audit row for {kind}."
        );
    }
}

#[test]
fn test_order_update_audit_channel_is_threaded_through() {
    // The audit sender must reach the disconnect (run loop), reconnect
    // (connect_and_listen) and sleep (post_close_sleep) emit points.
    let s = src();
    let emit_calls = s.matches("emit_order_update_ws_audit(").count();
    assert!(
        emit_calls >= 4,
        "expected >= 4 emit_order_update_ws_audit call sites (disconnect + reconnect + \
         sleep-entered + sleep-resumed); found {emit_calls}."
    );
    assert!(
        s.contains("ws_audit_tx"),
        "ws_audit_tx must be threaded through the order-update connection functions."
    );
}
