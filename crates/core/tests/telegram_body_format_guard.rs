//! Telegram body-format ratchet (Telegram cleanliness overhaul,
//! coordinator-relayed directive 2026-07-15).
//!
//! Render-based build-failing pins over the redesigned operator-facing
//! bodies: routine (Info/Low) cards stay SHORT, verdict-FIRST, and never
//! render sentinel placeholders ("?" / "not measured") — an unmeasured
//! value is OMITTED, structurally. High/Critical bodies are exempt from
//! the line budgets (failure detail is wanted).
//!
//! HONEST LIMITATION: this guard is FIXTURE-based, not enum-exhaustive —
//! it constructs representative events and asserts over their rendered
//! bodies. A NEW routine variant must add a fixture here (review
//! checklist item); the guard cannot see variants nobody constructs.

use tickvault_core::notification::events::{
    FeedScoreLine, NotificationEvent, RestLegScoreLine, Severity, ShutdownClass,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn feed(name: &str) -> FeedScoreLine {
    FeedScoreLine {
        name: name.to_string(),
        ticks: 1_942_000,
        exclusive_minutes: 12,
        lag_p50_ms: 700,
        lag_p99_ms: 1_400,
        drops_market: 0,
        blame_broker: 0,
        blame_ours: 0,
        blame_unclear: 0,
        stalls: 0,
        restarts: 0,
        streaming_minutes: 375,
    }
}

fn sentinel_feed(name: &str) -> FeedScoreLine {
    FeedScoreLine {
        name: name.to_string(),
        ticks: -1,
        exclusive_minutes: -1,
        lag_p50_ms: -1,
        lag_p99_ms: -1,
        drops_market: -1,
        blame_broker: -1,
        blame_ours: -1,
        blame_unclear: -1,
        stalls: -1,
        restarts: -1,
        streaming_minutes: -1,
    }
}

fn rest_leg(feed_name: &str, ok: i64, failed: i64) -> RestLegScoreLine {
    RestLegScoreLine {
        feed: feed_name.to_string(),
        leg: "spot candles".to_string(),
        ok_fetches: ok,
        failed_fetches: failed,
        named_gaps: -1,
        rate_limited_hits: -1,
        late_recovered: -1,
        close_p50_ms: -1,
        close_p99_ms: -1,
        close_max_ms: -1,
        close_samples: -1,
    }
}

#[allow(clippy::too_many_arguments)]
fn scorecard(
    dhan: FeedScoreLine,
    groww: FeedScoreLine,
    partial: bool,
    dhan_off: bool,
    groww_off: bool,
    rest_legs: Vec<RestLegScoreLine>,
) -> NotificationEvent {
    NotificationEvent::DualFeedDailyScorecard {
        trading_date_ist: "2026-07-15".to_string(),
        dhan,
        groww,
        session_minutes: 375,
        partial_coverage: partial,
        degraded: false,
        early_run: false,
        restart_partial: false,
        dhan_feed_off: dhan_off,
        groww_feed_off: groww_off,
        rest_legs,
        rest_legs_read_failed: false,
    }
}

fn scorecard_clean() -> NotificationEvent {
    scorecard(
        feed("Dhan"),
        feed("Groww"),
        false,
        false,
        false,
        vec![rest_leg("Dhan", 735, 0), rest_leg("Groww", 733, 2)],
    )
}

fn tf_pass(tail_unsealed: u64) -> NotificationEvent {
    NotificationEvent::TfConsistencySummary {
        dhan_date_ist: "2026-07-15".to_string(),
        groww_date_ist: "2026-07-14".to_string(),
        instruments: 214,
        buckets_compared: 692_171,
        mismatches: 0,
        missing_tf_rows: 0,
        no_coverage: 0,
        off_grid: 0,
        duplicates: 0,
        tail_unsealed,
        degraded: false,
        truncated: false,
        status_label: "pass".to_string(),
        top_detail: vec![],
    }
}

fn shutdown(class: ShutdownClass) -> NotificationEvent {
    NotificationEvent::ShutdownInitiated { class }
}

fn brutex_clean() -> NotificationEvent {
    NotificationEvent::BrutexCrossverifySummary {
        trading_date_ist: "2026-07-15".to_string(),
        outcome: "clean".to_string(),
        files_read: 5,
        symbols_compared: 742,
        matched: 276_490,
        diverged: 0,
        missing_brutex: 12,
        missing_live: 3,
        tail_unsealed: 742,
        unmapped: 6,
        noise_p95_paise: 5,
        noise_max_paise: 40,
        top_offenders: String::new(),
        hint: String::new(),
    }
}

/// The routine (Info/Low) fixture set the line-budget + phrase pins run
/// over. High/Critical bodies are exempt (failure detail is wanted).
fn routine_fixtures() -> Vec<NotificationEvent> {
    vec![
        NotificationEvent::TokenRenewed,
        tf_pass(0),
        tf_pass(9),
        shutdown(ShutdownClass::ScheduledStop),
        shutdown(ShutdownClass::OperatorStop),
        scorecard_clean(),
        scorecard(
            sentinel_feed("Dhan"),
            sentinel_feed("Groww"),
            false,
            false,
            false,
            vec![],
        ),
        scorecard(feed("Dhan"), feed("Groww"), false, true, false, vec![]),
        NotificationEvent::Spot1mFetchRecovered {
            minute_ist: "10:45 AM".to_string(),
            failed_minutes: 3,
        },
    ]
}

fn first_line(body: &str) -> &str {
    body.lines().next().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 1. Line budgets
// ---------------------------------------------------------------------------

#[test]
fn guard_daily_cards_carry_the_compact_date() {
    // G3 (fix round 2, 2026-07-15): past-day backfill/forced cards
    // (TICKVAULT_SCOREBOARD_DATE / TICKVAULT_TF_VERIFY_DATE) must be
    // distinguishable from today's — the scorecard header and the TF
    // pass one-liner ALWAYS carry the compact verified date.
    let body = scorecard_clean().to_message();
    assert!(
        first_line(&body).contains("Feed scorecard 3:45 PM \u{b7} 15 Jul"),
        "scorecard header must carry the compact date: {body}"
    );
    let body = tf_pass(0).to_message();
    assert!(
        body.contains("Timeframe check 3:40 PM \u{b7} 15 Jul"),
        "TF pass one-liner must carry the compact date: {body}"
    );
}

#[test]
fn guard_tf_pass_body_is_exactly_one_line() {
    for ev in [tf_pass(0), tf_pass(9)] {
        let body = ev.to_message();
        assert_eq!(body.lines().count(), 1, "TF pass must be ONE line: {body}");
    }
}

#[test]
fn guard_shutdown_bodies_within_two_lines_all_classes() {
    for class in [
        ShutdownClass::ScheduledStop,
        ShutdownClass::OperatorStop,
        ShutdownClass::ExternalStop,
    ] {
        let body = shutdown(class).to_message();
        assert!(
            body.lines().count() <= 2,
            "shutdown body ({class:?}) must stay ≤ 2 lines: {body}"
        );
    }
}

#[test]
fn guard_scorecard_fully_measured_within_six_lines() {
    for ev in [
        scorecard_clean(),
        scorecard(
            feed("Dhan"),
            feed("Groww"),
            true, // partial → caveat line
            false,
            false,
            vec![rest_leg("Dhan", 735, 0), rest_leg("Groww", 700, 35)],
        ),
        scorecard(feed("Dhan"), feed("Groww"), false, true, false, vec![]),
        scorecard(
            sentinel_feed("Dhan"),
            sentinel_feed("Groww"),
            false,
            false,
            false,
            vec![],
        ),
    ] {
        let body = ev.to_message();
        assert!(
            body.lines().count() <= 6,
            "scorecard must stay ≤ 6 lines in every arm, got {}: {body}",
            body.lines().count()
        );
    }
}

#[test]
fn guard_routine_info_low_bodies_within_four_lines() {
    for ev in routine_fixtures() {
        assert!(
            ev.severity() <= Severity::Low,
            "fixture {} must be routine (Info/Low) for this pin",
            ev.topic()
        );
        // The scorecard has its own dedicated ≤6 budget above.
        if ev.topic() == "DualFeedDailyScorecard" {
            continue;
        }
        let body = ev.to_message();
        assert!(
            body.lines().count() <= 4,
            "routine {} body must stay ≤ 4 lines, got {}: {body}",
            ev.topic(),
            body.lines().count()
        );
    }
    // Topic-keyed allowlist: the BruteX daily digest keeps its wider body
    // (≤ 8 lines on a clean day) — its diet is an explicitly DEFERRED item.
    let brutex = brutex_clean().to_message();
    assert!(
        brutex.lines().count() <= 8,
        "BruteX clean-day digest must stay ≤ 8 lines, got {}: {brutex}",
        brutex.lines().count()
    );
}

// ---------------------------------------------------------------------------
// 2. Banned sentinel/placeholder phrases in routine bodies
// ---------------------------------------------------------------------------

#[test]
fn guard_no_sentinel_placeholder_phrases_in_routine_scorecards() {
    // The all-sentinel + feed-off fixtures are exactly where the old body
    // rendered "?" / "not measured yet" walls — omission must now be
    // structural.
    let fixtures = [
        scorecard(
            sentinel_feed("Dhan"),
            sentinel_feed("Groww"),
            false,
            false,
            false,
            vec![],
        ),
        scorecard(feed("Dhan"), feed("Groww"), false, true, false, vec![]),
        scorecard(feed("Dhan"), feed("Groww"), false, false, true, vec![]),
        scorecard_clean(),
    ];
    for ev in fixtures {
        let body = ev.to_message();
        for banned in [
            "not measured",
            "could not be measured",
            "unknown",
            "| ?",
            ": ?",
            "not comparable",
            "not a pass and not a failure",
            "Note:",
        ] {
            assert!(
                !body.contains(banned),
                "routine scorecard body must not carry {banned:?}: {body}"
            );
        }
        // "no contest" wording may appear ONLY in the verdict (first) line.
        let rest: String = body.lines().skip(1).collect::<Vec<_>>().join("\n");
        assert!(
            !rest.contains("no contest"),
            "no-contest wording may appear only in line 1: {body}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Verdict-first ordering
// ---------------------------------------------------------------------------

#[test]
fn guard_scorecard_verdict_line_is_first_with_emoji_and_header() {
    for ev in [
        scorecard_clean(),
        scorecard(feed("Dhan"), feed("Groww"), true, false, false, vec![]),
        scorecard(feed("Dhan"), feed("Groww"), false, true, false, vec![]),
    ] {
        let body = ev.to_message();
        let first = first_line(&body);
        assert!(
            first.starts_with('\u{1f3c6}')
                || first.starts_with("\u{26a0}\u{fe0f}")
                || first.starts_with('\u{1f4ca}'),
            "scorecard line 1 must start with 🏆/⚠️/📊: {body}"
        );
        assert!(
            first.contains("<b>"),
            "scorecard line 1 must carry the bold header: {body}"
        );
    }
}

#[test]
fn guard_tf_pass_line_starts_with_ok_emoji() {
    let body = tf_pass(0).to_message();
    assert!(
        body.starts_with('\u{2705}'),
        "TF pass must lead with ✅: {body}"
    );
}

// ---------------------------------------------------------------------------
// 4. <code> alignment of the feed stat lines
// ---------------------------------------------------------------------------

#[test]
fn guard_scorecard_feed_stat_lines_are_code_wrapped_and_aligned() {
    let body = scorecard_clean().to_message();
    let stat_lines: Vec<&str> = body.lines().filter(|l| l.starts_with("<code>")).collect();
    assert_eq!(stat_lines.len(), 2, "one <code> stat line per feed: {body}");
    // The feed-name prefixes (up to the first ':') must be equal width so
    // the monospace columns align.
    let widths: Vec<usize> = stat_lines
        .iter()
        .map(|l| {
            l.trim_start_matches("<code>")
                .split(':')
                .next()
                .unwrap_or_default()
                .chars()
                .count()
        })
        .collect();
    assert_eq!(
        widths[0], widths[1],
        "feed-name prefixes must pad to equal width: {stat_lines:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Caveat (footnote) discipline
// ---------------------------------------------------------------------------

#[test]
fn guard_caveat_line_renders_iff_partial_flag_and_exactly_once() {
    let clean = scorecard_clean().to_message();
    assert!(
        !clean.contains("Counts are a floor"),
        "clean day must carry no caveat: {clean}"
    );
    let partial = scorecard(feed("Dhan"), feed("Groww"), true, false, false, vec![]).to_message();
    assert_eq!(
        partial.matches("Counts are a floor").count(),
        1,
        "partial day carries the caveat EXACTLY once: {partial}"
    );
}

// ---------------------------------------------------------------------------
// 6. No action lists below High
// ---------------------------------------------------------------------------

#[test]
fn guard_routine_bodies_never_carry_action_lists() {
    for ev in routine_fixtures() {
        let body = ev.to_message();
        for banned in ["What you need to do RIGHT NOW", "What to do RIGHT NOW"] {
            assert!(
                !body.contains(banned),
                "routine {} body must not carry an action list: {body}",
                ev.topic()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Feed-off honesty (the one-line OFF form)
// ---------------------------------------------------------------------------

#[test]
fn guard_feed_off_renders_exactly_one_off_line() {
    let body = scorecard(feed("Dhan"), feed("Groww"), false, true, false, vec![]).to_message();
    assert_eq!(
        body.matches("Dhan: OFF today (excluded from verdict)")
            .count(),
        1,
        "an OFF feed renders exactly one honest line: {body}"
    );
    assert!(
        !body.contains("<code>Dhan"),
        "an OFF feed must not render a stat line: {body}"
    );
}

// ---------------------------------------------------------------------------
// 8. REST-pair fixture sanity (severities pinned so the routing/loudness
//    contract behind the redesign cannot silently drift)
// ---------------------------------------------------------------------------

#[test]
fn guard_rest_pair_severities_high_open_info_resolve() {
    let degraded = NotificationEvent::Spot1mFetchDegraded {
        consecutive_failed_minutes: 3,
        minute_ist: "10:42 AM".to_string(),
    };
    assert_eq!(degraded.severity(), Severity::High);
    let recovered = NotificationEvent::Spot1mFetchRecovered {
        minute_ist: "10:45 AM".to_string(),
        failed_minutes: 3,
    };
    assert_eq!(recovered.severity(), Severity::Info);
}
