//! Source-scan ratchet — Groww feed-DOWN Telegram + liveness gauges
//! (operator directive 2026-07-06).
//!
//! Incident class: the Groww feed's disconnect falling edges (runtime
//! disable / bridge death) wrote ONLY a `ws_event_audit` DB row + logs —
//! `notifier.notify(` appeared exactly once in groww_bridge.rs (the
//! boot-connect rising edge), so a mid-market feed death paged NOTHING
//! (silent false-OK class, audit-findings Rule 11; the 2026-06-30
//! "Groww stopped at 10:31 IST and never recovered" incident had no
//! typed Telegram surface). This guard fails the build if:
//!
//!  1. the feed-disable falling edge stops emitting `FeedDown`,
//!  2. the bridge-death falling edge stops emitting `FeedDown`,
//!  3. the streaming rising edge stops emitting the honest `FeedRecovered`
//!     (recovery claimed on ticks, never socket-connect),
//!  4. the per-episode `down_announced` latch usage disappears (page spam
//!     or page loss — audit-findings Rule 4, edge-trigger only),
//!  5. the `tv_groww_ws_active` / `tv_feed_last_tick_age_seconds{feed=
//!     "groww"}` gauges stop being published from the bridge loop,
//!  6. the cw-agent metric selector stops shipping the new metric names,
//!  7. any `FeedDown` emit leaks into the Dhan main-feed connection code
//!     (Dhan blast radius must stay ZERO — the Dhan lanes keep their own
//!     WebSocketDisconnected/... semantics untouched).

use std::fs;
use std::path::PathBuf;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The PRODUCTION region of a source file — everything before the TOP-LEVEL
/// `#[cfg(test)] mod tests` module (column-0 attribute; an inner
/// `#[cfg(test)]` helper mid-file must NOT truncate the scan). Scanning the
/// whole file would match marker literals inside the unit-test module
/// (vacuous/false-OK class; same split the shadow-writer ratchet hardening
/// used).
fn production_region(src: &str) -> &str {
    match src.find("\n#[cfg(test)]\nmod tests") {
        Some(at) => &src[..at],
        None => src,
    }
}

/// Comment-stripped window around each occurrence of `marker` in the
/// production region: `before` lines above and `after` lines below the
/// marker line, CODE lines only.
fn marker_windows(src: &str, marker: &str, before: usize, after: usize) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut windows = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(marker) {
            continue;
        }
        let start = i.saturating_sub(before);
        let end = (i + after).min(lines.len());
        let region = lines[start..end]
            .iter()
            .filter(|l| !l.trim_start().starts_with("//"))
            .copied()
            .collect::<Vec<&str>>()
            .join("\n");
        windows.push(region);
    }
    windows
}

#[test]
fn test_feed_disable_falling_edge_emits_feed_down() {
    let src = read_repo_file("src/groww_bridge.rs");
    let prod = production_region(&src);
    let windows = marker_windows(prod, "\"feed_disabled\"", 12, 45);
    assert!(
        !windows.is_empty(),
        "groww_bridge.rs: expected at least one \"feed_disabled\" falling-edge marker \
         in the production region"
    );
    for (n, region) in windows.iter().enumerate() {
        assert!(
            region.contains("NotificationEvent::FeedDown"),
            "groww_bridge.rs: feed-disable falling edge #{n} must notify \
             NotificationEvent::FeedDown — a DB-row-only disconnect is the \
             2026-07-06 silent-outage regression class:\n{region}"
        );
        assert!(
            region.contains("try_announce_feed_down"),
            "groww_bridge.rs: the feed-disable FeedDown emit #{n} must be gated by the \
             per-episode try_announce_feed_down latch (audit Rule 4 — once per episode)"
        );
        // Regression: 2026-07-06 — the severity input must stay CALENDAR-aware
        // (is_trading_session_now), never the clock-only helper: the disable
        // arm computes `is_within_market_hours_ist()` 30 lines above for the
        // audit-row kind, and a "unify the two market checks" cleanup would
        // silently restore the 2026-05-09 Saturday/holiday false-page class.
        assert!(
            region
                .contains("market_open: tickvault_common::market_hours::is_trading_session_now()"),
            "groww_bridge.rs: the feed-disable FeedDown emit #{n} must pass the \
             calendar-aware is_trading_session_now() as market_open — the clock-only \
             is_within_market_hours_ist() pages High + SNS on Saturday manual runs \
             and NSE weekday holidays (fixed 2026-07-06):\n{region}"
        );
    }
}

