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
fn summary_event_emitted_inside_helper() {
    // 2026-05-09: per-connection `WebSocketConnected` emission was removed
    // (it duplicated `WebSocketPoolOnline` and produced two Telegrams ~60s
    // apart at boot — see fn docstring of `emit_websocket_connected_alerts`).
    // The aggregate `WebSocketPoolOnline` (Severity::Medium) is now the
    // single source of truth for boot success.
    let src = main_rs_source();
    let helper_start = src
        .find("async fn emit_websocket_connected_alerts(")
        .expect("helper must exist");
    let end = helper_start.saturating_add(4000).min(src.len());
    let body = &src[helper_start..end];
    assert!(
        body.contains("NotificationEvent::WebSocketPoolOnline"),
        "helper must emit `WebSocketPoolOnline` aggregate event so operators \
         get a single non-duplicate boot-complete signal"
    );
}

/// 2026-05-09 ratchet — block a regression that re-introduces per-connection
/// `WebSocketConnected` emission inside the boot helper. The aggregate
/// `WebSocketPoolOnline` (which carries the same per-feed breakdown in its
/// `per_connection` payload) is the single source of truth for boot success.
/// Re-introducing per-connection emission resurrects the duplicate Telegram
/// bug observed on 2026-05-09 (12:07 PM "TickVault is live" + 12:08 PM
/// "WebSocketConnected x5" coalesced summary).
#[test]
fn per_connection_websocket_connected_must_not_be_emitted_in_boot_helper() {
    let src = main_rs_source();
    let helper_start = src
        .find("async fn emit_websocket_connected_alerts(")
        .expect("helper must exist");
    // Find the END of the helper by locating the next `async fn` or `fn` at
    // module scope after the helper opens. Fall back to a generous window.
    let after_open = &src[helper_start..];
    // The helper body should fit in well under 6000 chars; take that.
    let end_offset = 6000.min(after_open.len());
    let body = &after_open[..end_offset];

    // It is fine for the body to mention `WebSocketConnected` in a comment
    // (e.g., the rationale in the docstring). Reject only `notify(...)`-shaped
    // call sites.
    let banned_patterns = [
        "notify(NotificationEvent::WebSocketConnected",
        "notify(tickvault_core::notification::events::NotificationEvent::WebSocketConnected",
    ];
    for pat in &banned_patterns {
        assert!(
            !body.contains(pat),
            "regression: `emit_websocket_connected_alerts` must NOT emit \
             per-connection `WebSocketConnected` events. The aggregate \
             `WebSocketPoolOnline` covers this signal. Two Telegrams at boot \
             (12:07 'TickVault is live' + 12:08 'WebSocketConnected x5') \
             confused operators on 2026-05-09; this guard prevents the \
             regression. If you NEED per-connection visibility, emit a new \
             dedicated variant — do NOT reuse `WebSocketConnected` (which \
             stays Low / coalesced for post-boot use cases). Banned pattern: \
             `{pat}`."
        );
    }
}
