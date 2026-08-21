//! Pins that the "is the feed actually working" alarm reads a GAUGE, and that
//! the gauge it reads is published and shipped.
//!
//! # The defect this prevents
//!
//! `tv-<env>-dhan-no-ticks-flowing` is the only alarm in the live lane that
//! separates "nothing broke" from "nothing ran". Its first version read the
//! COUNTER `tv_dhan_feed_ingest_ticks_total` with `Sum < 1`, and its own
//! header conceded the residual that made that unsafe: this repository has
//! never been able to verify, from the sandbox, whether the CloudWatch agent's
//! prometheus pipeline publishes each scrape's DELTA or the running CUMULATIVE
//! total.
//!
//! Those two readings are not equally forgiving here. Under deltas, `Sum < 1`
//! over a window means no ticks in that window — correct. Under cumulative
//! values, `Sum` is roughly five times the session total, so `< 1` stops being
//! true the moment the first tick of the morning lands and the alarm reports
//! health for the rest of the day regardless of what the feed does. Silently.
//!
//! A gauge is published verbatim by both pipelines, because there is no delta
//! to compute for a value that is free to go down. So the fix was not to guess
//! which reading is true — it was to ask the question of a signal that means
//! the same thing under either.
//!
//! This guard exists because that distinction is invisible at the call site: a
//! future edit that repoints the alarm back at the counter, or that renames
//! the gauge on one side of the EMF selector pair, produces an alarm that
//! looks entirely reasonable in review and reports health forever.

use tickvault_app::dhan_feed_stack::{LAST_TICK_AGE_GAUGE, last_tick_age_gauge_value};

const ALARM_TF: &str = include_str!("../../../deploy/aws/terraform/live-lane-alarms.tf");
const AGENT_JSON: &str = include_str!("../../../deploy/aws/cloudwatch-agent.json");
const USER_DATA: &str = include_str!("../../../deploy/aws/terraform/user-data.sh.tftpl");
const DRAIN_SRC: &str = include_str!("../src/dhan_feed_stack.rs");

/// The alarm's own resource block, bounded at its closing brace so a later
/// alarm's fields can never satisfy an assertion about this one.
fn no_ticks_alarm_block() -> &'static str {
    let start = ALARM_TF
        .find(r#"resource "aws_cloudwatch_metric_alarm" "dhan_no_ticks_flowing""#)
        .expect(
            "the no-ticks-flowing alarm must exist — it is the lane's only 'is it working' signal",
        );
    let rest = &ALARM_TF[start..];
    let end = rest
        .find("\n}\n")
        .expect("alarm block must terminate at a column-0 closing brace");
    &rest[..end]
}

#[test]
fn the_no_ticks_alarm_reads_the_gauge_and_never_the_tick_counter() {
    let block = no_ticks_alarm_block();

    assert!(
        block.contains(&format!("metric_name        = \"{LAST_TICK_AGE_GAUGE}\""))
            || block.contains(&format!("metric_name = \"{LAST_TICK_AGE_GAUGE}\"")),
        "the no-ticks-flowing alarm must read the gauge `{LAST_TICK_AGE_GAUGE}`. \
         Its block reads:\n{block}"
    );

    assert!(
        !block.contains("metric_name        = \"tv_dhan_feed_ingest_ticks_total\""),
        "the no-ticks-flowing alarm has been repointed at the tick COUNTER. \
         That is the defect this guard exists for: if the agent publishes \
         cumulative counter values rather than per-scrape deltas, `Sum` over \
         the window is several times the session total and the `< 1` test can \
         never be true once a single tick has ever arrived — so the one alarm \
         written to prove ticks are flowing reports health for the rest of the \
         day, silently. The gauge means the same thing under either pipeline."
    );
}

#[test]
fn the_no_ticks_alarm_uses_maximum_so_one_late_tick_cannot_erase_a_silent_window() {
    let block = no_ticks_alarm_block();
    assert!(
        block.contains("statistic = \"Maximum\""),
        "the alarm must read `Maximum`. On an AGE gauge, `Average` lets a \
         single fresh scrape at the end of a window cancel four minutes of \
         silence, and `Minimum` reports the freshest moment in the window — \
         which is the opposite of what is being asked."
    );
    assert!(
        block.contains("comparison_operator = \"GreaterThanOrEqualToThreshold\""),
        "an AGE gauge breaches when it grows. A `LessThanThreshold` comparison \
         is the counter-era shape and would fire whenever the feed is HEALTHY."
    );
}

#[test]
fn the_no_ticks_alarm_keeps_its_name_so_the_market_hours_gate_still_arms_it() {
    let block = no_ticks_alarm_block();
    assert!(
        block.contains(r#"alarm_name        = "tv-${var.environment}-dhan-no-ticks-flowing""#),
        "the alarm NAME is what the market-hours gate Lambda's ALARM_NAMES list \
         matches on. It carries `treat_missing_data = breaching` and \
         `actions_enabled = false`, so a rename silently un-arms it: the gate \
         flips a name that no longer exists and the alarm never pages at all."
    );
    assert!(
        block.contains(r#"treat_missing_data = "breaching""#)
            && block.contains("actions_enabled = false"),
        "both halves must stay: `breaching` is what makes process death visible \
         here, and `actions_enabled = false` is what stops it paging every \
         evening at 17:30 and all weekend."
    );
}

#[test]
fn the_gauge_is_shipped_by_both_emf_selector_copies() {
    for (label, src) in [
        ("deploy/aws/cloudwatch-agent.json", AGENT_JSON),
        ("deploy/aws/terraform/user-data.sh.tftpl", USER_DATA),
    ] {
        assert!(
            src.contains(LAST_TICK_AGE_GAUGE),
            "`{LAST_TICK_AGE_GAUGE}` is missing from {label}. The alarm would \
             evaluate a metric that never reaches CloudWatch, and \
             `treat_missing_data = breaching` would then page every armed \
             minute of every trading day."
        );
    }
}

#[test]
fn the_gauge_is_published_from_production_code_not_only_from_tests() {
    let production = DRAIN_SRC
        .split("\nmod tests {")
        .next()
        .expect("splitting on the test module always yields a first half");
    assert!(
        production.contains("metrics::gauge!(LAST_TICK_AGE_GAUGE)"),
        "nothing in production publishes `{LAST_TICK_AGE_GAUGE}`. A shipped \
         selector plus an alarm plus no emit is the worst of the three states: \
         the series never registers, missing data breaches, and the lane pages \
         continuously while being perfectly healthy."
    );
}

#[test]
fn last_tick_age_gauge_value_reports_the_age_once_a_tick_has_landed() {
    assert!((last_tick_age_gauge_value(Some(0), 9_999) - 0.0).abs() < f64::EPSILON);
    assert!((last_tick_age_gauge_value(Some(42), 9_999) - 42.0).abs() < f64::EPSILON);
}

#[test]
fn last_tick_age_gauge_value_reports_uptime_while_nothing_has_ever_ticked() {
    // The case the alarm exists for. Zero here would read as perfect health
    // during the exact outage being watched for — a lane that dialled,
    // connected, subscribed and received nothing.
    assert!(
        (last_tick_age_gauge_value(None, 0) - 0.0).abs() < f64::EPSILON,
        "at the instant the drain starts there is no silence to report yet"
    );
    assert!(
        last_tick_age_gauge_value(None, 600) >= 300.0,
        "ten minutes into a session with no tick ever, the gauge must be past \
         the alarm threshold — that is the 2026-08-12 shape and it must page \
         rather than reassure"
    );
}
