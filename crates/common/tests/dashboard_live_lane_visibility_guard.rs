//! Pins that every metric we PAY to publish is actually LOOKED AT.
//!
//! # The defect this guard exists to prevent recurring
//!
//! Between 2026-08-11 and 2026-08-15 the Dhan live lane — the only live tick
//! source in the system — had **twenty-one** metric series added to the
//! CloudWatch agent's EMF allowlist across three separate dated cost notes,
//! each one deliberate, each one justified, each one billed at ~$0.30/month.
//!
//! Not one of them was charted anywhere, and none of them has an alarm.
//!
//! So the money bought the series and nothing bought the *sight* of it. Every
//! way the live feed can lose ticks was counted, shipped to CloudWatch, and
//! then read by nobody — which is indistinguishable, from the operator's
//! chair, from not having instrumented it at all. The dashboard's own Row-7
//! comment had already described exactly this failure one subsystem earlier
//! ("the operator could look at an all-green page while the whole runtime was
//! dead"); it recurred anyway, because nothing mechanical connected the act of
//! publishing a metric to the act of displaying it.
//!
//! # Why a guard rather than a habit
//!
//! The two halves live in different files, are edited in different PRs, and
//! neither one fails when the other is missing. An allowlist entry with no
//! widget is invisible; a widget with no allowlist entry renders "no data",
//! which an operator reads as "fine" — strictly worse than an absent panel.
//! Only a check that reads BOTH files can see the mismatch.
//!
//! # What is pinned
//!
//! Every EMF-selected `tv_dhan_*` name is either charted on the operator
//! dashboard, or listed in [`DELIBERATELY_NOT_CHARTED`] with a written reason.
//! Adding a lane metric without doing one of those two things fails the build.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("dashboard visibility guard: cannot canonicalize repo root")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|e| panic!("dashboard visibility guard: {rel} must be readable: {e}"))
}

const USER_DATA: &str = "deploy/aws/terraform/user-data.sh.tftpl";
const DASHBOARD: &str = "deploy/aws/terraform/dashboard.tf";

/// Lane metrics that are EMF-published but intentionally not on the dashboard.
///
/// Each entry must carry a reason. "We forgot" is not one of them — that is
/// the case this guard exists to catch, and it looks identical to a deliberate
/// omission until somebody writes down which it is.
const DELIBERATELY_NOT_CHARTED: &[(&str, &str)] = &[
    (
        "tv_dhan_feed_depth_total",
        "SHIPPED but neither charted nor alarmed, and that is #1751's own \
         recorded decision rather than an oversight: paging on it is a new \
         Dhan-scoped alert, which dhan-rest-only-noise-lock-2026-07-14.md §3 \
         REJECTs without a dated row in THAT file first. Charting it here \
         would be the better half of the fix and is a one-widget follow-up; \
         listing it keeps the gap WRITTEN DOWN in the meantime, which is the \
         entire point of this list. It carries an `outcome` label whose \
         `refused` and `dropped` values answer 'did a depth level that \
         arrived fail to reach the table' — the two worth a chart first.",
    ),
    (
        "tv_dhan_ws_lag_excluded_total",
        "counts lag SAMPLES excluded from the p99 (stale/negative exchange \
     timestamps), not ticks lost. It is a quality qualifier for a histogram \
     that is deliberately NOT EMF-shipped, so charting it alone would show a \
     correction with nothing to correct. It belongs beside the lag histogram \
     on the /metrics endpoint, where the histogram actually lives.",
    ),
];

