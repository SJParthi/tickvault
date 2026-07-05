//! Source-scan ratchet — ws_event_audit loud drops (2026-07-05).
//!
//! The operator caught `ws_event_audit` EMPTY on 2026-07-05 (silent false-OK
//! class, audit-findings Rule 11): the producer `try_send` drop arms logged at
//! `debug!` with no counter, so dropped forensic rows were invisible. This
//! guard fails the build if any core drop arm regresses to a silent drop, or
//! if the order-update initial-`Connected` emit is removed.

use std::fs;
use std::path::PathBuf;

fn read_ws_src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/websocket")
        .join(file);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every `row dropped` drop-arm marker must sit in a region that logs at
/// `error!` (with the AUDIT-WS-01 code) and contains NO `debug!`.
fn assert_drop_arms_loud(src: &str, file: &str) {
    let lines: Vec<&str> = src.lines().collect();
    let mut markers = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("row dropped") {
            continue;
        }
        markers += 1;
        let start = i.saturating_sub(12);
        let end = (i + 4).min(lines.len());
        // CODE lines only — `//` comments may legitimately mention `debug!`
        // (e.g. "upgraded from debug!"); only executable regressions fail.
        let region = lines[start..end]
            .iter()
            .filter(|l| !l.trim_start().starts_with("//"))
            .copied()
            .collect::<Vec<&str>>()
            .join("\n");
        assert!(
            region.contains("error!"),
            "{file}: drop arm near line {} must log at error! (AUDIT-WS-01) — \
             silent drops are the 2026-07-05 regression class",
            i + 1
        );
        assert!(
            !region.contains("debug!"),
            "{file}: drop arm near line {} regressed to debug! — dropped \
             forensic rows must be LOUD",
            i + 1
        );
        assert!(
            region.contains("AuditWs01EventWriteFailed"),
            "{file}: drop arm near line {} must carry code = AUDIT-WS-01",
            i + 1
        );
    }
    assert!(
        markers >= 1,
        "{file}: expected at least one ws_event_audit drop-arm marker"
    );
    assert!(
        src.contains("tv_ws_event_audit_dropped_total"),
        "{file}: the drop arm must increment tv_ws_event_audit_dropped_total{{reason}}"
    );
}

#[test]
fn test_main_feed_drop_arm_is_loud() {
    assert_drop_arms_loud(&read_ws_src("connection.rs"), "connection.rs");
}

#[test]
fn test_order_update_drop_arm_is_loud() {
    assert_drop_arms_loud(
        &read_ws_src("order_update_connection.rs"),
        "order_update_connection.rs",
    );
}

#[test]
fn test_order_update_connected_emit_is_wired() {
    // 2026-07-05: the order-update WS must stamp an initial `Connected`
    // forensic row on a non-failure-recovery connect (mirrors the main-feed
    // B1 initial-connect emit). Removing it leaves "Order Update WS connected"
    // with no ws_event_audit row.
    let src = read_ws_src("order_update_connection.rs");
    assert!(
        src.contains("WsEventKind::Connected"),
        "order_update_connection.rs must emit WsEventKind::Connected on a \
         successful non-recovery connect"
    );
    assert!(
        src.contains("order-update initial connect"),
        "the Connected emit's reason string must identify the initial-connect episode"
    );
}
