//! Build-failing ratchet for the Muhurat box window (plan item J8, owner
//! 2026-10-26: "yes capture muhurat").
//!
//! A weekend Muhurat session is captured only if every leg agrees: the
//! terraform sessions name the same dates as the app's calendar, a dated
//! EventBridge rule runs `muhurat_check` on each date, both Lambdas receive
//! `MUHURAT_SESSIONS`, and the curfew plus the hard-stop guard treat the
//! window as in-hours. Any missing leg fails silently toward "box off, nothing
//! captured", so each one is pinned here. Pins read code lines only.

#![cfg(test)]

use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

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

/// `YYYY-MM-DD` dates quoted on lines matching `key`, from `start` to the
/// first line matching `stop`.
fn quoted_dates(body: &str, start: &str, stop: &str, key: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut inside = false;
    for line in body.lines() {
        if line.contains(start) {
            inside = true;
            continue;
        }
        if inside && !stop.is_empty() && line.trim_start().starts_with(stop) {
            break;
        }
        if inside && line.contains(key) {
            for part in line.split('"') {
                let b = part.as_bytes();
                if b.len() == 10
                    && b[4] == b'-'
                    && b[7] == b'-'
                    && part.bytes().filter(u8::is_ascii_digit).count() == 8
                {
                    out.insert(part.to_string());
                }
            }
        }
    }
    out
}

fn config_muhurat_dates() -> BTreeSet<String> {
    let toml = read("config/base.toml");
    let mut out = BTreeSet::new();
    let mut in_block = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_block = t == "[[trading.muhurat_trading_dates]]";
            continue;
        }
        if in_block
            && t.starts_with("date")
            && let Some(d) = t.split('"').nth(1)
        {
            out.insert(d.to_string());
        }
    }
    out
}

fn terraform_muhurat_dates() -> BTreeSet<String> {
    let tf = code_only(&read("deploy/aws/terraform/variables.tf"));
    quoted_dates(
        &tf,
        "variable \"muhurat_sessions\" {",
        "variable ",
        "date =",
    )
}

#[test]
fn terraform_sessions_name_exactly_the_app_calendars_muhurat_dates() {
    let config = config_muhurat_dates();
    let tf = terraform_muhurat_dates();
    assert!(
        !config.is_empty(),
        "config/base.toml lists no Muhurat date: the scan is broken"
    );
    assert_eq!(
        tf, config,
        "terraform var.muhurat_sessions and config/base.toml [[trading.muhurat_trading_dates]] \
         must name the same dates, or the box runs on a day the app does not treat as a session \
         (or never runs on one it does)"
    );
}

#[test]
fn each_date_gets_a_dated_rule_that_runs_muhurat_check() {
    let tf = code_only(&read("deploy/aws/terraform/start-watchdog-lambda.tf"));
    for needle in [
        "resource \"aws_cloudwatch_event_rule\" \"start_watchdog_muhurat_check\" {",
        "for_each = local.muhurat_dates",
        "\"cron(0/15 0-18 %d %d ? %s)\",",
        "resource \"aws_cloudwatch_event_target\" \"start_watchdog_muhurat_check\" {",
        "input = jsonencode({ mode = \"muhurat_check\" })",
        "resource \"aws_lambda_permission\" \"start_watchdog_muhurat_check\" {",
        "MUHURAT_SESSIONS = local.muhurat_sessions_env",
    ] {
        assert!(
            tf.contains(needle),
            "start-watchdog-lambda.tf is missing `{needle}`"
        );
    }
}

#[test]
fn no_weekly_weekend_cron_stands_in_for_the_dated_rule() {
    for file in [
        "deploy/aws/terraform/start-watchdog-lambda.tf",
        "deploy/aws/terraform/main.tf",
        "deploy/aws/terraform/budget-guards.tf",
    ] {
        let tf = code_only(&read(file));
        for line in tf.lines().filter(|l| l.contains("cron(")) {
            assert!(
                !(line.contains("SUN") || line.contains("SAT")),
                "{file}: `{line}` is a weekly weekend schedule; a Muhurat start must be a dated \
                 rule driven by var.muhurat_sessions"
            );
        }
    }
}

#[test]
fn the_hard_stop_guard_receives_the_sessions() {
    let tf = code_only(&read("deploy/aws/terraform/budget-guards.tf"));
    assert!(tf.contains("MUHURAT_SESSIONS = local.muhurat_sessions_env"));
}

#[test]
fn curfew_and_hard_stop_guard_treat_the_window_as_in_hours() {
    let watchdog = code_only(&read("crates/aws-lambdas/src/start_watchdog.rs"));
    assert!(watchdog.contains("if mode == \"muhurat_check\" {"));
    assert!(
        watchdog.contains(
            "if is_within_operating_hours(now)\n|| crate::muhurat_window::in_muhurat_up_window_raw(now, &env.muhurat_sessions)"
        ),
        "the curfew must skip a box inside a Muhurat window"
    );
    // 2026-10-26 (audit PR L4): the 16:30 daily_stop and the 16:45
    // stop_check skip the window too, each before it reads the instance.
    for mode in ["daily_stop", "stop_check"] {
        let arm = watchdog
            .find(&format!("if mode == \"{mode}\" {{"))
            .unwrap_or_else(|| panic!("start_watchdog.rs has no `{mode}` arm"));
        let rest = &watchdog[arm..];
        let skip = rest
            .find(
                "if crate::muhurat_window::in_muhurat_up_window_raw(now, &env.muhurat_sessions) {",
            )
            .unwrap_or_else(|| panic!("the `{mode}` arm must skip a Muhurat window"));
        let describe = rest
            .find("instance_info(ec2, &env.instance_id)")
            .unwrap_or_else(|| panic!("the `{mode}` arm reads the instance"));
        assert!(
            skip < describe,
            "the `{mode}` arm must check the Muhurat window before it acts on the box"
        );
    }
    let guard = code_only(&read("crates/aws-lambdas/src/hard_stop_guard.rs"));
    assert!(
        guard.contains(
            "if in_up_window(now_utc)\n|| crate::muhurat_window::in_muhurat_up_window_raw(now_utc, &env.muhurat_sessions)"
        ),
        "the hard-stop guard must treat a Muhurat window as in-window"
    );
}
