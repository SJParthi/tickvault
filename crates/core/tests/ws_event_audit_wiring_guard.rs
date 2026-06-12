//! Source-scan ratchet: the WS-event audit producer wiring must stay intact.
//!
//! Operator directive 2026-06-12: every WebSocket connect/disconnect/reconnect/
//! sleep must be durably tracked in `ws_event_audit`, future-proof for a
//! 5+5+5+1 = 16-connection expansion. This guard fails the build if a future
//! edit silently drops the audit emission at the lifecycle sites or breaks the
//! `ws_type`-parameterized pool wiring.

use std::fs;
use std::path::PathBuf;

fn read_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_disconnect_choke_point_stamps_audit_row() {
    // record_disconnect is the SINGLE choke point for all 4 disconnect paths —
    // its emit_ws_audit guarantees EVERY disconnect is tracked. The kind must
    // mirror the in-market vs off-hours Telegram routing.
    let src = read_src("src/websocket/connection.rs");
    assert!(
        src.contains("self.emit_ws_audit(audit_kind"),
        "record_disconnect must stamp a ws_event_audit row (audit_kind passed by \
         the call site so it mirrors the Telegram variant exactly)."
    );
    assert!(
        src.contains("WsEventKind::Disconnected")
            && src.contains("WsEventKind::DisconnectedOffHours"),
        "the disconnect audit kind must distinguish in-market vs off-hours."
    );
}

#[test]
fn test_initial_connect_is_tracked() {
    // B1 fix: the FIRST connect of every connection is tracked too — not just
    // reconnects — so "every connect" is honestly true.
    let src = read_src("src/websocket/connection.rs");
    assert!(
        src.contains("WsEventKind::Connected"),
        "the initial connect must stamp a Connected ws_event_audit row."
    );
}

#[test]
fn test_reconnect_and_sleep_sites_stamp_audit_rows() {
    let src = read_src("src/websocket/connection.rs");
    for kind in [
        "WsEventKind::Reconnected",
        "WsEventKind::SleepEntered",
        "WsEventKind::SleepResumed",
    ] {
        assert!(
            src.contains(kind),
            "the WS lifecycle wiring must stamp an audit row for {kind}."
        );
    }
    // The emit helper exists and stamps this connection's ws_type.
    assert!(
        src.contains("fn emit_ws_audit") && src.contains("WsType::MainFeed"),
        "emit_ws_audit must stamp WsType::MainFeed for the main-feed connection."
    );
}

#[test]
fn test_pool_wires_audit_channel_parameterized_by_pool_size() {
    // The pool must attach the audit channel to every connection AND pass the
    // pool size — this is what makes the 5-main-feed future track correctly
    // (pool_size reflects num_connections), with the SAME code path.
    let src = read_src("src/websocket/connection_pool.rs");
    assert!(
        src.contains("conn.with_ws_audit(")
            && src.contains("num_connections as i64")
            && src.contains("WsType::MainFeed"),
        "the pool must attach the audit channel with pool_size = num_connections \
         AND the connection's ws_type so the future 16-connection scenario is \
         tracked with no code change."
    );
}
