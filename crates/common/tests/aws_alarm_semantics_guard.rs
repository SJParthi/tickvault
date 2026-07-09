//! Ratchet: AWS CloudWatch alarm SEMANTICS pins (B5, 2026-07-03).
//!
//! Source-scan guards over the terraform files that fix two alarm-semantics
//! bugs + pin the boot-heartbeat contract:
//!
//!   1. `ebs_write_latency` (deploy/aws/terraform/alarms.tf) must be a
//!      metric-math per-op latency (Sum VolumeTotalWriteTime / Sum
//!      VolumeWriteOps × 1000), NOT the raw cumulative-seconds metric
//!      averaged against a "ms" threshold.
//!   2. The EventBridge DLQ alarm (deploy/aws/terraform/eventbridge-dlq.tf)
//!      must be edge-triggered on NEW dead-letter arrivals
//!      (NumberOfMessagesSent), NOT latched on queue depth
//!      (ApproximateNumberOfMessagesVisible) for the whole retention window.
//!   3. The DLQ queue retention must stay a bounded drain-after-inspect
//!      window (≤ 3 days).
//!   4. The boot-heartbeat alarm keeps its missing-is-breaching detection
//!      model on the dedicated tv_boot_completed signal.
//!
//! Without these pins, a future terraform edit could silently regress the
//! alarm back to a metric whose units don't match its threshold (never
//! fires) or to a depth-latch (permanently-red alarm for days).

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

