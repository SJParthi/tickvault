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

/// `(date, start, end)` for every `[[trading.muhurat_trading_dates]]` entry in
/// `config/base.toml`; missing times read as empty strings.
fn config_muhurat_sessions() -> BTreeSet<(String, String, String)> {
    let toml = read("config/base.toml");
    let mut out = BTreeSet::new();
    let mut current: Option<(String, String, String)> = None;
    let mut in_block = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if let Some(entry) = current.take() {
                out.insert(entry);
            }
            in_block = t == "[[trading.muhurat_trading_dates]]";
            if in_block {
                current = Some((String::new(), String::new(), String::new()));
            }
            continue;
        }
        let Some(entry) = current.as_mut().filter(|_| in_block) else {
            continue;
        };
        let value = t.split('"').nth(1).unwrap_or("").to_string();
        if t.starts_with("date") {
            entry.0 = value;
        } else if t.starts_with("session_start_ist") {
            entry.1 = value;
        } else if t.starts_with("session_end_ist") {
            entry.2 = value;
        }
    }
    if let Some(entry) = current.take() {
        out.insert(entry);
    }
    out
}

/// `(date, start, end)` for every entry of terraform's `muhurat_sessions`
/// default (one `{ date = .., session_start_ist = .., session_end_ist = .. }`
/// per line).
fn terraform_muhurat_sessions() -> BTreeSet<(String, String, String)> {
    let tf = code_only(&read("deploy/aws/terraform/variables.tf"));
    let mut out = BTreeSet::new();
    let mut inside = false;
    for line in tf.lines() {
        if line.contains("variable \"muhurat_sessions\" {") {
            inside = true;
            continue;
        }
        if inside && line.trim_start().starts_with("variable ") {
            break;
        }
        if inside && line.contains("date =") && line.contains("session_start_ist =") {
            let parts: Vec<&str> = line.split('"').collect();
            assert!(
                parts.len() >= 7,
                "variables.tf muhurat_sessions entry `{line}` is not one-line date/start/end"
            );
            out.insert((
                parts[1].to_string(),
                parts[3].to_string(),
                parts[5].to_string(),
            ));
        }
    }
    out
}

/// 2026-10-30 (owner: "go ahead and fix the muhurat risk dude"): the session
/// TIMES, not only the dates, have one source. Terraform starts the box and
/// arms the liveness gate from its copy; the app calendar and the build
/// checks read config/base.toml. A drift between them starts the box, or arms
/// the alarms, at an hour the app does not consider the session.
#[test]
fn terraform_session_times_equal_the_app_calendars_times() {
    let config = config_muhurat_sessions();
    let tf = terraform_muhurat_sessions();
    assert!(
        !config.is_empty(),
        "config/base.toml lists no Muhurat entry"
    );
    assert_eq!(
        tf, config,
        "terraform var.muhurat_sessions (date, start, end) must equal config/base.toml \
         [[trading.muhurat_trading_dates]] (date, session_start_ist, session_end_ist)"
    );
}

/// The workflows' deploy freeze reads the line right after the header
/// (`grep -A1`), so the date must stay first in every Muhurat block.
#[test]
fn config_muhurat_blocks_put_the_date_first() {
    let toml = read("config/base.toml");
    let lines: Vec<&str> = toml.lines().collect();
    let mut blocks = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "[[trading.muhurat_trading_dates]]" {
            blocks += 1;
            // The workflows match both lines EXACTLY (`grep -xF`): no indent,
            // no trailing comment. A looser check here would let an edit pass
            // that makes every guard read the date as an ordinary day.
            assert_eq!(
                *line, "[[trading.muhurat_trading_dates]]",
                "a Muhurat header must have no indent or trailing text (grep -xF)"
            );
            let next = lines.get(i + 1).copied().unwrap_or("");
            let date = next
                .strip_prefix("date = \"")
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or("");
            let b = date.as_bytes();
            assert!(
                b.len() == 10
                    && b[4] == b'-'
                    && b[7] == b'-'
                    && date.bytes().filter(u8::is_ascii_digit).count() == 8,
                "the line after a [[trading.muhurat_trading_dates]] header must be exactly \
                 `date = \"YYYY-MM-DD\"` (no indent, no trailing comment): the workflows read \
                 it with grep -A1 | grep -xF; found `{next}`"
            );
        }
    }
    assert!(blocks > 0, "no Muhurat block found");
}

/// The dated liveness arming: one open (session start + 20 min) and one close
/// (session end) per date, on the SAME gate Lambda, leaving the three
/// weekday-only did-it-run alarms disarmed.
#[test]
fn each_muhurat_date_arms_and_disarms_the_liveness_gate() {
    let tf = code_only(&read("deploy/aws/terraform/market-hours-liveness-alarm.tf"));
    for needle in [
        "muhurat_liveness_open_offset_minutes = 20",
        "for s in var.muhurat_sessions : s.date => {",
        "} if s.session_start_ist != \"\"",
        "resource \"aws_cloudwatch_event_rule\" \"tv_market_hours_liveness_muhurat_open\" {",
        "resource \"aws_cloudwatch_event_rule\" \"tv_market_hours_liveness_muhurat_close\" {",
        "\"cron(%d %d %d %d ? %s)\",",
        "arn = aws_lambda_function.tv_market_hours_liveness_gate.arn",
        "input = jsonencode({ mode = \"close\" })",
        "skip_alarms = [",
        "aws_cloudwatch_metric_alarm.deploy_watchdog_not_invoked.alarm_name,",
        "aws_cloudwatch_metric_alarm.market_open_readiness_not_invoked.alarm_name,",
        "aws_cloudwatch_metric_alarm.boot_heartbeat_gate_not_invoked.alarm_name,",
        "resource \"aws_lambda_permission\" \"tv_market_hours_liveness_muhurat_open\" {",
        "resource \"aws_lambda_permission\" \"tv_market_hours_liveness_muhurat_close\" {",
    ] {
        assert!(
            tf.contains(needle),
            "market-hours-liveness-alarm.tf is missing `{needle}`"
        );
    }
    let gate = read("crates/aws-lambdas/src/market_hours_gate.rs");
    let open = gate
        .find("// --- mode == open ---")
        .expect("market_hours_gate.rs open arm");
    assert!(
        gate[open..].contains("let alarm_names = alarms_to_open(&alarm_names, &event);"),
        "the gate's open arm must apply the event's skip list"
    );
}
