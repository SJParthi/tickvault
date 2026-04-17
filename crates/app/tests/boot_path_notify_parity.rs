//! Mechanical guard: both boot paths (FAST BOOT + slow boot) must call
//! `emit_websocket_connected_alerts` so operators receive identical
//! Telegram notifications regardless of which path ran.
//!
//! Regression: 2026-04-17 — live telegram showed only 3 of 5 `WebSocket #N
//! connected` alerts on slow boot, and zero on FAST BOOT after the 9:04 AM
//! crash. Root cause: the per-connection notify loop lived inline in the
//! slow-boot branch only. The helper + this guard prevent the two paths
//! from diverging again.

#![cfg(test)]

use std::path::PathBuf;

fn main_rs_source() -> String {
    // CARGO_MANIFEST_DIR == crates/app
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = manifest_dir.join("src/main.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

/// Counts non-comment, non-doc call sites of `emit_websocket_connected_alerts`.
/// Treats `//` comments and `///` doc comments as non-callsites.
fn count_real_call_sites(src: &str, needle: &str) -> usize {
    src.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                return false;
            }
            trimmed.contains(needle)
        })
        .filter(|line| line.contains(&format!("{needle}(")))
        .count()
}

#[test]
fn fast_boot_and_slow_boot_both_call_emit_websocket_connected_alerts() {
    let src = main_rs_source();

    // The helper definition is one occurrence. Each call site is another.
    // Definition lives in `async fn emit_websocket_connected_alerts(` — we
    // want to count actual CALL sites, so exclude the fn signature.
    let definition_marker = "async fn emit_websocket_connected_alerts(";
    assert!(
        src.contains(definition_marker),
        "helper `emit_websocket_connected_alerts` must exist in main.rs"
    );

    // Count call sites: `emit_websocket_connected_alerts(` anywhere else.
    // The definition line also contains this substring, so we subtract 1.
    let total_occurrences = count_real_call_sites(&src, "emit_websocket_connected_alerts");
    let call_sites = total_occurrences.saturating_sub(1);

    assert!(
        call_sites >= 2,
        "Both FAST BOOT and slow boot must call `emit_websocket_connected_alerts`. \
         Found {call_sites} call site(s) in main.rs. \
         Regression: 2026-04-17 live telegram showed FAST BOOT emits zero \
         `WebSocketConnected` alerts. Fix by calling the helper from both \
         boot paths. See .claude/plans/active-plan.md (Fix A)."
    );
}

#[test]
fn emit_websocket_connected_alerts_helper_is_defined() {
    let src = main_rs_source();
    assert!(
        src.contains("async fn emit_websocket_connected_alerts("),
        "helper `async fn emit_websocket_connected_alerts` must exist in main.rs \
         so that both boot paths can share the same notify semantics."
    );
}

#[test]
fn summary_event_emitted_after_per_connection_loop() {
    let src = main_rs_source();
    // Inside the helper body, the summary event must follow the loop.
    let helper_start = src
        .find("async fn emit_websocket_connected_alerts(")
        .expect("helper must exist");
    // Grab a generous window of the helper body.
    let end = helper_start.saturating_add(4000).min(src.len());
    let body = &src[helper_start..end];
    let loop_idx = body
        .find("NotificationEvent::WebSocketConnected")
        .expect("helper must emit per-connection events");
    let summary_idx = body.find("NotificationEvent::WebSocketPoolOnline").expect(
        "helper must emit a WebSocketPoolOnline summary event so operators \
         survive Telegram rate-limit drops of individual events",
    );
    assert!(
        summary_idx > loop_idx,
        "`WebSocketPoolOnline` summary must be emitted AFTER the per-connection \
         loop so the aggregate reflects the full attempt."
    );
}