/// Extract the EMF `metric_selectors` names from the deployed agent config.
fn emf_selected_names(user_data: &str) -> Vec<String> {
    // The UNION across EVERY `metric_selectors` entry, not just the first.
    // The agent config carries more than one declaration block (the host-only
    // list plus per-dimension ones), and reading only the first silently
    // under-reports what is published — which in this guard would produce
    // false "charted but never publishes" failures on names that publish fine.
    let mut names: Vec<String> = Vec::new();
    let mut rest = user_data;
    while let Some(idx) = rest.find("metric_selectors") {
        rest = &rest[idx + "metric_selectors".len()..];
        let Some(open) = rest.find("[\"") else { break };
        let after = &rest[open + 2..];
        let Some(close) = after.find('"') else { break };
        // Each selector is an anchored regex: `^(a|b|c)$` or `^single$`.
        for token in after[..close].split('|') {
            let token = token
                .trim()
                .trim_start_matches("^(")
                .trim_start_matches('^')
                .trim_end_matches(")$")
                .trim_end_matches('$')
                .trim();
            if token.starts_with("tv_") {
                names.push(token.to_string());
            }
        }
        rest = &after[close..];
    }
    assert!(
        !names.is_empty(),
        "no tv_* names parsed out of {USER_DATA} — the extractor is broken, \
         and every check in this file would pass or fail for the wrong reason"
    );
    names.sort();
    names.dedup();
    names
}

#[test]
fn every_published_live_lane_metric_is_visible_somewhere() {
    let selected = emf_selected_names(&read(USER_DATA));
    let dashboard = read(DASHBOARD);

    // ALARMS COUNT AS VISIBLE, and until 2026-08-15 this guard did not look at
    // them — while its own failure message said "and are in no alarm".
    //
    // That is a false claim inside a guard whose entire purpose is catching
    // false claims, and it is not harmless: it sends the reader hunting for a
    // missing alarm that already exists. It surfaced on the merge with #1751,
    // which added `tv_dhan_live_universe_fallback_total` and
    // `tv_dhan_ws_alive_connections` WITH alarms and no chart — correctly
    // visible, wrongly reported.
    //
    // The test is named `..._is_visible_somewhere`. An alarm IS somewhere: a
    // metric that pages an operator is not money spent on a number nobody
    // sees. Charted-or-alarmed is the property; charted-only was a bug.
    let alarms: String = std::fs::read_dir(repo_root().join("deploy/aws/terraform"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "tf"))
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    assert!(
        alarms.contains("aws_cloudwatch_metric_alarm"),
        "the terraform scan found no alarm definitions at all — the alarm half \
         of this check would pass vacuously and re-create the exact blind spot \
         it was added to close"
    );

    let lane_metrics: Vec<&String> = selected
        .iter()
        .filter(|n| n.starts_with("tv_dhan_"))
        .collect();

    let mut invisible: Vec<String> = Vec::new();
    for name in &lane_metrics {
        // A `"name"` occurrence in a `metrics = [...]` position is what puts a
        // line on a chart. A bare mention inside a `#` comment is not, so the
        // quoted form is the right thing to look for: comments in this file
        // never quote the metric.
        let charted = dashboard.contains(&format!("\"{name}\""));
        // `metric_name = "..."` is the alarm form; a bare comment mention is
        // not, so the quoted form is again what distinguishes them.
        // Whitespace-INSENSITIVE since 2026-08-21. This used to match two
        // hardcoded spacings — one space, or exactly nine. HCL aligns `=` to
        // the longest key in its own block, so the correct spacing depends on
        // what other attributes that particular alarm happens to declare. A
        // new alarm whose longest key was `comparison_operator` aligned to
        // EIGHT spaces, matched neither pattern, and the metric was reported
        // as unalarmed while its alarm sat in the file being scanned.
        //
        // That is the failure mode this guard exists to prevent, arriving
        // through the guard itself: it would have sent someone to add a
        // duplicate alarm for a metric that already had one.
        let alarmed = alarms.lines().any(|line| {
            let squeezed: String = {
                let mut out = String::with_capacity(line.len());
                let mut prev_space = false;
                for ch in line.trim().chars() {
                    let is_space = ch == ' ' || ch == '\t';
                    if !(is_space && prev_space) {
                        out.push(if is_space { ' ' } else { ch });
                    }
                    prev_space = is_space;
                }
                out
            };
            squeezed == format!("metric_name = \"{name}\"")
        });
        let excused = DELIBERATELY_NOT_CHARTED.iter().any(|(m, _)| m == *name);

        if !charted && !alarmed && !excused {
            invisible.push((*name).clone());
        }
    }

    assert!(
        invisible.is_empty(),
        "these live-lane metrics are PUBLISHED to CloudWatch (~$0.30/mo each) \
         but appear on no dashboard and are in no alarm — money spent on \
         numbers nobody can see:\n  {}\n\nEither add a widget in {DASHBOARD}, \
         or add the name to DELIBERATELY_NOT_CHARTED in this file with a \
         written reason. An unexplained gap here is exactly the state that \
         left the entire failure surface of the only live tick source \
         invisible for four days.",
        invisible.join("\n  ")
    );
}

