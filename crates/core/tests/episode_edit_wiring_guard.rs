//! Telegram UX Overhaul (2026-07-07) — episode live-edit wiring guard.
//!
//! Build-failing source scans pinning the episode dispatch contract in
//! `crates/core/src/notification/service.rs` + `episode.rs`:
//!
//! (a) `notify()` consults `episode_key()` BEFORE the coalescer branch
//! (b) the editMessageText URL is built in exactly ONE site in service.rs
//! (c) the edit transport has exactly the two blessed callers
//!     (`dispatch_episode_event` for live edits + `run_episode_tick` for
//!     the green close) — nothing else may edit Telegram messages
//! (d) the `EPISODE_*` constants stay pinned at their contract values
//! (e) no `warn!` in any edit/send/store failure arm (error-level law)
//! (f) terminal delivery failure keeps the TELEGRAM-01 `error!` + counter
//! (g) the ws_event_audit choke points are untouched by the episode work
//!
//! House pattern: source-order scans (see
//! `crates/app/tests/ws_rate_limit_cooldown_wiring_guard.rs`).

use std::fs;

fn service_src() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/notification/service.rs"
    ))
    .expect("service.rs must exist")
}

fn episode_src() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/notification/episode.rs"
    ))
    .expect("episode.rs must exist")
}

/// Extracts the region of `src` between `start` (inclusive) and the next
/// occurrence of `end` after it (exclusive). Panics loudly when a marker
/// is missing so a rename fails the build instead of vacuously passing.
fn region<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let s = src
        .find(start)
        .unwrap_or_else(|| panic!("region start marker missing: {start}"));
    let rest = &src[s..];
    let e = rest
        .find(end)
        .unwrap_or_else(|| panic!("region end marker missing: {end}"));
    &rest[..e]
}

// (a) notify() consults episode_key BEFORE the coalescer branch.
#[test]
fn guard_a_notify_consults_episode_key_before_coalescer() {
    let src = service_src();
    let notify_region = region(&src, "pub fn notify(", "pub fn is_active(");
    let episode_idx = notify_region
        .find("event.episode_key()")
        .expect("notify() must consult episode_key()");
    let coalescer_idx = notify_region
        .find("self.coalescer.as_ref()")
        .expect("notify() must still hold the coalescer branch");
    assert!(
        episode_idx < coalescer_idx,
        "episode_key() consultation must come BEFORE the coalescer branch in notify()"
    );
    // The episode lane must short-circuit (return) so an episode event can
    // never double-dispatch through the legacy path.
    let between = &notify_region[episode_idx..coalescer_idx];
    assert!(
        between.contains("return;"),
        "the episode lane in notify() must return before the coalescer branch"
    );
}

// (b) editMessageText URL built in exactly ONE site in service.rs, and
// nowhere else in the crate's production sources.
#[test]
fn guard_b_edit_message_text_single_build_site() {
    let src = service_src();
    let url_builds = src.matches("/editMessageText").count();
    // One URL format! site + doc/test mentions are excluded by scanning
    // for the URL-path literal only (tests use the mock's request path —
    // also counted here, so pin the PRODUCTION region: before the tests).
    let prod = region(&src, "//! Notification service", "#[cfg(test)]");
    assert_eq!(
        prod.matches("/editMessageText").count(),
        1,
        "exactly ONE editMessageText URL build site in production service.rs \
         (found {url_builds} total incl. tests)"
    );
    // No other production file in the crate touches the edit method.
    // Allowlist: service.rs (the transport) + episode.rs (the literal
    // appears ONLY inside its commandments banned-strings ratchet test).
    let src_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut offenders = Vec::new();
    scan_tree_for(src_dir, "editMessageText", &mut offenders);
    assert_eq!(
        offenders,
        vec![
            format!("{src_dir}/notification/episode.rs"),
            format!("{src_dir}/notification/service.rs"),
        ],
        "editMessageText must live ONLY in the notification transport (+ the \
         episode.rs banned-strings ratchet literal)"
    );
    let episode = episode_src();
    let episode_tests = region(&episode, "#[cfg(test)]", "\n}\n");
    assert!(
        episode_tests.contains("editMessageText"),
        "episode.rs may mention editMessageText ONLY inside its test module"
    );
    let episode_prod = region(&episode, "//! Telegram UX Overhaul", "#[cfg(test)]");
    assert!(
        !episode_prod.contains("editMessageText"),
        "episode.rs production region must not touch the edit transport"
    );
}

