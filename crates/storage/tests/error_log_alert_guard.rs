//! Ratchet for the `tv-error-logs` Grafana alert rule.
//!
//! Background: the alert defined in
//! `deploy/docker/grafana/provisioning/alerting/alerts.yml` uses a Loki
//! query `count_over_time(... [2m])` to count ERROR/FATAL log lines and
//! pages when the count > 0 sustained for `for:` duration.
//!
//! On 2026-04-26 20:28 IST a clean fresh boot at 20:27:44 (zero ERROR
//! lines from the live process — verified via empty JSONL sink and
//! empty `errors.summary.md`) nevertheless triggered the alert because
//! ERROR lines from a PRIOR (now-exited) `tickvault` process were still
//! inside Loki's 2-minute count window. With `for: 1m`, that single
//! stale-window paged the operator.
//!
//! Mechanical fix: `for:` was raised to `3m`. Because the count window
//! is `[2m]`, residual ERROR lines from a prior process age out of the
//! count within 2 minutes — strictly less than the 3-minute sustain
//! requirement. Therefore stale-from-prior-run logs cannot trigger the
//! alert. Genuine sustained errors still page.
//!
//! This guard pins the rule mechanically so a future edit cannot
//! silently revert `for:` below the count window.

use std::fs;
use std::path::{Path, PathBuf};

const ALERTS_YAML: &str = "deploy/docker/grafana/provisioning/alerting/alerts.yml";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_alerts_yaml() -> String {
    let path = workspace_root().join(ALERTS_YAML);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Extract the YAML stanza for the `tv-error-logs` rule.
///
/// We slice from `uid: tv-error-logs` to the next blank-line-then-`- uid:`
/// boundary so each assertion only inspects this rule, not adjacent ones.
fn tv_error_logs_stanza(text: &str) -> &str {
    let start = text
        .find("uid: tv-error-logs")
        .expect("tv-error-logs rule missing — Telegram early-warning signal removed");
    let tail = &text[start..];
    let end = tail
        .find("\n      - uid:")
        .or_else(|| tail.find("\n  # ---"))
        .or_else(|| tail.find("\n  - orgId:"))
        .unwrap_or(tail.len());
    &tail[..end]
}

#[test]
fn tv_error_logs_alert_rule_exists() {
    let text = load_alerts_yaml();
    assert!(
        text.contains("uid: tv-error-logs"),
        "tv-error-logs alert rule deleted — operator loses the ERROR/FATAL Telegram signal"
    );
    assert!(
        text.contains("title: \"Application ERROR Logs Detected\""),
        "tv-error-logs title changed — Telegram dedup may break"
    );
}

#[test]
fn tv_error_logs_uses_loki_json_level_match_not_substring() {
    // The spirit-correct match is `| json | level=~"ERROR|FATAL"` because
    // a raw substring regex would also match nested fields whose VALUE is
    // the literal token `"level":"ERROR"` (e.g. a serialized error payload).
    let stanza = load_alerts_yaml();
    let stanza = tv_error_logs_stanza(&stanza);
    assert!(
        stanza.contains("| json | level=~"),
        "tv-error-logs no longer uses parsed-level Loki match — fragile substring \
         regex risks false positives on log content. Restore the `| json | level=~` form."
    );
    assert!(
        stanza.contains("ERROR|FATAL"),
        "tv-error-logs no longer matches both ERROR and FATAL — coverage gap"
    );
}

#[test]
fn tv_error_logs_for_duration_exceeds_count_window() {
    // Rationale: count window is `[2m]`. `for:` MUST be greater than the
    // count window so that ERROR lines from a PRIOR (now-exited) process
    // age out of the count BEFORE the alert can fire. Otherwise a clean
    // fresh boot pages on stale residual logs.
    //
    // Locking the value to exactly `for: 3m` (what shipped on the
    // 2026-04-26 false-positive fix). If a future edit needs to change
    // it, the new value must still be strictly greater than the [Nm]
    // count window — update both this test AND the comment in the rule.
    let stanza = load_alerts_yaml();
    let stanza = tv_error_logs_stanza(&stanza);
    assert!(
        stanza.contains("count_over_time({job=\\\"tv-docker\\\", container=\\\"tickvault\\\"} | json | level=~\\\"ERROR|FATAL\\\" [2m])"),
        "tv-error-logs Loki count window changed away from `[2m]`. \
         If you raised it, also raise `for:` to remain strictly greater. \
         The invariant is `for: > count_window`."
    );
    assert!(
        stanza.contains("for: 3m"),
        "tv-error-logs `for:` MUST be `3m` (greater than the 2m count window). \
         Reverting to `for: 1m` reopens the 2026-04-26 20:28 IST false-positive \
         where stale logs from a prior process triggered a page on a clean boot. \
         If you genuinely need a different value, it must still be strictly \
         greater than the count window AND this test must be updated in the same PR."
    );
    // Belt-and-suspenders: explicitly reject the regressed values.
    assert!(
        !stanza.contains("for: 0m"),
        "tv-error-logs `for: 0m` reintroduced — every transient ERROR pages. \
         Revert to `for: 3m`."
    );
    assert!(
        !stanza.contains("for: 1m"),
        "tv-error-logs `for: 1m` reintroduced — stale-from-prior-run logs can \
         page on a clean fresh boot. See 2026-04-26 incident note in the rule \
         comment. Revert to `for: 3m`."
    );
}

#[test]
fn tv_error_logs_severity_remains_critical() {
    let stanza = load_alerts_yaml();
    let stanza = tv_error_logs_stanza(&stanza);
    assert!(
        stanza.contains("severity: critical"),
        "tv-error-logs severity downgraded — ERROR/FATAL Telegram routing depends on this label"
    );
}
