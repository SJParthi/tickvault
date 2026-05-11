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

// Wave 6 Sub-PR #1 item 1.4n — companion alert for the
// `tv_aggregator_tick_lag_total` counter (third drop class:
// broadcast::recv() returning Lagged because the subscriber fell
// behind the producer).

#[test]
fn aggregator_broadcast_lag_storm_alert_uid_is_pinned() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("uid: tv-aggregator-broadcast-lag-storm"),
        "Wave 6 Sub-PR #1 item 1.4n regression: alert \
         `tv-aggregator-broadcast-lag-storm` MUST exist in alerts.yml \
         so the operator gets a Telegram page when the aggregator \
         subscriber falls behind the fast_tick_broadcast producer \
         enough that > 100 frames are dropped at the broadcast \
         boundary in 1 minute during market hours."
    );
}

#[test]
fn aggregator_broadcast_lag_storm_alert_expression_includes_market_hours_gate() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("increase(tv_aggregator_tick_lag_total[1m]) > 100"),
        "Wave 6 Sub-PR #1 item 1.4n regression: alert expression must \
         reference `increase(tv_aggregator_tick_lag_total[1m]) > 100` \
         per audit-findings Rule 12 + Rule 4."
    );
    let uid_pos = yaml
        .find("uid: tv-aggregator-broadcast-lag-storm")
        .expect("alert uid must exist");
    let rest = &yaml[uid_pos..uid_pos.saturating_add(2000).min(yaml.len())];
    assert!(
        rest.contains("tv_market_hours_active == 1"),
        "Wave 6 Sub-PR #1 item 1.4n regression: tv-aggregator-broadcast-lag-storm \
         MUST be gated by `tv_market_hours_active == 1` per audit-findings Rule 3."
    );
}

#[test]
fn aggregator_broadcast_lag_storm_alert_severity_is_high() {
    let yaml = read_alerts_yaml();
    let uid_pos = yaml
        .find("uid: tv-aggregator-broadcast-lag-storm")
        .expect("alert uid must exist");
    let rest = &yaml[uid_pos..];
    let severity_pos = rest
        .find("severity:")
        .expect("severity label must follow the alert uid");
    let severity_slice = &rest[severity_pos..severity_pos + 32];
    assert!(
        severity_slice.contains("high"),
        "Wave 6 Sub-PR #1 item 1.4n regression: alert severity must be \
         `high` (Telegram pages operator). Found: {severity_slice:?}"
    );
}

// Wave 6 Sub-PR #1 item 1.4o — companion alert for the
// `tv_aggregator_late_ticks_discarded_total` counter (4th drop class:
// tick arrived AFTER its bucket sealed). Threshold is sustained
// 60/min for 5m (= > 300/5m) to filter routine IST midnight rollover.

#[test]
fn aggregator_late_tick_sustained_alert_uid_is_pinned() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("uid: tv-aggregator-late-tick-sustained"),
        "Wave 6 Sub-PR #1 item 1.4o regression: alert \
         `tv-aggregator-late-tick-sustained` MUST exist in alerts.yml \
         so the operator gets a Telegram page when late-tick rate \
         stays > 60/min for 5 consecutive minutes during market hours \
         (clock drift or slow consumer per AGGREGATOR-LATE-01)."
    );
}

#[test]
fn aggregator_late_tick_sustained_alert_expression_includes_market_hours_gate() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("increase(tv_aggregator_late_ticks_discarded_total[5m]) > 300"),
        "Wave 6 Sub-PR #1 item 1.4o regression: alert expression must \
         reference `increase(tv_aggregator_late_ticks_discarded_total[5m]) > 300` \
         (= sustained 60/min for 5m) per audit-findings Rule 12."
    );
    let uid_pos = yaml
        .find("uid: tv-aggregator-late-tick-sustained")
        .expect("alert uid must exist");
    let rest = &yaml[uid_pos..uid_pos.saturating_add(2000).min(yaml.len())];
    assert!(
        rest.contains("tv_market_hours_active == 1"),
        "Wave 6 Sub-PR #1 item 1.4o regression: tv-aggregator-late-tick-sustained \
         MUST be gated by `tv_market_hours_active == 1` per audit-findings Rule 3."
    );
}

#[test]
fn aggregator_late_tick_sustained_alert_for_window_is_5m() {
    let yaml = read_alerts_yaml();
    let uid_pos = yaml
        .find("uid: tv-aggregator-late-tick-sustained")
        .expect("alert uid must exist");
    let rest = &yaml[uid_pos..];
    let for_pos = rest.find("for:").expect("for: clause must exist");
    let for_slice = &rest[for_pos..for_pos + 16];
    assert!(
        for_slice.contains("5m"),
        "Wave 6 Sub-PR #1 item 1.4o regression: alert `for:` window must \
         be `5m` so transient IST midnight cascade bursts do not fire. \
         Found: {for_slice:?}"
    );
}

// Wave 6 Sub-PR #1 item 1.4p — alert-family completeness meta-test.
// Pins the relationship between the 4 drop-class alerts (1.4j seals,
// 1.4l mpsc, 1.4n broadcast, 1.4o late ticks). If any future PR
// silently deletes one without removing it from the family, this
// test fails the build — operator's drop-diagnosis coverage cannot
// silently regress.

