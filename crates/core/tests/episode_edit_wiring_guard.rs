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
    // 2026-07-09 boot bubble: the gate grew a boot_bubble refinement and
    // rustfmt splits the receiver — scan for the method call itself.
    let episode_idx = notify_region
        .find(".episode_key()")
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
    // Boot bubble (2026-07-09): the gate lets the Boot family through ONLY
    // while boot_bubble is on — the rollback path must stay legacy while
    // the WS families keep the episode lane unconditionally.
    assert!(
        between.contains("EpisodeFamily::Boot || self.boot_bubble"),
        "notify() must gate the Boot family on the boot_bubble kill switch"
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
    // triggers it, so it cannot live inside dispatch_episode_event)
    // + 2026-07-09 boot bubble: 1 call in dispatch_boot_episode_event
    // (checklist live edits) + 1 call in run_boot_tick (the dirty
    // re-drive inside the drain ticker).
    assert_eq!(
        prod.matches("edit_telegram_message_with_retry(").count(),
        5,
        "edit transport call sites drifted — blessed callers are \
         dispatch_episode_event + run_episode_tick + \
         dispatch_boot_episode_event + run_boot_tick only"
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
    let boot_dispatch = region(
        prod,
        "async fn dispatch_boot_episode_event",
        "const fn key_boot(",
    );
    assert!(
        boot_dispatch.contains("edit_telegram_message_with_retry("),
        "dispatch_boot_episode_event must own the boot live-edit call"
    );
    // The whole boot fold→render→send/edit body is serialized — two
    // milestones racing the in-flight first send must never open a
    // duplicate bubble (2026-07-09 correctness graft).
    assert!(
        boot_dispatch.contains("boot_dispatch_lock.lock().await"),
        "dispatch_boot_episode_event must hold the boot serialization lock"
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
    let boot_tick = region(prod, "async fn run_boot_tick", "async fn deliver_drained");
    assert!(
        boot_tick.contains("boot_lock.lock().await"),
        "run_boot_tick must share the boot serialization lock"
    );
    assert!(
        boot_tick.contains("retire_boot("),
        "run_boot_tick must own the silent boot retirement"
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
        // Hostile-review fix 2026-07-07: stale-Down expiry bound (the
        // restart edge — a rehydrated Down bubble must never live forever).
        "pub const EPISODE_DOWN_STALE_EXPIRE_SECS: u64 = 1800;",
        // Boot bubble (2026-07-09): render ceiling + silent retire window.
        "pub const BOOT_BUBBLE_MAX_CHARS: usize = 1200;",
        "pub const BOOT_BUBBLE_RETIRE_SECS: u64 = 900;",
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
        // Boot bubble (2026-07-09) regions.
        ("async fn dispatch_boot_episode_event", "const fn key_boot("),
        ("async fn run_boot_tick", "async fn deliver_drained"),
        (
            "async fn init_boot_sha_flavor",
            "async fn rehydrate_episodes",
        ),
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
    // PR-C2 re-shape (2026-07-13): the main-feed `connection.rs` choke point
    // (`emit_ws_audit`) was DELETED with the Dhan live-WS lane (operator
    // retirement directive — websocket-connection-scope-lock.md "2026-07-13
    // Amendment" §B); the order-update choke point is the surviving core-side
    // ws_event_audit producer this guard pins.
    let order_update = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/websocket/order_update_connection.rs"
    ))
    .expect("order_update_connection.rs must exist");
    // The audit emit choke point still exists…
    assert!(
        order_update.contains("emit_order_update_ws_audit("),
        "order-update ws_event_audit choke point missing"
    );
    // …and the episode machinery never reached into it (the bubbles are
    // a Telegram-surface concern; the forensic record is untouched).
    // The scan targets the episode-module identifiers — the plain English
    // word "episode" legitimately appears in WS-GAP-10 comments.
    for (name, src) in [("order_update_connection.rs", &order_update)] {
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
// exactly once per episode OPEN and once per Low→High ESCALATION (the
// escalation edge routes SendFirstPage through the FSM, hostile-review
// fix 2026-07-07), never on edits/fallbacks.
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

// Escalation pin (hostile-review fix 2026-07-07): the pure FSM must route
// a High/Critical event folding into a sub-High episode (live state OR a
// fresh tombstone) to a FRESH first page — a silent edit produces no
// Telegram push and no SMS for an in-market HIGH outage.
#[test]
fn guard_escalation_edge_reaches_first_page_bypass() {
    let episode = episode_src();
    let prod = region(&episode, "//! Telegram UX Overhaul", "#[cfg(test)]");
    let fsm = region(
        prod,
        "pub fn next_episode_action",
        "pub struct EpisodeDecision",
    );
    assert!(
        fsm.contains("st.severity_peak < Severity::High"),
        "the live-state escalation edge must consult severity_peak"
    );
    assert!(
        fsm.contains("t.severity_peak < Severity::High"),
        "the tombstone-reopen escalation edge must consult the tombstone peak"
    );
    // The sub-threshold transient edit failure must never be silent
    // (counted + error!-loud), and High/Critical must fall back on the
    // very first exhausted transient.
    let src = service_src();
    let dispatch = region(
        &src,
        "async fn dispatch_episode_event",
        "async fn episode_fallback_send",
    );
    assert!(
        dispatch.contains("decision.state.severity_peak >= Severity::High"),
        "HIGH episodes must fall back on the first exhausted transient"
    );
    assert!(
        dispatch.contains(r#"reason = "edit_transient_deferred""#),
        "sub-threshold transient must stay error!-loud (TELEGRAM-03)"
    );
    assert!(
        dispatch.contains(r#""reason" => "transient_deferred""#),
        "sub-threshold transient must be counted"
    );
}

// Stale-Down expiry pin (hostile-review fix 2026-07-07): the registry
// tick must expire event-less Down episodes (restart edge) and the shell
// must neutralize the old bubble; a Resolve with no state routes to the
// legacy lane instead of a silent drop.
#[test]
fn guard_stale_expiry_and_legacy_resolve_wired() {
    let episode = episode_src();
    let prod = region(&episode, "//! Telegram UX Overhaul", "#[cfg(test)]");
    assert!(
        prod.contains("down_stale_expire_secs"),
        "EpisodeConfig must carry the stale-Down expiry bound"
    );
    assert!(
        prod.contains("EpisodeAction::SendLegacy"),
        "the FSM must route untracked recoveries to the legacy lane"
    );
    let src = service_src();
    let tick = region(
        &src,
        "pub(crate) async fn run_episode_tick",
        "async fn deliver_drained",
    );
    assert!(
        tick.contains("render_episode_stale_closed("),
        "run_episode_tick must neutralize expired stale bubbles"
    );
    let dispatch = region(
        &src,
        "async fn dispatch_episode_event",
        "async fn episode_fallback_send",
    );
    assert!(
        dispatch.contains("EpisodeAction::SendLegacy =>"),
        "dispatch must deliver SendLegacy through the legacy immediate lane"
    );
    assert!(
        dispatch.contains(r#""action" => "legacy_passthrough""#),
        "the legacy passthrough must be counted"
    );
}
