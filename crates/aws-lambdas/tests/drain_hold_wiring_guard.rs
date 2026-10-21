//! Build-failing ratchet for the **drain hold** (operator Quote 31,
//! 2026-10-21): after the close the box keeps running until the app reports
//! the day's data is in the database.
//!
//! The app writes `/tickvault/<env>/drain-hold`; four stops read it (the
//! 17:30 daily stop, the 17:45 stop check, the hourly curfew, the hard-stop
//! guard's out-of-window stop). Every reader fails toward STOPPING, so a
//! missing grant or env var degrades silently to the old behaviour: the box
//! stops at 17:30 with the day half-written and nothing says why. That is
//! why each leg is pinned here. The budget stop must NEVER wait on the hold,
//! and that is pinned too.
//!
//! Every pin reads CODE lines only (comment lines are dropped first), so a
//! dated comment can never satisfy one.

#![cfg(test)]

use std::path::{Path, PathBuf};

const HOLD_PATH: &str = "/tickvault/${var.environment}/drain-hold";
const HOLD_ARN_TAIL: &str = "parameter/tickvault/${var.environment}/drain-hold";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/aws-lambdas parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path: &Path = &repo_root().join(rel);
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// The file with comment lines removed and every run of spaces collapsed to
/// one, so HCL alignment padding never breaks a pin.
fn code_only(body: &str) -> String {
    body.lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with('#') || t.starts_with("//"))
        })
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn position(body: &str, needle: &str, what: &str) -> usize {
    body.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

/// The text of one terraform resource block, from its header up to the next
/// top-level `resource` (nested blocks also close on a bare `}` once
/// indentation is collapsed, so the next header is the reliable end).
fn tf_block<'a>(tf: &'a str, header: &str) -> &'a str {
    let start = position(tf, header, "terraform block");
    let rest = &tf[start..];
    let end = rest[1..].find("\nresource ").map_or(rest.len(), |i| i + 1);
    &rest[..end]
}

#[test]
fn the_daily_stop_target_is_the_start_watchdog_in_daily_stop_mode() {
    let tf = code_only(&read("deploy/aws/terraform/start-watchdog-lambda.tf"));
    let block = tf_block(
        &tf,
        "resource \"aws_cloudwatch_event_target\" \"daily_stop\" {",
    );
    for needle in [
        "rule = aws_cloudwatch_event_rule.daily_stop.name",
        "arn = aws_lambda_function.start_watchdog.arn",
        "input = jsonencode({ mode = \"daily_stop\" })",
        "dead_letter_config {",
        "retry_policy {",
    ] {
        assert!(
            block.contains(needle),
            "the daily_stop target must carry `{needle}`; block:\n{block}"
        );
    }
    let permission = tf_block(
        &tf,
        "resource \"aws_lambda_permission\" \"start_watchdog_daily_stop\" {",
    );
    assert!(
        permission.contains("source_arn = aws_cloudwatch_event_rule.daily_stop.arn")
            && permission.contains("principal = \"events.amazonaws.com\""),
        "the daily_stop rule must be allowed to invoke the start-watchdog"
    );

    // The old SSM stop target must be gone, or the box is stopped twice and
    // the hold is ignored by one of them.
    let main = code_only(&read("deploy/aws/terraform/main.tf"));
    assert!(
        !main.contains("resource \"aws_cloudwatch_event_target\" \"daily_stop\""),
        "main.tf must not carry a second daily_stop target"
    );
    assert!(
        !main.contains("ssm_stop_arn"),
        "main.tf must not point anything at the SSM stop automation"
    );
    assert!(
        main.contains("resource \"aws_cloudwatch_event_rule\" \"daily_stop\""),
        "the daily_stop RULE stays in main.tf"
    );
}

#[test]
fn both_lambdas_read_the_drain_hold_parameter() {
    for rel in [
        "crates/aws-lambdas/src/start_watchdog.rs",
        "crates/aws-lambdas/src/hard_stop_guard.rs",
    ] {
        let rs = code_only(&read(rel));
        assert!(
            rs.contains("std::env::var(\"DRAIN_HOLD_PARAM\")"),
            "{rel} must read DRAIN_HOLD_PARAM from the environment"
        );
        assert!(
            rs.contains("crate::drain_hold::drain_hold_verdict("),
            "{rel} must judge the hold with the shared drain_hold verdict"
        );
    }
    for rel in [
        "deploy/aws/terraform/start-watchdog-lambda.tf",
        "deploy/aws/terraform/budget-guards.tf",
    ] {
        let tf = code_only(&read(rel));
        assert!(
            tf.contains(&format!("DRAIN_HOLD_PARAM = \"{HOLD_PATH}\"")),
            "{rel} must pass DRAIN_HOLD_PARAM"
        );
    }
}

