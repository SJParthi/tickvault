//! Build-failing ratchet pinning the **evening stop-window lockstep**.
//!
//! The operator's 9-hour trading window is 08:30–17:30 IST (operator Quote 14,
//! 2026-08-08 — the stop moved 16:30 → 17:30; the START is unchanged). FIVE
//! places encode the evening edge, and **four of them will actively fight the
//! schedule if they disagree** — this is not a cosmetic consistency test:
//!
//! | Site | If it stays at the old time |
//! |---|---|
//! | `main.tf` `daily_stop` cron | the box simply stops at the old time |
//! | `hard_stop_guard::in_up_window` | runs HOURLY and **force-stops** any box outside its window → kills the box at 17:00, pages, and silently cancels the extra hour |
//! | `start_watchdog::OPERATING_CLOSE_IST_MINUTES` | the curfew guard **stops** the box 30 min early |
//! | `start_watchdog::STOP_TRIGGER_UTC_HOUR` + its cron | stop-verify fires 45 min BEFORE the stop → a false "auto-stop FAILED" page every trading day |
//! | `.github/workflows/deploy-aws.yml` (2 branches) | a deploy that self-started the box **stops it again inside the window**, and a box down during a paid hour is reported as "intentionally stopped" |
//!
//! So a partial edit does not merely look inconsistent — it produces either a
//! prematurely-killed box or a guaranteed daily false page. A comment cannot
//! catch that; this test can.
//!
//! Authority: `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
//! §7 (schedule note).

#![cfg(test)]

use std::path::{Path, PathBuf};

use tickvault_aws_lambdas::hard_stop_guard::in_up_window;
use tickvault_aws_lambdas::start_watchdog::{
    OPERATING_CLOSE_IST_MINUTES, STOP_TRIGGER_UTC_HOUR, STOP_TRIGGER_UTC_MINUTE,
};

use chrono::{TimeZone, Utc};

/// The single source of truth for this test: 17:30 IST = 12:00 UTC.
const STOP_IST_HOUR: u32 = 17;
const STOP_IST_MINUTE: u32 = 30;
const STOP_UTC_HOUR: u32 = 12;
const STOP_UTC_MINUTE: u32 = 0;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/aws-lambdas parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// IST is a fixed +05:30 offset, so the UTC/IST pair must be self-consistent.
/// If someone edits one half of a cron they must edit the constant too.
#[test]
fn stop_time_utc_and_ist_agree() {
    let ist_minutes = STOP_IST_HOUR * 60 + STOP_IST_MINUTE;
    let utc_minutes = STOP_UTC_HOUR * 60 + STOP_UTC_MINUTE;
    assert_eq!(
        ist_minutes,
        utc_minutes + 5 * 60 + 30,
        "17:30 IST must equal 12:00 UTC (+05:30) — this test's own anchors drifted"
    );
}

/// The Rust stop-trigger constants must match the terraform cron.
#[test]
fn start_watchdog_stop_trigger_matches_the_cron() {
    assert_eq!(
        STOP_TRIGGER_UTC_HOUR, STOP_UTC_HOUR,
        "start_watchdog::STOP_TRIGGER_UTC_HOUR must match main.tf daily_stop cron hour"
    );
    assert_eq!(STOP_TRIGGER_UTC_MINUTE, STOP_UTC_MINUTE);

    let tf = read(&repo_root().join("deploy/aws/terraform/main.tf"));
    let expected = format!("cron({STOP_UTC_MINUTE} {STOP_UTC_HOUR} ? * MON-FRI *)");
    assert!(
        tf.contains(&expected),
        "main.tf must schedule daily_stop at `{expected}` (= {STOP_IST_HOUR}:{STOP_IST_MINUTE} IST)"
    );
    assert!(
        !tf.contains("cron(0 11 ? * MON-FRI *)"),
        "main.tf still carries the OLD 16:30 IST daily_stop cron — the box would stop an hour early"
    );
}

/// The stop-VERIFY must fire AFTER the stop, never before.
///
/// This is the concrete failure the old timing produced: a verify at 16:45
/// against a 17:30 stop reports "auto-stop FAILED" while the box is legitimately
/// still running — a false page every single trading day.
#[test]
fn stop_verify_fires_after_the_stop() {
    let tf = read(&repo_root().join("deploy/aws/terraform/start-watchdog-lambda.tf"));

    // Collect every MON-FRI cron in the file and find the stop_check one by
    // requiring it to be strictly after the stop trigger.
    let mut verify: Option<(u32, u32)> = None;
    for line in tf.lines() {
        let t = line.trim_start();
        if t.starts_with('#') || !t.contains("schedule_expression") {
            continue;
        }
        let Some(open) = t.find("cron(") else {
            continue;
        };
        let rest = &t[open + 5..];
        let Some(close) = rest.find(')') else {
            continue;
        };
        let fields: Vec<&str> = rest[..close].split_whitespace().collect();
        if fields.len() < 5 || fields[4] != "MON-FRI" {
            continue;
        }
        let (Ok(m), Ok(h)) = (fields[0].parse::<u32>(), fields[1].parse::<u32>()) else {
            continue;
        };
        // The stop_check is the only MON-FRI schedule in the evening band.
        if h >= STOP_UTC_HOUR {
            verify = Some((h, m));
        }
    }
    let (vh, vm) = verify.expect(
        "start-watchdog-lambda.tf must carry an evening stop_check cron at or after the stop hour",
    );
    let verify_minutes = vh * 60 + vm;
    let stop_minutes = STOP_UTC_HOUR * 60 + STOP_UTC_MINUTE;
    assert!(
        verify_minutes > stop_minutes,
        "stop_check fires at {vh:02}:{vm:02} UTC but the stop is {STOP_UTC_HOUR:02}:{STOP_UTC_MINUTE:02} UTC. \
         Verifying BEFORE the stop reports a false 'auto-stop FAILED' every trading day."
    );
}