/// Extract the body of a terraform `resource "<type>" "<name>" { ... }` block
/// by brace counting from the resource header line.
fn tf_resource_block(body: &str, resource_type: &str, resource_name: &str) -> String {
    let header = format!("resource \"{resource_type}\" \"{resource_name}\"");
    let start = body
        .find(&header)
        .unwrap_or_else(|| panic!("resource block {header} not found")); // APPROVED: test
    let rest = &body[start..];
    let open = rest
        .find('{')
        .expect("resource header must be followed by an opening brace"); // APPROVED: test
    let mut depth = 0usize;
    for (idx, ch) in rest[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return rest[..open + idx + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces in resource block {header}"); // APPROVED: test
}

#[test]
fn test_ebs_write_latency_alarm_uses_metric_math_per_op_latency() {
    let tf = read("deploy/aws/terraform/alarms.tf");
    let block = tf_resource_block(&tf, "aws_cloudwatch_metric_alarm", "ebs_write_latency");

    assert!(
        block.contains("metric_query"),
        "Z+ L2 VERIFY ratchet: ebs_write_latency must use metric_query blocks \
         (metric-math per-op latency), not a plain single-metric alarm. \
         VolumeTotalWriteTime alone is cumulative SECONDS per period, not ms."
    );
    assert!(
        block.contains("VolumeTotalWriteTime"),
        "ebs_write_latency must still source VolumeTotalWriteTime as the \
         numerator of the per-op latency expression."
    );
    assert!(
        block.contains("VolumeWriteOps"),
        "ebs_write_latency must divide by VolumeWriteOps — without the ops \
         denominator the metric is not a per-operation latency."
    );
    assert!(
        block.contains("(wt / ops) * 1000"),
        "ebs_write_latency expression must compute (wt / ops) * 1000 — \
         Sum write-seconds / Sum write-ops × 1000 = avg ms per write op."
    );
    assert!(
        !block.contains("statistic = \"Average\""),
        "regression pin: ebs_write_latency must NOT regress to the old \
         top-level `statistic = \"Average\"` single-metric form — that shape \
         compared cumulative seconds-per-period against a 50 'ms' threshold."
    );
}

#[test]
fn test_eventbridge_dlq_alarm_is_edge_triggered_on_new_arrivals() {
    let tf = read("deploy/aws/terraform/eventbridge-dlq.tf");
    let block = tf_resource_block(&tf, "aws_cloudwatch_metric_alarm", "eventbridge_dlq_depth");

    assert!(
        block.contains("metric_name         = \"NumberOfMessagesSent\"")
            || block.contains("NumberOfMessagesSent"),
        "Z+ L2 VERIFY ratchet: the EventBridge DLQ alarm must key on \
         NumberOfMessagesSent (fires the period a NEW message dead-letters, \
         then self-clears)."
    );
    assert!(
        !block.contains("ApproximateNumberOfMessagesVisible"),
        "regression pin: the DLQ alarm must NOT regress to \
         ApproximateNumberOfMessagesVisible — a depth metric latches the \
         alarm in ALARM for the entire SQS retention window while a stale, \
         already-inspected message sits in the queue."
    );
}

#[test]
fn test_eventbridge_dlq_retention_is_bounded_drain_after_inspect() {
    let tf = read("deploy/aws/terraform/eventbridge-dlq.tf");
    let block = tf_resource_block(&tf, "aws_sqs_queue", "eventbridge_dlq");

    let retention_line = block
        .lines()
        .find(|l| l.trim_start().starts_with("message_retention_seconds"))
        .expect("eventbridge_dlq queue must set message_retention_seconds explicitly"); // APPROVED: test
    let value: u64 = retention_line
        .split('=')
        .nth(1)
        .and_then(|v| v.trim().split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("could not parse retention from: {retention_line}")); // APPROVED: test

    assert!(
        value <= 259_200,
        "Z+ L2 VERIFY ratchet: eventbridge DLQ message_retention_seconds must \
         stay ≤ 259200 (3 days — the bounded drain-after-inspect window). \
         Found {value}."
    );
}

#[test]
fn test_boot_heartbeat_window_hands_over_to_market_hours_window() {
    // 2026-07-09 SEAM CLOSURE ratchet: the boot-heartbeat window originally
    // closed at 09:10 IST (cron(40 3)) while the market-hours liveness window
    // only opens at 09:20 IST (cron(50 3)) AND resets its alarms OK on open
    // (5-period evaluation → first possible page ~09:25-09:26). A process
    // death anywhere in [09:10, 09:20) IST — exactly spanning the 09:15
    // market open — paged nobody inside the seam and at best ~09:25 (up to
    // ~15 min blind over the highest-value window of the day). The fix moved
    // the boot-window close to 09:20 IST so coverage hands over at the exact
    // minute the market-hours window opens. This pin forces any future move
    // of EITHER cron to re-derive the handover in the same PR.
    let boot = read("deploy/aws/terraform/boot-heartbeat-alarm.tf");
    let market = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");

    let boot_open = tf_resource_block(&boot, "aws_cloudwatch_event_rule", "tv_boot_heartbeat_open");
    let boot_close = tf_resource_block(
        &boot,
        "aws_cloudwatch_event_rule",
        "tv_boot_heartbeat_close",
    );
    let market_open = tf_resource_block(
        &market,
        "aws_cloudwatch_event_rule",
        "tv_market_hours_liveness_open",
    );

    // Pin the FULL `schedule_expression = "..."` attribute line (not a bare
    // cron substring), so an in-block comment or description mentioning the
    // cron can never vacuously satisfy the pin while the schedule regresses.
    assert!(
        boot_open.contains("schedule_expression = \"cron(20 3 ? * MON-FRI *)\""),
        "boot-heartbeat OPEN must stay 08:50 IST (schedule_expression = \
         cron(20 3 ? * MON-FRI *)) — 10 min after the 08:40 IST soft boot \
         deadline (§10)."
    );
    assert!(
        boot_close.contains("schedule_expression = \"cron(50 3 ? * MON-FRI *)\""),
        "boot-heartbeat CLOSE must stay 09:20 IST (schedule_expression = \
         cron(50 3 ? * MON-FRI *)) — the exact minute the market-hours liveness \
         window opens. Moving it earlier re-opens the [close, 09:20) no-page \
         seam spanning the 09:15 market open (2026-07-09 audit finding); moving \
         it later widens the double-page overlap with the market-hours window."
    );
    assert!(
        !boot_close.contains("cron(40 3"),
        "regression pin: the boot-heartbeat close must NOT regress to 09:10 \
         IST (cron(40 3)) — that re-opens the 09:10-09:20 IST blind hole."
    );
    assert!(
        boot_close.contains("state = \"ENABLED\""),
        "the boot-heartbeat close rule must pin state = \"ENABLED\" — with the \
         attribute absent the AWS provider stops MANAGING rule state (the #1404 \
         lesson), and a once-disabled close rule leaves the breaching-on-missing \
         alarm's actions armed past 09:20 → the intentional 16:30 IST stop \
         false-pages nightly."
    );
    assert!(
        market_open.contains("schedule_expression = \"cron(50 3 ? * MON-FRI *)\""),
        "market-hours liveness OPEN must stay 09:20 IST (schedule_expression = \
         cron(50 3 ? * MON-FRI *)); if it moves, the boot-heartbeat close \
         (currently the same minute) must move WITH it in the same PR or the \
         seam re-opens."
    );
}

#[test]
fn test_boot_heartbeat_alarm_contract_unchanged() {
    let tf = read("deploy/aws/terraform/boot-heartbeat-alarm.tf");
    let block = tf_resource_block(&tf, "aws_cloudwatch_metric_alarm", "boot_heartbeat_missing");

    assert!(
        block.contains("metric_name         = \"tv_boot_completed\"")
            || block.contains("\"tv_boot_completed\""),
        "boot-heartbeat alarm must key on the dedicated tv_boot_completed \
         signal emitted by crates/app/src/main.rs::emit_boot_completed."
    );
    assert!(
        block.contains("treat_missing_data = \"breaching\"")
            || block.contains("treat_missing_data  = \"breaching\""),
        "boot-heartbeat alarm must keep treat_missing_data = \"breaching\" — \
         a MISSING heartbeat (app hung / never booted) is exactly the \
         condition it pages on; notBreaching would blind it."
    );
}