#[test]
fn both_lambda_roles_may_read_the_drain_hold_and_never_write_it() {
    let watchdog = code_only(&read("deploy/aws/terraform/start-watchdog-lambda.tf"));
    let policy = tf_block(
        &watchdog,
        "resource \"aws_iam_role_policy\" \"start_watchdog\" {",
    );
    assert!(
        policy.contains(HOLD_ARN_TAIL),
        "the start-watchdog role must be able to GetParameter the drain hold"
    );
    assert!(
        !policy.contains("ssm:PutParameter"),
        "the start-watchdog role must never write SSM"
    );

    let guard = code_only(&read("deploy/aws/terraform/budget-guards.tf"));
    let sid = position(&guard, "Sid = \"ReadDrainHold\"", "guard IAM");
    let statement = &guard[sid..];
    let statement = &statement[..statement.find("\n}").unwrap_or(statement.len())];
    assert!(
        statement.contains("\"ssm:GetParameter\"") && statement.contains(HOLD_ARN_TAIL),
        "the hard-stop guard role must GetParameter the drain hold, scoped to its one ARN"
    );
    assert!(
        !statement.contains("PutParameter"),
        "the hard-stop guard must only READ the drain hold"
    );
}

/// The budget stop is NEVER gated on the hold (Quote 31's first REJECT row).
/// In `run_guard` the in-window branch, which holds the breach stop, returns
/// before the drain hold is read at all.
#[test]
fn the_budget_breach_stop_precedes_any_drain_hold_read() {
    let rs = code_only(&read("crates/aws-lambdas/src/hard_stop_guard.rs"));
    let run_start = position(&rs, "pub async fn run_guard", "guard");
    let run_end = run_start
        + rs[run_start..]
            .find("pub struct SdkEc2")
            .expect("SdkEc2 follows run_guard");
    let run = &rs[run_start..run_end];
    let in_window = position(run, "if in_up_window(now_utc) {", "in-window branch");
    let breach = position(run, "return execute_breach_stop(", "breach stop");
    let hold = position(
        run,
        "read_drain_hold(ssm, &env.drain_hold_param, now_utc)",
        "out-of-window drain-hold read",
    );
    assert!(
        in_window < breach && breach < hold,
        "the in-window budget breach stop must be evaluated (and return) before \
         the drain hold is read"
    );
    // An ACTIVE hold is honoured only after the same budget evaluation runs
    // and, on a breach, the same breach stop is taken (review fix
    // 2026-10-21): the hold read is followed by a second breach stop that
    // precedes the `skipped: drain_hold` return.
    let after_hold = &run[hold..];
    let classify = position(
        after_hold,
        "classify_in_window_action(usd, effective_env.budget_kill_usd)",
        "budget check under a hold",
    );
    let held_breach = position(after_hold, "return execute_breach_stop(", "held breach");
    let honoured = position(after_hold, "\"skipped\": \"drain_hold\"", "hold honoured");
    assert!(
        classify < held_breach && held_breach < honoured,
        "under an active hold the budget must be checked and a breach stopped \
         BEFORE the hold is honoured"
    );
    assert_eq!(
        run.matches("drain_hold_param").count(),
        1,
        "run_guard must read the drain hold exactly once, on the out-of-window arm"
    );

    for fn_name in [
        "pub async fn execute_breach_stop",
        "pub fn classify_in_window_action",
    ] {
        let start = position(&rs, fn_name, "budget path");
        let body = &rs[start..];
        let body = &body[..body[1..].find("\npub ").map_or(body.len(), |i| i + 1)];
        assert!(
            !body.contains("drain_hold"),
            "`{fn_name}` must never consult the drain hold"
        );
    }

    let killswitch = code_only(&read("crates/aws-lambdas/src/budget_killswitch.rs"));
    assert!(
        !killswitch.contains("drain_hold"),
        "the AWS-Budgets kill-switch must never consult the drain hold"
    );
}

#[test]
fn the_watchdog_reads_the_hold_before_each_scheduled_stop() {
    let rs = code_only(&read("crates/aws-lambdas/src/start_watchdog.rs"));
    let run_start = position(&rs, "pub async fn run_watchdog", "watchdog");
    let run = &rs[run_start..];
    for (mode, next_mode) in [
        ("if mode == \"daily_stop\" {", "if mode == \"stop_check\" {"),
        (
            "if mode == \"stop_check\" {",
            "if mode == \"curfew_check\" {",
        ),
        (
            // The default `check` branch follows the curfew; its holiday
            // marker read is its first SSM call.
            "if mode == \"curfew_check\" {",
            "if holiday_stop_is_today(",
        ),
    ] {
        let start = position(run, mode, "watchdog mode");
        let body = &run[start..];
        let end = body[1..].find(next_mode).map_or(body.len(), |i| i + 1);
        let body = &body[..end];
        let hold = position(
            body,
            "read_drain_hold(ssm, &env.drain_hold_param, now)",
            mode,
        );
        let stop = position(body, "try_self_stop(ec2, &env.instance_id)", mode);
        assert!(
            hold < stop,
            "`{mode}` must read the drain hold before it stops the box"
        );
    }
}
