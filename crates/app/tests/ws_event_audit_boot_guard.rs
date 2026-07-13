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

/// Phase C1 (2026-07-13): the consumer helper relocated from the main.rs
/// binary to the lib module so `dhan_rest_stack` (the Q4-i order-update
/// rewire home) can create its own consumer — the internals pins now scan
/// the module's source.
fn consumer_module_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ws_audit_consumer.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn rest_stack_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_rest_stack.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_ws_event_audit_consumer_is_spawned_at_boot() {
    // The consumer is spawned via the shared helper, which the main-feed
    // pool, the legacy order-update spawn sites AND the dhan_rest_stack
    // rewire site (PR-C1, 2026-07-13) all reuse.
    let module = consumer_module_src();
    assert!(
        module.contains("fn spawn_ws_event_audit_consumer(")
            && module.contains("run_ws_event_audit_consumer(rx, questdb_cfg)"),
        "spawn_ws_event_audit_consumer helper must create the channel + spawn the consumer."
    );
    let src = main_src();
    assert!(
        src.contains("use tickvault_app::ws_audit_consumer::spawn_ws_event_audit_consumer;"),
        "main.rs must re-import the relocated consumer helper."
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
    // PR-C1 (2026-07-13, Q4-i): the dhan_rest_stack rewire site must ALSO
    // create its own consumer and pass the sender into the connection —
    // the stack becomes the SOLE order-update call site after Phase C2.
    let stack = rest_stack_src();
    assert!(
        stack.contains("ws_audit_consumer::spawn_ws_event_audit_consumer("),
        "dhan_rest_stack must create a ws_event_audit consumer for its order-update WS."
    );
    assert!(
        stack.contains("ou_ws_audit_tx"),
        "dhan_rest_stack must pass the audit sender into run_order_update_connection."
    );
}

#[test]
fn test_ws_event_audit_consumer_ensures_table_and_emits_error_code() {
    let module = consumer_module_src();
    assert!(
        module.contains("ensure_ws_event_audit_table(&questdb_cfg).await"),
        "the consumer must ensure the ws_event_audit table at start."
    );
    assert!(
        module.contains("ErrorCode::AuditWs01EventWriteFailed.code_str()"),
        "a flush/append failure must emit the AUDIT-WS-01 code."
    );
}
