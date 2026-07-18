//! Source-scan ratchet: the WS-event audit consumer must stay wired into boot.
//!
//! Operator directive 2026-06-12: every WebSocket lifecycle event must be
//! durably tracked. The producer side (core connections) stamps rows; the app
//! must own the consumer that ensures the table + drains the channel + emits
//! AUDIT-WS-01 on failure. This guard fails the build if that boot wiring is
//! silently removed.

use std::fs;
use std::path::PathBuf;

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

// PR-C1 round-2 (2026-07-13): renamed to carry the relocated helper's full fn
// name (`spawn_ws_event_audit_consumer`) so the pub-fn-test-guard name
// heuristic keeps pinning it after the main.rs → lib module move.
#[test]
fn test_spawn_ws_event_audit_consumer_wired_and_moved_intact() {
    // The consumer is spawned via the shared helper, which the main-feed
    // pool, the legacy order-update spawn sites AND the dhan_rest_stack
    // rewire site (PR-C1, 2026-07-13) all reuse.
    let module = consumer_module_src();
    assert!(
        module.contains("fn spawn_ws_event_audit_consumer(")
            && module.contains("run_ws_event_audit_consumer(rx, questdb_cfg)"),
        "spawn_ws_event_audit_consumer helper must create the channel + spawn the consumer."
    );
    // 2026-07-15 (Groww live-feed deletion): the Groww bridge / fleet /
    // sidecar-supervisor producer sites — the last main.rs consumers of the
    // helper — died with the lanes, so main.rs no longer imports it. The
    // helper module is RETAINED (the scoreboard still reads ws_event_audit;
    // a future producer re-wires through the same helper) and its internals
    // stay pinned above + in the table/error-code test below.
}

#[test]
fn test_order_update_connection_is_audit_wired() {
    // 2026-07-16 (the order-update rewire — operator directive, governance
    // on PR #1597; FLIPS the 2026-07-14 negative ratchet): the
    // dhan_rest_stack Phase 5a MUST spawn the order-update WS again — as
    // the PAPER-MODE push channel, gated on `[dhan_order_push] enabled`
    // (default OFF) and Telegram-silent (the `None` notifier binding — the
    // 2026-07-14 Dhan noise lock still owns the page surface). The ws
    // lifecycle audit sender is honestly `None` at this site today (the
    // shared consumer helper is not cheaply reachable in the stack —
    // wiring it is a flagged follow-up); the in-file dhan_rest_stack
    // ratchet (`test_rest_stack_order_update_push_gated_and_no_canary`)
    // pins the exactly-once spawn count.
    let stack = rest_stack_src();
    let stack_prod = stack
        .split_once("#[cfg(test)]")
        .map(|(prod, _)| prod.to_string())
        .unwrap_or(stack);
    assert!(
        stack_prod.contains("run_order_update_connection("),
        "dhan_rest_stack must spawn the order-update WS as the config-gated \
         paper-mode push channel (2026-07-16 rewire — \
         .claude/plans/active-plan-dhan-order-update-rewire.md)."
    );
    assert!(
        stack_prod.contains("if config.dhan_order_push.enabled {"),
        "the order-update WS spawn must be gated on [dhan_order_push] enabled \
         (default OFF — a disabled boot must stay byte-identical)."
    );
    assert!(
        stack_prod.contains("let order_push_notifier: Option<Arc<NotificationService>> = None;"),
        "the order-update WS spawn must stay Telegram-silent — the notifier \
         arg is a NAMED None binding (2026-07-14 Dhan noise lock)."
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
