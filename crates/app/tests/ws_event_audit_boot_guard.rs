//! Source-scan ratchet: the WS-event audit consumer must stay wired into boot.
//!
//! Operator directive 2026-06-12: every WebSocket lifecycle event must be
//! durably tracked. The producer side (core connections) stamps rows; the app
//! must own the consumer that ensures the table + drains the channel + emits
//! AUDIT-WS-01 on failure. This guard fails the build if that boot wiring is
//! silently removed.

use std::fs;
use std::path::PathBuf;

fn main_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_ws_event_audit_consumer_is_spawned_at_boot() {
    let src = main_src();
    // The consumer is spawned via the shared helper, which both the main-feed
    // pool AND the order-update connection reuse.
    assert!(
        src.contains("fn spawn_ws_event_audit_consumer(")
            && src.contains("run_ws_event_audit_consumer(rx, questdb_cfg)"),
        "spawn_ws_event_audit_consumer helper must create the channel + spawn the consumer."
    );
    assert!(
        src.contains("Some(ws_audit_tx)"),
        "the audit channel sender must be passed into the WebSocket pool."
    );
}

#[test]
fn test_order_update_connection_is_audit_wired() {
    // The order-update connection must also stamp ws_event_audit rows — it
    // creates its own consumer via the shared helper and passes the sender.
    let src = main_src();
    let helper_calls = src
        .matches("spawn_ws_event_audit_consumer(config.questdb.clone())")
        .count();
    assert!(
        helper_calls >= 2,
        "the order-update spawn site(s) must call spawn_ws_event_audit_consumer too \
         (found {helper_calls} call(s); expected the pool + at least one order-update site)."
    );
    assert!(
        src.contains("ord_ws_audit_tx") || src.contains("ou_ws_audit_tx"),
        "the order-update spawn must pass an audit sender into run_order_update_connection."
    );
}

#[test]
fn test_ws_event_audit_consumer_ensures_table_and_emits_error_code() {
    let src = main_src();
    assert!(
        src.contains("ensure_ws_event_audit_table(&questdb_cfg).await"),
        "the consumer must ensure the ws_event_audit table at start."
    );
    assert!(
        src.contains("ErrorCode::AuditWs01EventWriteFailed.code_str()"),
        "a flush/append failure must emit the AUDIT-WS-01 code."
    );
}
