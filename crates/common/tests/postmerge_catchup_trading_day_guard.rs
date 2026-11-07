//! The post-merge catch-up never causes a box start (incident Sat
//! 2026-11-07; owner: "today is saturday why the fuck server started").
//!
//! Two bot merges had no push run, so `postmerge-catchup.yml` dispatched
//! `deploy-aws.yml` (runs 42311495097, created 04:52 IST, and 42331877828,
//! created 08:29 IST). A `workflow_dispatch` sets SHOULD_BE_UP=yes in
//! deploy-aws's "Ensure instance running" step, so each run self-started the
//! stopped box, deployed and stopped it again.
//!
//! The catch-up now dispatches `deploy-aws.yml` only on an ordinary IST
//! trading day (never a Saturday, Sunday, `[[trading.nse_holidays]]` date or
//! `[[trading.muhurat_trading_dates]]` date: deploy-aws's market-hours guard
//! refuses the whole Muhurat date, so a dispatch there only leaves a red run
//! that counts as covering the SHA) AND only between 15:45 and 16:05
//! IST, when the box is up by schedule (it stops at 16:30), so deploy-aws
//! finds it running. Outside that it dispatches nothing, so no run exists for
//! the SHA and the next in-window firing or the after-close crons deploy it.
//! `deploy-aws.yml` itself is unchanged, so a human dispatch still deploys at
//! any time.
//!
//! This file pins the shell (source scan), the cron slots, the calendar shape
//! the shell relies on, and a Rust model of the decision. The shell itself
//! was run by hand on 2026-11-07 against `config/base.toml` for the same rows
//! as `decision_model_matches_the_shell_rows`.

use std::path::PathBuf;

const CATCHUP: &str = ".github/workflows/postmerge-catchup.yml";
const DEPLOY: &str = ".github/workflows/deploy-aws.yml";
const CALENDAR: &str = "config/base.toml";

/// IST date, weekday and time from ONE clock read, plus the window test.
const INPUTS_AND_WINDOW: &str = "read -r ist_day ist_dow ist_hhmm < <(TZ=Asia/Kolkata date '+%F %u %H%M'); deploy_day=yes; in_window=no; if [ \"$((10#${ist_hhmm}))\" -ge 1545 ] && [ \"$((10#${ist_hhmm}))\" -le 1605 ]; then in_window=yes; fi";

/// The deploy-day decision: a weekend, an NSE holiday or a Muhurat date is
/// not a deploy day.
const DEPLOY_DAY_LINE: &str = "if [ \"${ist_dow}\" -ge 6 ] || grep -qxF \"date = \\\"${ist_day}\\\"\" <(grep -A1 -xF -e '[[trading.nse_holidays]]' -e '[[trading.muhurat_trading_dates]]' config/base.toml 2>/dev/null); then deploy_day=no; fi";

/// The skip inside the dispatch loop: either condition holds the dispatch.
const DEPLOY_SKIP: &str = "if [ \"${wf}\" = \"deploy-aws.yml\" ] && { [ \"${deploy_day}\" = \"no\" ] || [ \"${in_window}\" = \"no\" ]; }; then";

/// The dispatch the skip must come before.
const DISPATCH: &str = "if gh workflow run \"${wf}\" --ref main; then";

/// The window, IST `HHMM`, inclusive. 15:45 = the earliest swap the
/// market-hours guard allows; 16:05 + ~14.5 min (measured dispatch to done)
/// ends before the 16:30 stop.
const WINDOW_START_HHMM: u32 = 1545;
const WINDOW_END_HHMM: u32 = 1605;

