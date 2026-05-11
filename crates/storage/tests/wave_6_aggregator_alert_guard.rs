//! Wave 6 Sub-PR #1 item 1.4j — Prometheus alert guard.
//!
//! Pins the `tv-aggregator-no-seals-during-market` alert rule in
//! `deploy/docker/grafana/provisioning/alerting/alerts.yml`. The
//! rule pages the operator (Severity::High) if the multi-TF
//! aggregator (master switch, item 1.4d merged in #578) emits zero
//! sealed candles for 5 consecutive minutes during 09:15-15:30 IST.
//!
//! Mirrors the pattern in `wave_2d_alert_guard.rs` +
//! `resilience_sla_alert_guard.rs`. Future deletion of the rule
//! fails the build.

use std::fs;
use std::path::{Path, PathBuf};

const GRAFANA_ALERTS_YAML: &str = "deploy/docker/grafana/provisioning/alerting/alerts.yml";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_alerts_yaml() -> String {
    let path = workspace_root().join(GRAFANA_ALERTS_YAML);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

#[test]
fn aggregator_no_seals_alert_uid_is_pinned() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("uid: tv-aggregator-no-seals-during-market"),
        "Wave 6 Sub-PR #1 item 1.4j regression: alert \
         `tv-aggregator-no-seals-during-market` MUST exist in alerts.yml. \
         Without it the operator gets ZERO Telegram when the master \
         switch silently dies (global_seal_sender None, broadcast Closed, \
         or full mpsc on every try_send)."
    );
}

#[test]
fn aggregator_no_seals_alert_expression_includes_market_hours_gate() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("increase(tv_aggregator_seals_emitted_total[5m]) == 0"),
        "Wave 6 Sub-PR #1 item 1.4j regression: alert expression must \
         reference `increase(tv_aggregator_seals_emitted_total[5m]) == 0` \
         per audit-findings Rule 12 — raw counter would lie after restart."
    );
    assert!(
        yaml.contains("tv_market_hours_active == 1"),
        "Wave 6 Sub-PR #1 item 1.4j regression: alert MUST be gated by \
         `tv_market_hours_active == 1` per audit-findings Rule 3 — \
         pre-market / post-market silence is expected and must not page."
    );
}

#[test]
fn aggregator_no_seals_alert_severity_is_high() {
    let yaml = read_alerts_yaml();
    // The alert block is followed by `severity: high` somewhere in the
    // surrounding section. Mirror the wave_2d guard pattern: scan that
    // the rule uid coexists with severity:high in the same group.
    let uid_pos = yaml
        .find("uid: tv-aggregator-no-seals-during-market")
        .expect("alert uid must exist");
    let rest = &yaml[uid_pos..];
    let severity_pos = rest
        .find("severity:")
        .expect("severity label must follow the alert uid");
    let severity_slice = &rest[severity_pos..severity_pos + 32];
    assert!(
        severity_slice.contains("high"),
        "Wave 6 Sub-PR #1 item 1.4j regression: alert severity must be \
         `high` (Telegram pages operator). Found: {severity_slice:?}"
    );
}

#[test]
fn aggregator_no_seals_alert_for_window_is_5m() {
    let yaml = read_alerts_yaml();
    let uid_pos = yaml
        .find("uid: tv-aggregator-no-seals-during-market")
        .expect("alert uid must exist");
    let rest = &yaml[uid_pos..];
    let for_pos = rest.find("for:").expect("for: clause must exist");
    let for_slice = &rest[for_pos..for_pos + 16];
    assert!(
        for_slice.contains("5m"),
        "Wave 6 Sub-PR #1 item 1.4j regression: alert `for:` window must \
         be `5m` so transient gaps (depth rebalance, IST midnight \
         rollover, etc.) do not fire. Found: {for_slice:?}"
    );
}

// Wave 6 Sub-PR #1 item 1.4l — companion alert for the
// `tv_seal_mpsc_dropped_total` counter (one of the 3 drop series
// in the "Aggregator drops & lag (5m)" panel from 1.4k).

#[test]
fn aggregator_mpsc_drop_storm_alert_uid_is_pinned() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("uid: tv-aggregator-mpsc-drop-storm"),
        "Wave 6 Sub-PR #1 item 1.4l regression: alert \
         `tv-aggregator-mpsc-drop-storm` MUST exist in alerts.yml so \
         the operator gets a Telegram page when the seal-writer task \
         falls behind enough that > 100 seals are dropped at the \
         producer side in 1 minute during market hours."
    );
}

#[test]
fn aggregator_mpsc_drop_storm_alert_expression_includes_market_hours_gate() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("increase(tv_seal_mpsc_dropped_total[1m]) > 100"),
        "Wave 6 Sub-PR #1 item 1.4l regression: alert expression must \
         reference `increase(tv_seal_mpsc_dropped_total[1m]) > 100` \
         per audit-findings Rule 12 + Rule 4 (rising-edge fire only)."
    );
    // The market-hours gate `tv_market_hours_active == 1` is already
    // pinned for the 1.4j alert above; this assertion only confirms
    // the new alert ALSO carries the gate (audit-findings Rule 3).
    let uid_pos = yaml
        .find("uid: tv-aggregator-mpsc-drop-storm")
        .expect("alert uid must exist");
    let rest = &yaml[uid_pos..uid_pos.saturating_add(2000).min(yaml.len())];
    assert!(
        rest.contains("tv_market_hours_active == 1"),
        "Wave 6 Sub-PR #1 item 1.4l regression: tv-aggregator-mpsc-drop-storm \
         MUST be gated by `tv_market_hours_active == 1` so pre-market / \
         post-market silence does not page (audit-findings Rule 3)."
    );
}

#[test]
fn aggregator_mpsc_drop_storm_alert_severity_is_high() {
    let yaml = read_alerts_yaml();
    let uid_pos = yaml
        .find("uid: tv-aggregator-mpsc-drop-storm")
        .expect("alert uid must exist");
    let rest = &yaml[uid_pos..];
    let severity_pos = rest
        .find("severity:")
        .expect("severity label must follow the alert uid");
    let severity_slice = &rest[severity_pos..severity_pos + 32];
    assert!(
        severity_slice.contains("high"),
        "Wave 6 Sub-PR #1 item 1.4l regression: alert severity must be \
         `high` (Telegram pages operator). Found: {severity_slice:?}"
    );
}
