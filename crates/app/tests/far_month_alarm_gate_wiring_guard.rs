//! Source-scan ratchet — §36.7 D7 far-month alarm-gate wiring (AM-r1 F7,
//! 2026-07-10).
//!
//! Hostile-review-confirmed gap: the two ALARM-FACING consumer sites of the
//! far-month IndexFuture exclusion — the tick-gap silent-gauge subtraction
//! and the SLO tick_freshness filter — live inside spawned closures in
//! main.rs with no direct test. Only the pure `far_month_future_sids`
//! helper was tested, so a future refactor could drop
//! `is_far_month_future_sid` from either consumer with every §36.7 test
//! still green: the ~8 quiet far-month SIDs would lift the ~33
//! always-silent healthy floor toward the threshold-40 alarm and drag SLO
//! freshness below 0.95 — the exact false-page class the D7 change exists
//! to prevent, restored silently.
//!
//! House pattern: `ws_rate_limit_cooldown_wiring_guard.rs` /
//! `health_counter_fix7_guard.rs` — pin the call sites by source scan so
//! removal fails the build. Runbook: `futidx-4-error-codes.md` §3.

use std::fs;
use std::path::PathBuf;

fn read_main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Gauge site (60s tick-gap loop): far-month silents are counted, then
/// SUBTRACTED from the alarm-facing gauge value.
const GAUGE_FILTER: &str = ".filter(|(sid, seg, _)| is_far_month_future_sid(*sid, *seg))";
const GAUGE_SUBTRACT: &str = "total_silent.saturating_sub(excluded_silent)";
/// SLO site (10s freshness loop): far-month SIDs are filtered OUT of the
/// silent set feeding tick_freshness.
const SLO_FILTER: &str = "&& !is_far_month_future_sid(*sid, *seg)";
/// Seed site: the exclusion set is refreshed from every fresh plan.
const SEED_FN: &str = "fn seed_tick_gap_detector_from_plan(";
const SEED_STORE: &str = "store_far_month_future_exclusions(far)";
const SEED_DETECTOR: &str = "detector.seed_subscribed(";

#[test]
fn ratchet_gauge_subtraction_site_is_wired() {
    let src = read_main_rs();
    assert!(
        src.contains(GAUGE_FILTER),
        "§36.7 D7 regression: the 60s tick-gap gauge loop no longer counts \
         far-month silents via is_far_month_future_sid — the far months \
         would lift the alarm-facing gauge again (AM-r1 F7)"
    );
    assert!(
        src.contains(GAUGE_SUBTRACT),
        "§36.7 D7 regression: the gauge no longer subtracts excluded_silent \
         — tv_tick_gap_instruments_silent would count far months again"
    );
}

#[test]
fn ratchet_slo_freshness_filter_is_wired() {
    let src = read_main_rs();
    assert!(
        src.contains(SLO_FILTER),
        "§36.7 D7 regression: the SLO tick_freshness silent-set filter no \
         longer excludes far-month futures via is_far_month_future_sid — \
         SLO freshness would drop below 0.95 on healthy days (AM-r1 F7)"
    );
}

#[test]
fn ratchet_seed_refreshes_exclusions_before_seeding() {
    let src = read_main_rs();
    let seed_pos = src
        .find(SEED_FN)
        .expect("seed_tick_gap_detector_from_plan must exist");
    let store_pos = src
        .find(SEED_STORE)
        .expect("store_far_month_future_exclusions(far) call must exist");
    let after_seed = &src[seed_pos..];
    let detector_rel = after_seed
        .find(SEED_DETECTOR)
        .expect("detector.seed_subscribed must exist after the seed fn");
    assert!(
        store_pos > seed_pos && store_pos < seed_pos + detector_rel,
        "§36.7 D7 regression: the far-month exclusion store must run inside \
         seed_tick_gap_detector_from_plan BEFORE the detector seeding loop \
         so every fresh plan (both boot arms + a lane cold start) refreshes \
         the set (AM-r1 F7)"
    );
}

#[test]
fn ratchet_nearest_month_flows_through_shared_singular_on_canonical_groups() {
    // AM-r1 F3 + F6: the exclusion derivation must group by the CANONICAL
    // underlying and identify the nearest month via the SHARED singular
    // selector — raw-literal grouping silently disabled the exclusion on
    // alias drift (MIDCPNIFTY precedent), and a private min-loop would
    // fork the `>= today` rule.
    let src = read_main_rs();
    assert!(
        src.contains("canonicalize_index_symbol(&inst.underlying_symbol)"),
        "AM-r1 F3 regression: far_month_future_sids no longer groups by the \
         canonical underlying — mixed alias literals split groups and \
         disable the exclusion"
    );
    assert!(
        src.contains("select_index_future_expiry(&dates, today_ist)"),
        "AM-r1 F6 regression: far_month_future_sids no longer identifies \
         the nearest month via the shared select_index_future_expiry"
    );
}