#[test]
fn nothing_is_charted_that_can_never_publish() {
    // The inverse error, and the more dangerous one. A widget whose metric is
    // not EMF-selected renders as "no data" — visually identical to a healthy
    // flat-zero loss counter. An operator glancing at it concludes nothing is
    // being lost, when in fact nothing is being measured.
    let selected = emf_selected_names(&read(USER_DATA));
    let dashboard = read(DASHBOARD);

    let mut phantom: Vec<String> = Vec::new();
    for name in dashboard.split('"').filter(|s| {
        s.starts_with("tv_dhan_") || s.starts_with("tv_tick") || s.starts_with("tv_ws_")
    }) {
        // Derived metrics-log-filter series (order-side) are published by a
        // log metric filter rather than the EMF path, so they legitimately
        // never appear in the selector list.
        if name.ends_with("_delta_total") {
            continue;
        }
        if !selected.iter().any(|s| s == name) {
            phantom.push(name.to_string());
        }
    }
    phantom.sort();
    phantom.dedup();

    assert!(
        phantom.is_empty(),
        "these metrics are CHARTED on the operator dashboard but are not in \
         the CloudWatch agent's EMF allowlist, so they can never carry a \
         datapoint:\n  {}\n\nThe widget will render 'no data', which on a loss \
         counter is indistinguishable from 'nothing is being lost'. Either add \
         the name to metric_selectors in {USER_DATA} (with a dated cost note, \
         ~$0.30/mo) or remove the widget.",
        phantom.join("\n  ")
    );
}

#[test]
fn the_lane_row_actually_exists_and_is_substantial() {
    // Non-vacuity. Both checks above pass trivially against a dashboard file
    // that charts nothing at all, which is precisely the state this guard was
    // written to end.
    let dashboard = read(DASHBOARD);
    let selected = emf_selected_names(&read(USER_DATA));

    let lane_count = selected
        .iter()
        .filter(|n| n.starts_with("tv_dhan_"))
        .count();
    assert!(
        lane_count >= 15,
        "found only {lane_count} tv_dhan_* names in the EMF allowlist — the \
         scanner is not matching, so both checks above would pass vacuously"
    );

    let charted = selected
        .iter()
        .filter(|n| n.starts_with("tv_dhan_"))
        .filter(|n| dashboard.contains(&format!("\"{n}\"")))
        .count();
    assert!(
        charted >= 15,
        "only {charted} of {lane_count} live-lane metrics are charted. The \
         live feed is the only tick source in the system; a dashboard that \
         does not show it is not an operator dashboard"
    );
}

#[test]
fn every_excuse_carries_a_real_reason() {
    // An allowlist whose entries can be blank is a rubber stamp: the next
    // person adds a name, leaves the reason empty, and the guard passes while
    // the metric goes dark. Require the reason to be long enough that it had
    // to be written by someone who thought about it.
    for (metric, reason) in DELIBERATELY_NOT_CHARTED {
        assert!(
            reason.len() > 80,
            "{metric} is excused from the dashboard with a {}-char reason. \
             Write down WHY it is safe for this metric to be invisible, or \
             chart it",
            reason.len()
        );
        assert!(metric.starts_with("tv_"), "{metric} is not a metric name");
    }
}