fn read(rel: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

fn find(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{CATCHUP}: {what} not found: `{needle}`"))
}

/// No dispatch on a non-trading day and none outside the window: both are
/// decided before the loop, and one skip inside the loop holds the deploy-aws
/// dispatch on either. Deleting either condition fails this test.
#[test]
fn catchup_dispatches_deploy_only_on_a_trading_day_inside_the_window() {
    let wf = read(CATCHUP);
    let inputs = find(&wf, INPUTS_AND_WINDOW, "IST clock read + window test");
    let decide = find(&wf, DEPLOY_DAY_LINE, "deploy-day decision");
    let loop_start = find(
        &wf,
        "for wf in ci.yml bench.yml deploy-aws.yml terraform-apply.yml; do",
        "dispatch loop",
    );
    let skip = find(&wf, DEPLOY_SKIP, "deploy-aws trading-day / window skip");
    let dispatch = find(&wf, DISPATCH, "workflow dispatch");
    assert!(
        inputs < decide && decide < loop_start && loop_start < skip && skip < dispatch,
        "{CATCHUP}: the trading day and the window must be decided before the \
         dispatch loop, and the deploy-aws skip must sit inside the loop before \
         `gh workflow run`"
    );
    let skip_line = wf[skip..].lines().next().unwrap_or_default();
    assert!(
        skip_line.trim_end().ends_with("continue; fi"),
        "{CATCHUP}: the deploy-aws skip must `continue` (no dispatch): `{skip_line}`"
    );
    // The window constants in the shell are the ones this file models.
    assert!(
        INPUTS_AND_WINDOW.contains(&format!("-ge {WINDOW_START_HHMM} ]"))
            && INPUTS_AND_WINDOW.contains(&format!("-le {WINDOW_END_HHMM} ]")),
        "window bounds drifted between the shell and this test"
    );
}

#[test]
fn only_the_deploy_lane_is_held() {
    let wf = read(CATCHUP);
    assert_eq!(
        wf.matches("\"${deploy_day}\" = \"no\"").count(),
        1,
        "{CATCHUP}: only the deploy-aws lane may be held on a non-deploy day; \
         ci.yml, bench.yml and terraform-apply.yml start no box"
    );
    assert_eq!(
        wf.matches("\"${in_window}\" = \"no\"").count(),
        1,
        "{CATCHUP}: only the deploy-aws lane may be held outside the window"
    );
    assert!(
        !wf.contains("gh workflow run \"${wf}\" --ref main -f"),
        "{CATCHUP}: the catch-up dispatch must not pass inputs that change deploy-aws"
    );
}

/// At least one weekday cron slot lands inside the window (UTC + 05:30), so
/// the catch-up can fire there at all; the `*/30` slot adds 16:00 IST.
#[test]
fn a_weekday_cron_slot_lands_inside_the_window() {
    let wf = read(CATCHUP);
    let mut inside = Vec::new();
    for line in wf.lines() {
        let Some(rest) = line.trim().strip_prefix("- cron: '") else {
            continue;
        };
        let expr = rest.trim_end_matches('\'');
        let fields: Vec<&str> = expr.split_whitespace().collect();
        assert_eq!(fields.len(), 5, "{CATCHUP}: bad cron `{expr}`");
        let (Ok(minute), Ok(hour)) = (fields[0].parse::<u32>(), fields[1].parse::<u32>()) else {
            continue;
        };
        let ist = (hour * 60 + minute + 330) % (24 * 60);
        let hhmm = (ist / 60) * 100 + ist % 60;
        if fields[4] == "1-5" && (WINDOW_START_HHMM..=WINDOW_END_HHMM).contains(&hhmm) {
            inside.push(hhmm);
        }
    }
    assert!(
        inside.len() >= 2,
        "{CATCHUP}: needs at least two weekday cron slots inside \
         {WINDOW_START_HHMM}-{WINDOW_END_HHMM} IST, found {inside:?}"
    );
    assert!(
        wf.contains("- cron: '*/30 * * * *'"),
        "{CATCHUP}: the */30 slot backfills ci/bench and adds 16:00 IST"
    );
}

#[test]
fn catchup_reads_the_calendar_before_the_probe_without_failing_the_job() {
    let wf = read(CATCHUP);
    let read_step = find(
        &wf,
        "- name: Read the NSE calendar (holidays + Muhurat dates)",
        "calendar checkout step",
    );
    let probe = find(
        &wf,
        "- name: Backfill main HEAD if it has no ci.yml / bench.yml run",
        "probe step",
    );
    assert!(
        read_step < probe,
        "{CATCHUP}: the calendar must be checked out before the probe"
    );
    let step = &wf[read_step..probe];
    for needle in [
        "continue-on-error: true",
        "uses: actions/checkout@",
        "sparse-checkout: config/base.toml",
        "persist-credentials: false",
        "ref: main",
    ] {
        assert!(
            step.contains(needle),
            "{CATCHUP}: calendar step lacks `{needle}`"
        );
    }
}

#[test]
fn a_human_dispatch_of_deploy_aws_still_starts_the_box_at_any_time() {
    let wf = read(DEPLOY);
    assert!(
        wf.contains(
            "if [ \"$UP_WINDOW\" = \"yes\" ] || [ \"$EVENT_NAME\" = \"workflow_dispatch\" ]; then SHOULD_BE_UP=yes; fi"
        ),
        "{DEPLOY}: a workflow_dispatch must still set SHOULD_BE_UP=yes, so a human \
         dispatch deploys on a weekend, a holiday or at night"
    );
}

/// Every `[[trading.nse_holidays]]` and `[[trading.muhurat_trading_dates]]`
/// header is followed by its `date = "YYYY-MM-DD"` line: the shell reads the
/// date with `grep -A1` and would miss a date on any later line.
#[test]
fn calendar_date_is_the_first_line_after_each_header() {
    let cal = read(CALENDAR);
    let lines: Vec<&str> = cal.lines().collect();
    let mut seen = 0;
    for (i, line) in lines.iter().enumerate() {
        if *line == "[[trading.nse_holidays]]" || *line == "[[trading.muhurat_trading_dates]]" {
            seen += 1;
            let next = lines.get(i + 1).copied().unwrap_or_default();
            let ok = next.len() == "date = \"2026-01-01\"".len()
                && next.starts_with("date = \"")
                && next.ends_with('"')
                && next[8..18].bytes().enumerate().all(|(k, b)| {
                    if k == 4 || k == 7 {
                        b == b'-'
                    } else {
                        b.is_ascii_digit()
                    }
                });
            assert!(
                ok,
                "{CALENDAR}:{} `{line}` is not followed by a date line: `{next}`",
                i + 2
            );
        }
    }
    assert!(
        seen > 10,
        "{CALENDAR}: only {seen} calendar headers found; the scan is broken"
    );
}

/// A calendar Muhurat date is not a deploy day: deploy-aws's market-hours
/// guard refuses the whole date (`deploy-aws.yml` "Check current IST time
/// against market window"), so a dispatch there only leaves a red run that
/// counts as covering the SHA. The decision line must read the Muhurat
/// header beside the holiday header, and must not treat a Muhurat date as an
/// exception that dispatches.
#[test]
fn a_muhurat_date_never_dispatches() {
    let wf = read(CATCHUP);
    let decide_at = find(&wf, DEPLOY_DAY_LINE, "deploy-day decision");
    let line = wf[decide_at..].lines().next().unwrap_or_default();
    assert!(
        line.contains("-e '[[trading.muhurat_trading_dates]]'")
            && line.contains("-e '[[trading.nse_holidays]]'")
            && line.trim_end().ends_with("then deploy_day=no; fi"),
        "{CATCHUP}: a Muhurat date must set deploy_day=no like a holiday: `{line}`"
    );
    assert!(
        !line.contains("if !"),
        "{CATCHUP}: the deploy-day decision must not exempt any calendar date: `{line}`"
    );
    // The shell decides on any weekday too: a weekday Muhurat date skips.
    let weekday_muhurat =
        "[[trading.muhurat_trading_dates]]\ndate = \"2026-11-11\"\nname = \"weekday\"\n";
    assert!(!dispatches_deploy(weekday_muhurat, "2026-11-11", 3, 1550));
    assert!(dispatches_deploy(weekday_muhurat, "2026-11-12", 4, 1550));
    // The real calendar's Sunday Muhurat date.
    assert!(!dispatches_deploy(&read(CALENDAR), "2026-11-08", 7, 1550));
}

/// Rust model of `INPUTS_AND_WINDOW` + `DEPLOY_DAY_LINE` + `DEPLOY_SKIP`:
/// true when the catch-up dispatches deploy-aws.
fn dispatches_deploy(cal: &str, ist_day: &str, ist_dow: u32, ist_hhmm: u32) -> bool {
    let listed = |header: &str| {
        let lines: Vec<&str> = cal.lines().collect();
        let want = format!("date = \"{ist_day}\"");
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| **l == header)
            .any(|(i, _)| lines.get(i + 1).is_some_and(|n| *n == want))
    };
    let deploy_day = !(ist_dow >= 6
        || listed("[[trading.nse_holidays]]")
        || listed("[[trading.muhurat_trading_dates]]"));
    let in_window = (WINDOW_START_HHMM..=WINDOW_END_HHMM).contains(&ist_hhmm);
    deploy_day && in_window
}