fn scan_tree_for(dir: &str, needle: &str, hits: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(p) = path.to_str() {
                scan_tree_for(p, needle, hits);
            }
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(content) = fs::read_to_string(&path)
            && content.contains(needle)
            && let Some(p) = path.to_str()
        {
            hits.push(p.to_string());
        }
    }
    hits.sort();
}

// (c) the edit transport has exactly the blessed callers.
#[test]
fn guard_c_edit_transport_blessed_callers_only() {
    let src = service_src();
    let prod = region(&src, "//! Notification service", "#[cfg(test)]");
    // Occurrences: 1 definition + 1 call in dispatch_episode_event (live
    // edits) + 1 call in run_episode_tick (the green close edit — no event
    // triggers it, so it cannot live inside dispatch_episode_event).
    assert_eq!(
        prod.matches("edit_telegram_message_with_retry(").count(),
        3,
        "edit transport call sites drifted — blessed callers are \
         dispatch_episode_event + run_episode_tick only"
    );
    let dispatch = region(
        prod,
        "async fn dispatch_episode_event",
        "async fn episode_fallback_send",
    );
    assert!(
        dispatch.contains("edit_telegram_message_with_retry("),
        "dispatch_episode_event must own the live-edit call"
    );
    let tick = region(
        prod,
        "pub(crate) async fn run_episode_tick",
        "async fn deliver_drained",
    );
    assert!(
        tick.contains("edit_telegram_message_with_retry("),
        "run_episode_tick must own the green-close edit call"
    );
}

// (d) EPISODE_* constants pinned at the contract values.
#[test]
fn guard_d_episode_constants_pinned() {
    let src = episode_src();
    for pin in [
        "pub const EPISODE_EDIT_MIN_INTERVAL_SECS: u64 = 20;",
        "pub const EPISODE_STABILITY_SECS: u64 = 60;",
        "pub const EPISODE_REOPEN_SECS: u64 = 120;",
        "pub const EPISODE_STEADY_MAX_CHARS: usize = 320;",
        "pub const EPISODE_STEADY_MAX_LINES: usize = 3;",
        "pub const EPISODE_FIRST_PAGE_MAX_CHARS: usize = 3800;",
        "pub const EPISODE_REHYDRATE_MAX_AGE_SECS: u64 = 7200;",
        "pub const EPISODE_EDIT_FAILURES_FALLBACK_THRESHOLD: u8 = 2;",
    ] {
        assert!(
            src.contains(pin),
            "episode constant drifted from the 2026-07-07 contract: {pin}"
        );
    }
    // The first-page ceiling must equal the Telegram chunk limit so the
    // first page is ALWAYS one chunk.
    let service = service_src();
    assert!(
        service.contains("pub(crate) const TELEGRAM_CHUNK_LIMIT_CHARS: usize = 3800;"),
        "TELEGRAM_CHUNK_LIMIT_CHARS must stay 3800 (== EPISODE_FIRST_PAGE_MAX_CHARS)"
    );
}

// (e) no warn! in any edit/send/store failure arm — flush/persist/send
// failures are error!-level law (error_level_meta_guard companion).
#[test]
fn guard_e_no_warn_in_episode_failure_arms() {
    let src = service_src();
    for (start, end) in [
        (
            "async fn dispatch_episode_event",
            "async fn episode_fallback_send",
        ),
        ("async fn episode_fallback_send", "// ---"),
        (
            "async fn write_episode_store",
            "async fn rehydrate_episodes",
        ),
        (
            "async fn rehydrate_episodes",
            "pub(crate) async fn run_episode_tick",
        ),
        (
            "pub(crate) async fn run_episode_tick",
            "async fn deliver_drained",
        ),
        ("async fn deliver_drained", "// ---"),
    ] {
        let r = region(&src, start, end);
        assert!(
            !r.contains("warn!"),
            "warn! found in episode failure region starting {start:?} — \
             persist/send/edit failures must use error! with a code field"
        );
    }
}