#[test]
fn test_bridge_death_falling_edge_emits_feed_down() {
    let src = read_repo_file("src/groww_bridge.rs");
    let prod = production_region(&src);
    let windows = marker_windows(prod, "\"bridge_died\"", 8, 50);
    assert!(
        !windows.is_empty(),
        "groww_bridge.rs: expected at least one \"bridge_died\" falling-edge marker \
         in the production region"
    );
    for (n, region) in windows.iter().enumerate() {
        assert!(
            region.contains("NotificationEvent::FeedDown"),
            "groww_bridge.rs: bridge-death falling edge #{n} must notify \
             NotificationEvent::FeedDown:\n{region}"
        );
        assert!(
            region.contains("try_announce_feed_down"),
            "groww_bridge.rs: the bridge-death FeedDown emit #{n} must be gated by the \
             per-episode try_announce_feed_down latch"
        );
        // Regression: 2026-07-06 — calendar-aware severity input (see the
        // feed-disable twin above for the full rationale).
        assert!(
            region
                .contains("market_open: tickvault_common::market_hours::is_trading_session_now()"),
            "groww_bridge.rs: the bridge-death FeedDown emit #{n} must pass the \
             calendar-aware is_trading_session_now() as market_open (Saturday/holiday \
             false-page class, fixed 2026-07-06):\n{region}"
        );
    }
}

#[test]
fn test_streaming_rising_edge_emits_feed_recovered() {
    let src = read_repo_file("src/groww_bridge.rs");
    let prod = production_region(&src);
    // The recovery block follows the "groww_resumed" Reconnected classify —
    // a generous forward window covers it.
    let windows = marker_windows(prod, "\"groww_resumed\"", 4, 60);
    assert!(
        !windows.is_empty(),
        "groww_bridge.rs: expected the \"groww_resumed\" rising-edge marker \
         in the production region"
    );
    let any_recovery = windows.iter().any(|region| {
        region.contains("NotificationEvent::FeedRecovered") && region.contains("take_feed_recovery")
    });
    assert!(
        any_recovery,
        "groww_bridge.rs: the streaming rising edge must fire the honest \
         FeedRecovered via take_feed_recovery (recovery claimed on ticks, \
         never on mere socket-connect)"
    );
    // The recovery must never fire from the boot-connect (socket-connect)
    // arm — that arm keeps saying "awaiting first tick".
    let boot_windows = marker_windows(prod, "\"groww_subscribed\"", 8, 30);
    for region in &boot_windows {
        assert!(
            !region.contains("FeedRecovered"),
            "groww_bridge.rs: FeedRecovered must NOT fire from the boot-connect \
             (socket-connect) arm — connected != streaming (audit Rule 11)"
        );
    }
}

#[test]
fn test_gauges_published_from_bridge_loop() {
    let src = read_repo_file("src/groww_bridge.rs");
    let prod = production_region(&src);
    assert!(
        prod.contains("metrics::gauge!(\"tv_groww_ws_active\")"),
        "groww_bridge.rs: the connected-level tv_groww_ws_active gauge must be \
         published from the bridge loop (the Dhan pool watchdog never runs on a \
         groww-only boot) — the CloudWatch inactivity alarm is blind without it"
    );
    assert!(
        prod.contains("tv_feed_last_tick_age_seconds\", \"feed\" => \"groww\""),
        "groww_bridge.rs: tv_feed_last_tick_age_seconds{{feed=\"groww\"}} must be \
         published with a STATIC feed label from the bridge loop"
    );
    // The age gauge must stay honest: publish only when a tick has been seen
    // (last_tick_age_secs returns Option; a fake 0 pre-first-tick is the
    // false-OK class the judge plan explicitly bans).
    assert!(
        prod.contains("last_tick_age_secs(Feed::Groww"),
        "groww_bridge.rs: the age gauge must read FeedHealthRegistry::last_tick_age_secs \
         (Option-gated — never a fabricated age)"
    );
}