#[test]
fn decision_model_matches_the_shell_rows() {
    let cal = read(CALENDAR);
    // Non-trading days never dispatch, even inside the window.
    assert!(!dispatches_deploy(&cal, "2026-11-07", 6, 1550)); // the incident Saturday
    assert!(!dispatches_deploy(&cal, "2026-11-07", 6, 452)); // run 42311495097's time
    assert!(!dispatches_deploy(&cal, "2026-11-15", 7, 1550)); // an ordinary Sunday
    assert!(!dispatches_deploy(&cal, "2026-11-10", 2, 1550)); // a weekday NSE holiday
    // A trading day never dispatches outside the window.
    for hhmm in [0, 5, 452, 830, 840, 900, 1200, 1544, 1606, 1630, 2359] {
        assert!(
            !dispatches_deploy(&cal, "2026-11-09", 1, hhmm),
            "Mon 2026-11-09 {hhmm:04} must not dispatch"
        );
    }
    // A trading day inside the window dispatches, at both edges.
    for hhmm in [1545, 1548, 1554, 1600, 1605] {
        assert!(
            dispatches_deploy(&cal, "2026-11-09", 1, hhmm),
            "Mon 2026-11-09 {hhmm:04} must dispatch"
        );
    }
    // No calendar file: weekends still never dispatch.
    assert!(!dispatches_deploy("", "2026-11-07", 6, 1550));
    assert!(dispatches_deploy("", "2026-11-09", 1, 1550));
    // A Muhurat date never dispatches (deploy-aws refuses the whole date).
    let muhurat = "[[trading.muhurat_trading_dates]]\ndate = \"2026-11-08\"\nname = \"Diwali\"\n";
    assert!(!dispatches_deploy(muhurat, "2026-11-08", 7, 1550));
}