/// The curfew guard must not close before the scheduled stop.
#[test]
fn curfew_close_is_not_before_the_stop() {
    let stop_ist_minutes = STOP_IST_HOUR * 60 + STOP_IST_MINUTE;
    assert!(
        OPERATING_CLOSE_IST_MINUTES >= stop_ist_minutes,
        "start_watchdog::OPERATING_CLOSE_IST_MINUTES is {OPERATING_CLOSE_IST_MINUTES} \
         ({}:{:02} IST) but the scheduled stop is {STOP_IST_HOUR}:{STOP_IST_MINUTE} IST. \
         The curfew guard STOPS the box outside operating hours, so a close before the \
         stop kills the box early every day.",
        OPERATING_CLOSE_IST_MINUTES / 60,
        OPERATING_CLOSE_IST_MINUTES % 60
    );
}

/// The hourly never-cross guard must consider the whole scheduled window "up".
///
/// `in_up_window` is what force-stops a running box. If its upper bound is
/// below the scheduled stop, the guard becomes the thing that ends the session.
#[test]
fn hard_stop_up_window_covers_the_whole_scheduled_day() {
    // 2026-08-11 is a Tuesday. Build UTC instants and check the IST semantics.
    let at = |h: u32, m: u32| {
        Utc.with_ymd_and_hms(2026, 8, 11, h, m, 0)
            .single()
            .expect("valid UTC instant")
    };

    // 03:00 UTC = 08:30 IST — the scheduled START must be inside.
    assert!(
        in_up_window(at(3, 0)),
        "08:30 IST (the scheduled start) must be inside the up-window"
    );
    // 11:59 UTC = 17:29 IST — one minute before the stop must be inside.
    assert!(
        in_up_window(at(11, 59)),
        "17:29 IST must be inside the up-window — otherwise the hourly guard \
         force-stops the box before its scheduled stop"
    );
    // 12:00 UTC = 17:30 IST — the stop minute itself is inclusive.
    assert!(
        in_up_window(at(12, 0)),
        "17:30 IST (the scheduled stop minute) must be inside the up-window"
    );
    // 12:01 UTC = 17:31 IST — past the stop, the guard SHOULD act.
    assert!(
        !in_up_window(at(12, 1)),
        "17:31 IST must be OUTSIDE the up-window — the guard is the backstop for \
         a failed stop, so it must not stay permissive all evening"
    );
    // Weekends are always out, regardless of clock.
    // 2026-08-15 is a Saturday.
    let sat = Utc
        .with_ymd_and_hms(2026, 8, 15, 6, 0, 0)
        .single()
        .expect("valid UTC instant");
    assert!(
        !in_up_window(sat),
        "Saturday must always be outside the up-window"
    );
}

/// The operator-facing strings must state the CURRENT stop time.
///
/// Deliberately asserts the POSITIVE, not the absence of "16:30". A first
/// draft of this test banned the old time anywhere in the file and immediately
/// failed on its own re-blessing comment — because house convention REQUIRES
/// dated history to name the value it superseded ("the banner read 16:30 …").
/// Banning the old string would have forced deleting that provenance, which is
/// the opposite of what the rules want. What actually matters is that the
/// string the OPERATOR reads is correct, so that is what is checked.
#[test]
fn operator_facing_messages_state_the_current_stop_time() {
    let root = repo_root();
    let stop = format!("{STOP_IST_HOUR}:{STOP_IST_MINUTE} IST");

    // The console HTML holds the real banner the operator sees.
    let html = read(&root.join("crates/aws-lambdas/src/operator_control_console.html"));
    let banner = html
        .lines()
        .find(|l| l.contains("stoppedbanner"))
        .expect("console HTML must carry the stopped-box banner");
    assert!(
        banner.contains(&format!("auto-stops {stop}")),
        "the stopped-box banner must say `auto-stops {stop}`; it reads:\n  {banner}"
    );

    // The console offline message and the operator-control assertion must agree.
    let front = read(&root.join("crates/aws-lambdas/src/qdb_console_front.rs"));
    assert!(
        front.contains(&format!("08:30\u{2013}{stop}")),
        "qdb_console_front OFFLINE_MSG must state the 08:30-{stop} window"
    );
    let ctl = read(&root.join("crates/aws-lambdas/src/operator_control.rs"));
    assert!(
        ctl.contains(&format!("auto-stops {stop}")),
        "operator_control must assert the banner's CURRENT text (`auto-stops {stop}`)"
    );
}