#[test]
fn wave_6_aggregator_drop_alert_family_is_complete() {
    let yaml = read_alerts_yaml();
    let required: &[(&str, &str)] = &[
        (
            "tv-aggregator-no-seals-during-market",
            "1.4j (master switch dead)",
        ),
        ("tv-aggregator-mpsc-drop-storm", "1.4l (writer fell behind)"),
        (
            "tv-aggregator-broadcast-lag-storm",
            "1.4n (subscriber fell behind)",
        ),
        (
            "tv-aggregator-late-tick-sustained",
            "1.4o (clock drift / slow consumer)",
        ),
    ];
    let mut missing: Vec<String> = Vec::new();
    for (uid, label) in required {
        let needle = format!("uid: {uid}");
        if !yaml.contains(&needle) {
            missing.push(format!("{uid} — {label}"));
        }
    }
    assert!(
        missing.is_empty(),
        "Wave 6 Sub-PR #1 item 1.4p regression: the 4-alert drop-class \
         family must coexist in alerts.yml so the operator has Telegram \
         coverage for EVERY drop class shown on the 1.4k dashboard \
         panel. Missing:\n  {}\n\nIf you genuinely need to remove one, \
         update the required-set in this test first and document why \
         the operator no longer needs that coverage.",
        missing.join("\n  ")
    );
}

#[test]
fn wave_6_aggregator_alert_family_all_gated_by_market_hours() {
    let yaml = read_alerts_yaml();
    let uids = [
        "tv-aggregator-no-seals-during-market",
        "tv-aggregator-mpsc-drop-storm",
        "tv-aggregator-broadcast-lag-storm",
        "tv-aggregator-late-tick-sustained",
    ];
    let mut missing_gate: Vec<&str> = Vec::new();
    for uid in &uids {
        let pos = yaml
            .find(&format!("uid: {uid}"))
            .unwrap_or_else(|| panic!("alert uid {uid} must exist (1.4p requires it)"));
        let end = pos.saturating_add(2000).min(yaml.len());
        let block = &yaml[pos..end];
        if !block.contains("tv_market_hours_active == 1") {
            missing_gate.push(*uid);
        }
    }
    assert!(
        missing_gate.is_empty(),
        "Wave 6 Sub-PR #1 item 1.4p regression: every aggregator drop-class \
         alert MUST be gated by `tv_market_hours_active == 1` per \
         audit-findings Rule 3 (no off-hours pages). Missing the gate: \
         {missing_gate:?}"
    );
}

#[test]
fn wave_6_aggregator_alert_family_all_severity_high() {
    let yaml = read_alerts_yaml();
    let uids = [
        "tv-aggregator-no-seals-during-market",
        "tv-aggregator-mpsc-drop-storm",
        "tv-aggregator-broadcast-lag-storm",
        "tv-aggregator-late-tick-sustained",
    ];
    let mut wrong_severity: Vec<&str> = Vec::new();
    for uid in &uids {
        let pos = yaml
            .find(&format!("uid: {uid}"))
            .unwrap_or_else(|| panic!("alert uid {uid} must exist (1.4p requires it)"));
        let rest = &yaml[pos..];
        let sev_pos = rest
            .find("severity:")
            .unwrap_or_else(|| panic!("severity: must follow alert {uid}"));
        let sev_slice = &rest[sev_pos..sev_pos + 32];
        if !sev_slice.contains("high") {
            wrong_severity.push(*uid);
        }
    }
    assert!(
        wrong_severity.is_empty(),
        "Wave 6 Sub-PR #1 item 1.4p regression: every aggregator drop-class \
         alert MUST be severity:high so it pages Telegram. Downgrading to \
         medium / low silently buries operator pages. Wrong severity: \
         {wrong_severity:?}"
    );
}

/// Wave 6 Sub-PR #1 item 1.4r — pins that every alert in the family
/// carries the `wave: "6"` label so dashboards / Alertmanager routing
/// rules can filter the Wave 6 family without enumerating each uid.
#[test]
fn wave_6_aggregator_alert_family_all_labeled_wave_six() {
    let yaml = read_alerts_yaml();
    let uids = [
        "tv-aggregator-no-seals-during-market",
        "tv-aggregator-mpsc-drop-storm",
        "tv-aggregator-broadcast-lag-storm",
        "tv-aggregator-late-tick-sustained",
    ];
    let mut missing_label: Vec<&str> = Vec::new();
    for uid in &uids {
        let pos = yaml
            .find(&format!("uid: {uid}"))
            .unwrap_or_else(|| panic!("alert uid {uid} must exist (1.4r requires it)"));
        let end = pos.saturating_add(2000).min(yaml.len());
        let block = &yaml[pos..end];
        let has_double_quoted = block.contains("wave: \"6\"");
        let has_single_quoted = block.contains("wave: '6'");
        if !has_double_quoted && !has_single_quoted {
            missing_label.push(*uid);
        }
    }
    assert!(
        missing_label.is_empty(),
        "Wave 6 Sub-PR #1 item 1.4r regression: every aggregator drop-class \
         alert MUST carry `wave: \"6\"` label so dashboards / Alertmanager \
         routing can group the Wave 6 family without enumerating uids. \
         Missing the label: {missing_label:?}"
    );
}