// (f) terminal delivery failure keeps TELEGRAM-01 loudness.
#[test]
fn guard_f_telegram01_retained_on_terminal_failure() {
    let src = service_src();
    let dispatch = region(
        &src,
        "async fn dispatch_episode_event",
        "async fn episode_fallback_send",
    );
    assert!(
        dispatch.contains("Telegram01Dropped"),
        "dispatch_episode_event terminal send failure must keep the TELEGRAM-01 error!"
    );
    assert!(
        dispatch.contains(r#""reason" => "send_failed""#),
        "dispatch_episode_event must keep tv_telegram_dropped_total{{send_failed}}"
    );
    let fallback = region(&src, "async fn episode_fallback_send", "// ---");
    assert!(
        fallback.contains("Telegram01Dropped"),
        "episode_fallback_send terminal failure must keep the TELEGRAM-01 error!"
    );
}

// (g) ws_event_audit choke points untouched by the episode machinery.
#[test]
fn guard_g_ws_audit_choke_points_untouched() {
    let connection = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/websocket/connection.rs"
    ))
    .expect("connection.rs must exist");
    let order_update = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/websocket/order_update_connection.rs"
    ))
    .expect("order_update_connection.rs must exist");
    // The audit emit choke points still exist…
    assert!(
        connection.contains("emit_ws_audit("),
        "main-feed ws_event_audit choke point missing"
    );
    assert!(
        order_update.contains("emit_order_update_ws_audit("),
        "order-update ws_event_audit choke point missing"
    );
    // …and the episode machinery never reached into them (the bubbles are
    // a Telegram-surface concern; the forensic record is untouched).
    // The scan targets the episode-module identifiers — the plain English
    // word "episode" legitimately appears in WS-GAP-10 comments.
    for (name, src) in [
        ("connection.rs", &connection),
        ("order_update_connection.rs", &order_update),
    ] {
        for ident in [
            "episode_key",
            "EpisodeKey",
            "EpisodeRegistry",
            "EpisodeAction",
            "notification::episode",
            "dispatch_episode_event",
        ] {
            assert!(
                !src.contains(ident),
                "{name} must not reference the episode machinery ({ident})"
            );
        }
    }
}

// Bonus pin: the episode core imports ONLY the notification Severity —
// never the error_code::Severity twin (duplicate-enum confusion guard).
#[test]
fn guard_episode_core_uses_notification_severity_only() {
    let src = episode_src();
    assert!(
        src.contains("use super::events::Severity;"),
        "episode.rs must import notification::events::Severity"
    );
    assert!(
        !src.contains("error_code::Severity"),
        "episode.rs must NOT import the error_code Severity twin"
    );
    // Purity pin: the pure core never reads a clock.
    assert!(
        !src.contains("SystemTime::now") && !src.contains("Utc::now"),
        "episode.rs is a pure core — no clock reads (now_ms is injected)"
    );
}

// Bonus pin: the SNS-SMS leg rides ONLY the SendFirstPage arm — SMS
// exactly once per episode open, never on edits/fallbacks.
#[test]
fn guard_sms_leg_rides_first_page_only() {
    let src = service_src();
    let dispatch = region(
        &src,
        "async fn dispatch_episode_event",
        "async fn episode_fallback_send",
    );
    assert_eq!(
        dispatch.matches("send_sns_sms(").count(),
        1,
        "exactly one SMS send site in dispatch_episode_event"
    );
    let first_page_arm = region(
        dispatch,
        "EpisodeAction::SendFirstPage",
        "EpisodeAction::EditThrottled",
    );
    assert!(
        first_page_arm.contains("send_sns_sms("),
        "the SMS leg must live inside the SendFirstPage arm"
    );
}
