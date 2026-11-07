//! The post-merge catch-up never starts the box on a non-trading day
//! (incident Sat 2026-11-07; owner: "today is saturday why the fuck server
//! started").
//!
//! Two bot merges had no push run, so `postmerge-catchup.yml` dispatched
//! `deploy-aws.yml` (runs 42311495097 at 05:04 IST and 42331877828 at
//! 08:41 IST). A `workflow_dispatch` sets SHOULD_BE_UP=yes in deploy-aws's
//! "Ensure instance running" step, so each run self-started the stopped box,
//! deployed and stopped it again.
//!
//! The fix lives in the catch-up job: on an IST Saturday, Sunday or NSE
//! holiday (`config/base.toml` `[[trading.nse_holidays]]`) it does not
//! dispatch `deploy-aws.yml`; a calendar Muhurat date is a trading day. It
//! dispatches nothing, so no run exists for the SHA and the first firing on
//! the next trading day (00:00 IST) dispatches it, before the 09:00 IST open.
//! `deploy-aws.yml` itself is unchanged, so a human dispatch still deploys on
//! any day.
//!
//! This file pins the shell (source scan) and the calendar shape the shell
//! relies on. `trading_day_model` is a Rust model of the shell's decision;
//! the shell itself was run against `config/base.toml` by hand on
//! 2026-11-07 (Sat 07 no, Sun 08 Muhurat yes, Mon 09 yes, Tue 10 holiday no).

use std::path::PathBuf;

const CATCHUP: &str = ".github/workflows/postmerge-catchup.yml";
const DEPLOY: &str = ".github/workflows/deploy-aws.yml";
const CALENDAR: &str = "config/base.toml";

/// The exact line that decides the IST trading day.
const TRADING_DAY_LINE: &str = "if ! grep -qxF \"date = \\\"${ist_day}\\\"\" <(grep -A1 -xF '[[trading.muhurat_trading_dates]]' config/base.toml 2>/dev/null) && { [ \"${ist_dow}\" -ge 6 ] || grep -qxF \"date = \\\"${ist_day}\\\"\" <(grep -A1 -xF '[[trading.nse_holidays]]' config/base.toml 2>/dev/null); }; then trading_day=no; fi";

/// The IST date, weekday and default the line above reads.
const TRADING_DAY_INPUTS: &str =
    "ist_day=$(TZ=Asia/Kolkata date +%F); ist_dow=$(TZ=Asia/Kolkata date +%u); trading_day=yes";

/// The skip inside the dispatch loop.
const DEPLOY_SKIP: &str =
    "if [ \"${wf}\" = \"deploy-aws.yml\" ] && [ \"${trading_day}\" = \"no\" ]; then";

/// The dispatch the skip must come before.
const DISPATCH: &str = "if gh workflow run \"${wf}\" --ref main; then";

fn read(rel: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

fn find(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{CATCHUP}: {what} not found: `{needle}`"))
}

#[test]
fn catchup_skips_the_deploy_dispatch_on_a_non_trading_day() {
    let wf = read(CATCHUP);
    let inputs = find(&wf, TRADING_DAY_INPUTS, "IST date/weekday read");
    let decide = find(&wf, TRADING_DAY_LINE, "trading-day decision");
    let loop_start = find(
        &wf,
        "for wf in ci.yml bench.yml deploy-aws.yml terraform-apply.yml; do",
        "dispatch loop",
    );
    let skip = find(&wf, DEPLOY_SKIP, "deploy-aws non-trading-day skip");
    let dispatch = find(&wf, DISPATCH, "workflow dispatch");
    assert!(
        inputs < decide && decide < loop_start && loop_start < skip && skip < dispatch,
        "{CATCHUP}: the trading day must be decided before the dispatch loop, and \
         the deploy-aws skip must sit inside the loop before `gh workflow run`"
    );
    let skip_line = wf[skip..].lines().next().unwrap_or_default();
    assert!(
        skip_line.trim_end().ends_with("continue; fi"),
        "{CATCHUP}: the deploy-aws skip must `continue` (no dispatch): `{skip_line}`"
    );
}

#[test]
fn only_the_deploy_lane_is_held_on_a_non_trading_day() {
    let wf = read(CATCHUP);
    assert_eq!(
        wf.matches("\"${trading_day}\" = \"no\"").count(),
        1,
        "{CATCHUP}: exactly one lane (deploy-aws.yml) may be held on a \
         non-trading day; ci.yml, bench.yml and terraform-apply.yml start no box"
    );
    // The SHA must NOT be marked covered on a skipped day: the skip dispatches
    // nothing, so the next trading day's firing finds zero runs.
    assert!(
        !wf.contains("gh workflow run \"${wf}\" --ref main -f"),
        "{CATCHUP}: the catch-up dispatch must not pass inputs that change deploy-aws"
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
fn a_human_dispatch_of_deploy_aws_still_starts_the_box_on_any_day() {
    let wf = read(DEPLOY);
    assert!(
        wf.contains(
            "if [ \"$UP_WINDOW\" = \"yes\" ] || [ \"$EVENT_NAME\" = \"workflow_dispatch\" ]; then SHOULD_BE_UP=yes; fi"
        ),
        "{DEPLOY}: a workflow_dispatch must still set SHOULD_BE_UP=yes, so a human \
         dispatch deploys on a weekend or holiday"
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

/// Rust model of `TRADING_DAY_LINE` over the same `grep -A1` reading.
fn trading_day_model(cal: &str, ist_day: &str, ist_dow: u32) -> bool {
    let listed = |header: &str| {
        let lines: Vec<&str> = cal.lines().collect();
        let want = format!("date = \"{ist_day}\"");
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| **l == header)
            .any(|(i, _)| lines.get(i + 1).is_some_and(|n| *n == want))
    };
    if listed("[[trading.muhurat_trading_dates]]") {
        return true;
    }
    !(ist_dow >= 6 || listed("[[trading.nse_holidays]]"))
}

#[test]
fn trading_day_model_matches_the_incident_week() {
    let cal = read(CALENDAR);
    // Saturday of the incident.
    assert!(!trading_day_model(&cal, "2026-11-07", 6));
    // An ordinary Sunday.
    assert!(!trading_day_model(&cal, "2026-11-15", 7));
    // A weekday NSE holiday (Republic Day).
    assert!(!trading_day_model(&cal, "2026-01-26", 1));
    // An ordinary Monday.
    assert!(trading_day_model(&cal, "2026-11-09", 1));
    // No calendar file: weekends still skip, weekdays still dispatch.
    assert!(!trading_day_model("", "2026-11-07", 6));
    assert!(trading_day_model("", "2026-01-26", 1));
    // A Muhurat date is a trading day even on a Sunday.
    let muhurat = "[[trading.muhurat_trading_dates]]\ndate = \"2026-11-08\"\nname = \"Diwali\"\n";
    assert!(trading_day_model(muhurat, "2026-11-08", 7));
}
