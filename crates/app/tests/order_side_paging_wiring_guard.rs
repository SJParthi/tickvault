//! Source-scan ratchet — cluster-C order-side PAGING wired end-to-end
//! (2026-07-14). Pins the counter-side paging chain in lockstep so a
//! rename / deletion on ANY side (main.rs pre-registration, either
//! terraform shape) fails the build:
//!
//! 1. main.rs pre-registers `tv_orders_rejected_total` + both
//!    `tv_orders_placed_total{mode}` series at 0 AFTER the metrics
//!    recorder installs (the feed-stall round-5 first-sample-baseline
//!    lesson — the CW agent's delta pipeline drops each counter series'
//!    first sample, so a lazily-born rejected-counter loses the session's
//!    FIRST rejection and alarm #10 stays dead for single-rejection
//!    sessions).
//! 2. `deploy/aws/terraform/order-side-alarms.tf` extracts the placed
//!    counter into the DERIVED `tv_orders_placed_delta_total` name on the
//!    `/tickvault/<env>/metrics` log group (the seal-drop-alarm.tf house
//!    pattern) and the storm alarm reads that same derived name.
//! 3. The daily-loss alarm ships ARMED (arm-on-arrival — tv_daily_pnl is
//!    cluster A's emit) with `ok_actions = []` + `notBreaching` and
//!    WITHOUT `actions_enabled = false`.
//! 4. The fill-lag alarm ships DISARMED (`actions_enabled = false`) with
//!    the verbatim Phase-1 arming contract in its description.
//!
//! House precedents: `seal_drop_paging_wiring_guard.rs` (this file's
//! template — post-install registration ordering, string-aware HCL
//! stripper + self-test, needles built from the real literals),
//! `auth_failed_alarm_wiring_guard.rs`.
//! Runbook: `.claude/rules/project/wave-2-error-codes.md` (AUDIT-06) +
//! `.claude/rules/project/gap-enforcement.md` (OMS-GAP-02 / RISK-GAP-01).

use std::fs;
use std::path::PathBuf;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())) // APPROVED: test
}

/// Strip `//`-line-comments (treating `://` URL scheme separators as code)
/// so a comment mentioning a needle can never satisfy the scan.
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Everything strictly before the first top-level `#[cfg(test)]` — the
/// production region (robust to code after `mod tests`).
fn production_region(body: &str) -> &str {
    body.split("#[cfg(test)]").next().unwrap_or(body)
}