/// The 12-hour clock times inside the operator-facing SNS/Telegram bodies are a
/// SEVENTH lockstep site, and the one a reader trusts most.
///
/// Added 2026-08-08 (operator Quote 14) after finding six live message strings
/// still reading "4:30 PM IST auto-stop" while EventBridge had moved to 17:30.
/// Nothing crashes on a stale string — it just tells the operator the wrong
/// time, in the exact place the 10 Telegram commandments demand a precise IST
/// 12-hour time. That is a silent-wrong-answer bug, which is worse than a loud
/// one, so it gets a build-failing pin like every other coupled site.
///
/// The pin is derived from `STOP_IST_HOUR`/`STOP_IST_MINUTE` (themselves derived
/// from the Lambda's own UTC trigger constants), so it tracks any future move
/// automatically instead of hardcoding another literal that can rot.
#[test]
fn operator_facing_stop_times_are_in_lockstep() {
    let root = repo_root();
    let body = read(&root.join("crates/aws-lambdas/src/start_watchdog.rs"));

    // 24h -> 12h with meridiem, the form the message bodies actually use.
    let (h12, meridiem) = match STOP_IST_HOUR {
        0 => (12, "AM"),
        h if h < 12 => (h, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    };
    let current = format!("{h12}:{STOP_IST_MINUTE:02} {meridiem} IST");

    assert!(
        body.contains(&current),
        "start_watchdog.rs operator-facing bodies must name the CURRENT stop \
         time as `{current}` (derived from the Lambda's own UTC trigger \
         constants). A body naming any other time tells the operator the wrong \
         hour — commandment 9 requires a precise IST 12-hour time."
    );

    // Inverted pin: the retired 4:30 PM text must never come back. Every
    // occurrence would be operator-facing, so one is already too many.
    assert!(
        !body.contains("4:30 PM"),
        "the retired 4:30 PM IST stop time must not reappear in any \
         operator-facing body (operator widened the window to 5:30 PM IST on \
         2026-08-08, Quote 14)."
    );
}

/// The FIFTH site: `.github/workflows/deploy-aws.yml`.
///
/// Added 2026-08-27. The four sites this file was written for all live under
/// `deploy/aws/terraform/**` or `crates/aws-lambdas/src/**`, and every path this
/// guard reads is one of those two. The deploy workflow encodes the SAME evening
/// edge in two live `sh` branches and was never scanned, so it kept the old
/// 16:30 for nineteen days after operator Quote 14 moved the window.
///
/// Both branches decide something real:
///
/// | Branch | With the old 16:30 |
/// |---|---|
/// | `UP_WINDOW` (stopped-box decision) | a box that is down during a PAID hour is classified "intentionally stopped": the deploy skips GREEN and SILENT — a false-OK |
/// | the auto-stop guard | a dispatch-deploy that self-started the box **stops it again at e.g. 17:00**, inside the window, under a comment reading "NEVER stop during the trading window" |
///
/// The expected boundary is DERIVED from this file's own `STOP_IST_*` constants,
/// so moving the window moves this assertion with it and cannot drift again.
#[test]
fn deploy_workflow_encodes_the_same_evening_edge() {
    let wf = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let expected = STOP_IST_HOUR * 100 + STOP_IST_MINUTE;

    // Only LIVE shell lines count. A comment carrying an old figure is stale
    // prose, not a decision, and this guard exists for decisions.
    let live: Vec<(usize, &str)> = wf
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .filter(|(_, l)| !l.trim_start().starts_with('#'))
        // Select ONLY the up-window branches. The file also carries the
        // market-hours BLACKOUT guard (`-ge 900 ... -lt 1545`), which is a
        // different window and must NOT be dragged to the stop edge — an
        // earlier draft of this filter caught it and would have failed on a
        // correct line. The up-window's START (08:30) is what distinguishes
        // them, and Quote 14 left that start unchanged.
        .filter(|(_, l)| l.contains("$HHMM") && l.contains("-ge 830"))
        .collect();

    assert!(
        live.len() >= 2,
        "expected at least the two window branches in deploy-aws.yml (UP_WINDOW and the \
         auto-stop guard); found {} — if a branch was removed, remove it here too rather \
         than weakening the count",
        live.len()
    );

    for (lineno, line) in &live {
        assert!(
            line.contains(&format!("-le {expected}")),
            "deploy-aws.yml:{lineno} compares against the wrong evening edge — the window \
             closes at {STOP_IST_HOUR}:{STOP_IST_MINUTE:02} IST, so this must read \
             `-le {expected}`. Line: {}",
            line.trim()
        );
    }

    // Belt and braces: no live line anywhere may carry the retired figure.
    for (i, line) in wf.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        assert!(
            !line.contains("1630"),
            "deploy-aws.yml:{} still carries the retired 16:30 IST edge on a LIVE line. \
             Operator Quote 14 (2026-08-08) moved the window to 17:30. Line: {}",
            i + 1,
            line.trim()
        );
    }
}
