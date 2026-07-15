//! Source-scan ratchet — REST 1m pull digest wired into the daily
//! scorecard (Groww REST plan PR-5, operator Quote 2 2026-07-13: *"always
//! clearly note within a second — or within how many seconds precisely —
//! we are fetching this live real OHLCV, along with the option chain
//! API"*).
//!
//! Pins (house pattern: `groww_spot_1m_wiring_guard.rs`):
//! 1. `run_feed_scoreboard` (feed_scoreboard_boot.rs, PRODUCTION region)
//!    really aggregates the day's `rest_fetch_audit` rows + the
//!    `spot_1m_rest` latency fallback and sets the /metrics-local p99
//!    gauge — the digest rides the EXISTING 15:45 run (both boot arms +
//!    the backfill/NOW overrides for free), never a new task.
//! 2. main.rs threads the digest into the Telegram card —
//!    `build_rest_leg_score_lines(` + the `rest_legs_read_failed` honest
//!    footnote flag — so the Quote-2 lines can never silently drop out of
//!    the operator surface.
//! 3. Degrade stages are CODED (`stage = "rest_leg_*"` under
//!    SCOREBOARD-01) — a read/parse failure is loud, additive, and never
//!    touches the card's existing sections.
//!
//! Runbooks: `.claude/rules/project/dual-feed-scoreboard-error-codes.md`
//! + `.claude/rules/project/rest-1m-pipeline-error-codes.md`.

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The PRODUCTION region only — the in-module tests exercise the same
/// identifiers, so an unsplit scan would pass vacuously on test code.
fn production_region(src: &str) -> String {
    src.split("#[cfg(test)]").next().unwrap_or("").to_string()
}

#[test]
fn ratchet_scoreboard_run_aggregates_rest_leg_digest() {
    let boot = production_region(&read_app_src("src/feed_scoreboard_boot.rs"));

    // The digest SQL builders are CALLED from the run (the `(` suffix
    // distinguishes call/def sites from doc mentions); both sources feed
    // the aggregation.
    for needle in [
        "build_rest_fetch_audit_day_sql(target_ist_day)",
        "build_spot1m_close_latency_day_sql(target_ist_day)",
        "aggregate_rest_leg_day(&audit_lite, &spot_latency_fallback)",
    ] {
        assert!(
            boot.contains(needle),
            "feed_scoreboard_boot.rs production region must contain {needle:?} — \
             the PR-5 digest must ride the existing daily run"
        );
    }

    // Degrade stages are coded + loud (never a silent skip).
    for stage in [
        "stage = \"rest_leg_read\"",
        "stage = \"rest_leg_parse\"",
        "stage = \"rest_leg_fallback_read\"",
        "stage = \"rest_leg_fallback_parse\"",
    ] {
        assert!(
            boot.contains(stage),
            "the digest read/parse failure arms must log the coded {stage} \
             (SCOREBOARD-01) — a silent degrade is a Rule-11 violation"
        );
    }

    // The dashboard gauge (plan PR-5 \"+ dashboard gauge\"): /metrics-local,
    // same-day gated, static-label allowlisted.
    assert!(
        boot.contains("tv_rest_leg_close_to_data_p99_ms"),
        "the p99 dashboard gauge must be set from the daily run"
    );
    assert!(
        boot.contains("rest_leg_gauge_labels(&s.feed, &s.leg)"),
        "gauge labels must pass the static allowlist — parsed strings must \
         never become metric label values"
    );

    // The summary threads the digest + the honest read-failure flag.
    for needle in ["rest_legs,", "rest_legs_read_failed,"] {
        assert!(
            boot.contains(needle),
            "ScoreboardSummary must carry {needle:?} so main.rs can render \
             the Quote-2 lines"
        );
    }
}

#[test]
fn ratchet_main_threads_rest_leg_digest_into_the_card() {
    let main_src = read_app_src("src/main.rs");
    assert!(
        main_src.contains("build_rest_leg_score_lines("),
        "main.rs must map the summary's digest aggregates into the card's \
         RestLegScoreLine vec — the Quote-2 answer must reach Telegram"
    );
    assert!(
        main_src.contains("rest_legs_read_failed: summary.rest_legs_read_failed"),
        "main.rs must thread the read-failure flag so the card carries the \
         honest cause footnote instead of a silent under-count"
    );
}

/// Stub-guard (audit-findings Rule 14): the late-recovery split constant
/// really is the 60s boundary — the prompt distribution must never be
/// skewed by a 2-hour sweep repair, and a silent constant change would
/// silently redefine every published freshness number.
#[test]
fn ratchet_rest_leg_late_threshold_is_60s() {
    assert_eq!(
        tickvault_app::feed_scoreboard_boot::REST_LEG_LATE_RECOVERY_MS,
        60_000
    );
}