/// Strip `#`-comments from an HCL (terraform) body, STRING-AWARE: a `#`
/// inside a double-quoted string (e.g. an alarm_description's "criterion
/// #5") is kept as code. The seal-drop guard's 2026-07-09 hostile-review
/// lesson: comments can never be terraform configuration, so they are
/// removed before matching — otherwise the tf file's own header comment
/// satisfies the `ok_actions=[]` needle (the vacuous-guard false-OK class).
fn strip_hcl_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut in_str = false;
        let mut esc = false;
        let mut cut = line.len();
        for (i, &b) in bytes.iter().enumerate() {
            if esc {
                esc = false;
                continue;
            }
            match b {
                b'\\' if in_str => esc = true,
                b'"' => in_str = !in_str,
                b'#' if !in_str => {
                    cut = i;
                    break;
                }
                _ => {}
            }
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

#[test]
fn test_hcl_comment_stripper_selftest() {
    // Anti-vacuity self-test (house-mandatory): the stripper must drop
    // comment text (incl. a comment mentioning ok_actions = []) while
    // keeping a `#` inside a quoted string intact.
    let src = "# header ok_actions = [] comment\n\
               alarm_description = \"criterion #5 - kept\"\n\
               real_attr = 1 # trailing comment ok_actions = []\n";
    let stripped = strip_hcl_comments(src);
    assert!(
        !compact(&stripped).contains("ok_actions=[]"),
        "comment text survived HCL stripping: {stripped}"
    );
    assert!(
        stripped.contains("criterion #5 - kept") && stripped.contains("real_attr = 1"),
        "string or code text was wrongly removed: {stripped}"
    );
}

/// Whitespace-compacted form so rustfmt / terraform-fmt re-wrapping and
/// alignment can never hide a real site from a contiguous needle.
fn compact(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

const REJECTED_COUNTER: &str = "tv_orders_rejected_total";
const PLACED_COUNTER: &str = "tv_orders_placed_total";
const PLACED_DERIVED: &str = "tv_orders_placed_delta_total";
const ORDER_SIDE_TF: &str = "../../deploy/aws/terraform/order-side-alarms.tf";

#[test]
fn test_order_counters_preregistered_after_recorder_install() {
    // Production region only: a future #[cfg(test)] block in main.rs
    // mentioning the counters must never satisfy this scan.
    let full = read_repo_file("src/main.rs");
    let main_src = strip_line_comments(production_region(&full));
    let install = main_src
        .find("observability::init_metrics(")
        .expect("the metrics recorder install site must exist in main.rs"); // APPROVED: test
    let compact_main = compact(&main_src);
    for needle in [
        format!("counter!(\"{REJECTED_COUNTER}\").increment(0)"),
        format!("counter!(\"{PLACED_COUNTER}\",\"mode\"=>\"paper\").increment(0)"),
        format!("counter!(\"{PLACED_COUNTER}\",\"mode\"=>\"live\").increment(0)"),
    ] {
        assert!(
            compact_main.contains(&needle),
            "main.rs must pre-register the order-side counter series at 0 \
             (missing compacted needle: {needle}) — without it the CW agent's \
             dropped-first-sample delta baseline eats the session's FIRST \
             rejection/placement and alarm #10 / the storm alarm are dead \
             for single-event sessions"
        );
    }
    // Source-order: the registrations must sit AFTER the recorder install
    // (a pre-install registration resolves to the no-op recorder — the VOID
    // round-4 class from the feed-stall lesson).
    let reg = main_src.find(&format!("\"{REJECTED_COUNTER}\"")).expect(
        "main.rs must pre-register tv_orders_rejected_total post-install", // APPROVED: test
    );
    assert!(
        install < reg,
        "the order-side counter registrations in main.rs must come AFTER \
         observability::init_metrics — a pre-install registration registers \
         nothing"
    );
}

#[test]
fn test_orders_placed_filter_shape_matches_emitted_series() {
    let tf = compact(&strip_hcl_comments(&read_repo_file(ORDER_SIDE_TF)));
    // The filter extracts the REAL emitted counter name from the metrics
    // log group into the DERIVED name (never the raw identity — a future
    // EMF allowlisting of the raw name must not double-count).
    assert!(
        tf.contains(&format!("pattern=\"{{$.{PLACED_COUNTER}=*}}\"")),
        "order-side-alarms.tf filter pattern must match the real emitted \
         series field $.{PLACED_COUNTER} (mode-agnostic)"
    );
    assert!(
        tf.contains(&format!("name=\"{PLACED_DERIVED}\"")),
        "the filter transformation must publish the DERIVED name {PLACED_DERIVED}"
    );
    assert!(
        tf.contains(&format!("value=\"$.{PLACED_COUNTER}\"")),
        "the filter transformation value must read $.{PLACED_COUNTER}"
    );
    assert!(
        tf.contains("log_group_name=\"/tickvault/${var.environment}/metrics\""),
        "the filter must ride the /tickvault/<env>/metrics log group \
         (the per-scrape delta shipping leg)"
    );
    // The storm alarm reads the derived name with the storm shape.
    assert!(
        tf.contains(&format!("metric_name=\"{PLACED_DERIVED}\""))
            && tf.contains("comparison_operator=\"GreaterThanOrEqualToThreshold\"")
            && tf.contains("threshold=50")
            && tf.contains("statistic=\"Sum\""),
        "the orders-placed-storm alarm must read {PLACED_DERIVED} with \
         Sum >= 50 per window"
    );
}

#[test]
fn test_daily_loss_alarm_is_armed_with_no_ok_actions_and_not_breaching() {
    let raw = read_repo_file(ORDER_SIDE_TF);
    let stripped = strip_hcl_comments(&raw);
    // Isolate the daily-loss alarm resource block.
    let start = stripped
        .find("resource \"aws_cloudwatch_metric_alarm\" \"daily_loss_breach\"")
        .expect("order-side-alarms.tf must define the daily_loss_breach alarm"); // APPROVED: test
    let rest = &stripped[start..];
    let end = rest[1..].find("\nresource ").map_or(rest.len(), |i| i + 1);
    let block = compact(&rest[..end]);
    assert!(
        block.contains("alarm_name=\"tv-${var.environment}-daily-loss-breach\""),
        "daily-loss alarm name shape drifted"
    );
    assert!(
        block.contains("metric_name=\"tv_daily_pnl\"")
            && block.contains("comparison_operator=\"LessThanThreshold\"")
            && block.contains("statistic=\"Minimum\"")
            && block.contains("treat_missing_data=\"notBreaching\""),
        "daily-loss alarm must read tv_daily_pnl Minimum < threshold with \
         notBreaching (dormant-silent in dry-run — arm-on-arrival)"
    );
    assert!(
        block.contains("ok_actions=[]"),
        "daily-loss alarm must carry NO ok_actions — the risk-engine halt \
         latch outlives an intraday MTM bounce (Rule-11 false-recovery)"
    );
    assert!(
        !block.contains("actions_enabled=false"),
        "daily-loss alarm must ship ARMED (missing-data physics keep it \
         silent in dry-run) — actions_enabled=false would need a tf change \
         at cluster-A arrival, defeating arm-on-arrival"
    );
}

#[test]
fn test_fill_lag_alarm_ships_disarmed_with_arming_description() {
    let raw = read_repo_file(ORDER_SIDE_TF);
    let stripped = strip_hcl_comments(&raw);
    let start = stripped
        .find("resource \"aws_cloudwatch_metric_alarm\" \"order_fill_lag_high\"")
        .expect("order-side-alarms.tf must define the order_fill_lag_high alarm"); // APPROVED: test
    let rest = &stripped[start..];
    let end = rest[1..].find("\nresource ").map_or(rest.len(), |i| i + 1);
    let block = &rest[..end];
    let block_compact = compact(block);
    assert!(
        block_compact.contains("actions_enabled=false"),
        "order-fill-lag-high must SHIP DISARMED — in dry-run the metric \
         never publishes and an armed alarm is a permanently-\
         INSUFFICIENT_DATA dead pager"
    );
    assert!(
        block.contains("ARMED at Phase-1 live promotion"),
        "the fill-lag alarm description must carry the verbatim Phase-1 \
         arming contract (the arming PR's checklist lives in the \
         description so it cannot be lost)"
    );
    assert!(
        block_compact.contains("metric_name=\"tv_order_fill_lag_seconds\"")
            && block_compact.contains("statistic=\"Maximum\"")
            && block_compact.contains("threshold=10"),
        "fill-lag alarm shape drifted (tv_order_fill_lag_seconds Maximum > 10s)"
    );
}