#[test]
fn test_status_file_evidence_is_freshness_gated() {
    // 2026-07-06 stale-status false-OK fix: the sidecar NEVER deletes its
    // status file (a killed child freezes it at its last "streaming" write;
    // EBS persists it across the daily stop/start), so the bridge MUST gate
    // every status-file read through the pure freshness classifier before
    // treating it as evidence. Without the gate: a disable→re-enable cycle
    // fired a false "✅ Prices are flowing" FeedRecovered off the pre-disable
    // fossil, and a sidecar dead at boot read yesterday's file as today's
    // proof — pinning tv_groww_ws_active at 1 all session and blinding the
    // groww-ws-inactive alarm (the exact silent-outage class this feature
    // exists to close).
    let src = read_repo_file("src/groww_bridge.rs");
    let prod = production_region(&src);
    let windows = marker_windows(prod, "read_status_file(&status_file_path)", 2, 12);
    assert!(
        !windows.is_empty(),
        "groww_bridge.rs: expected the status-file read in the production region"
    );
    for (n, region) in windows.iter().enumerate() {
        assert!(
            region.contains("groww_status_is_live"),
            "groww_bridge.rs: status-file read #{n} must gate on groww_status_is_live \
             (a stale record must be treated exactly like an absent file):\n{region}"
        );
        // 2026-07-06 boot-deferral fix: the one-shot `subscribed` record is
        // fresh for only 120s, so freshness must LATCH per episode — without
        // the latch, a >120s docker/notifier boot chain permanently lost the
        // deferred boot-connect ping and left the gauge 0 on tickless windows.
        assert!(
            region.contains("fresh_status_seen_this_episode"),
            "groww_bridge.rs: status-file read #{n} must latch freshness per episode \
             (fresh_status_seen_this_episode) — requiring the one-shot `subscribed` \
             record to STILL be fresh at announce time silently breaks the \
             'delayed, never lost' boot-connect deferral:\n{region}"
        );
    }
    // The disable arm must advance the live floor + reset the episode latch
    // so a post-re-enable read can never accept a pre-disable fossil as fresh.
    let disable_windows = marker_windows(prod, "\"feed_disabled\"", 12, 60);
    assert!(!disable_windows.is_empty());
    for (n, region) in disable_windows.iter().enumerate() {
        assert!(
            region.contains("live_floor_ist_nanos = receipt_ist_nanos()"),
            "groww_bridge.rs: the disable arm #{n} must advance the live-evidence \
             floor (live_floor_ist_nanos = receipt_ist_nanos()) so a re-enable can \
             never latch off the pre-disable frozen status file or the pre-disable \
             NDJSON backlog"
        );
        assert!(
            region.contains("fresh_status_seen_this_episode = false"),
            "groww_bridge.rs: the disable arm #{n} must reset the per-episode \
             status-freshness latch (fresh_status_seen_this_episode = false) — \
             otherwise the pre-disable episode's proof leaks across the re-enable"
        );
    }
}

#[test]
fn test_tick_channel_streaming_latch_is_freshness_gated() {
    // 2026-07-06 replayed-backlog false-recovery fix: the STATUS file is not
    // the only streaming-evidence channel — `drain_new_data` returning true
    // for ANY validated line (including the ≤2s pre-disable NDJSON backlog
    // replayed on the first re-enable wake, and a respawned bridge's byte-0
    // re-tail) used to latch `streaming_observed` un-gated: false
    // FeedRecovered ("Prices are flowing") before the relaunched sidecar had
    // even authenticated, DOWN episode consumed, tv_groww_ws_active pinned 1
    // all session. The latch (and ONLY the latch — persist/fold stay
    // unconditional, zero loss) must gate on the drained lines' per-message
    // capture stamps being at/after the same live floor the status channel
    // uses.
    let src = read_repo_file("src/groww_bridge.rs");
    let prod = production_region(&src);
    let windows = marker_windows(prod, "state.drain_new_data(&tick_file_path", 2, 25);
    assert!(
        !windows.is_empty(),
        "groww_bridge.rs: expected the run-loop drain_new_data call in the \
         production region"
    );
    for (n, region) in windows.iter().enumerate() {
        assert!(
            region.contains("wake_had_fresh_capture(live_floor_ist_nanos)"),
            "groww_bridge.rs: run-loop drain site #{n} must gate the tick-channel \
             streaming latch on wake_had_fresh_capture(live_floor_ist_nanos) — an \
             un-gated `parsed_a_tick => streaming_observed` resurrects the \
             replayed-backlog false-recovery class:\n{region}"
        );
    }
}

#[test]
fn test_cw_agent_selector_ships_the_new_metrics() {
    // The LIVE cw-agent copy (shipped on every deploy). The
    // terraform/user-data lockstep twin is pinned by
    // crates/storage/tests/cw_agent_selector_lockstep_guard.rs.
    let cfg = read_repo_file("../../deploy/aws/cloudwatch-agent.json");
    for name in [
        "tv_groww_ws_active",
        "tv_feed_last_tick_age_seconds",
        "tv_feed_sidecar_stall_restart_total",
    ] {
        assert!(
            cfg.contains(name),
            "deploy/aws/cloudwatch-agent.json metric_selectors must ship {name} — \
             an alarm on a metric that never reaches CloudWatch is a false-OK"
        );
    }
}

#[test]
fn test_dhan_blast_radius_is_zero() {
    // The Groww feed-down alerting must not leak into the Dhan main-feed
    // connection code — Dhan keeps its own WebSocketDisconnected /
    // ...OffHours / Reconnected semantics untouched (hard scope constraint).
    let dhan = read_repo_file("../core/src/websocket/connection.rs");
    assert!(
        !dhan.contains("FeedDown") && !dhan.contains("FeedRecovered"),
        "crates/core/src/websocket/connection.rs must NOT emit FeedDown/FeedRecovered — \
         the Dhan lanes are out of scope for the Groww feed-down alerting"
    );
}
