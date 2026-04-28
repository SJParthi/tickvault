//! Ratchet for Grafana 13's hard 40-character limit on alert-rule UIDs.
//!
//! Background: on 2026-04-28 the `tv-grafana` container entered a
//! crash-restart loop because `alerts.yml` contained one rule whose UID
//! was 48 characters long
//! (`tv-hot-path-02-prev-close-writer-sustained-drops`). Grafana 13
//! refuses to start the provisioning module with:
//!
//!   "cannot create rule with UID '...': UID is longer than 40 symbols"
//!
//! The provisioning failure cascades through every dependent module
//! (HTTP server, dashboards, secrets, …) and the container never reaches
//! the `/api/health` endpoint — operator dashboards go dark.
//!
//! This test scans the on-disk `alerts.yml` and fails the build if any
//! `uid:` field is longer than 40 characters, so a future edit cannot
//! re-introduce the same crash-loop.

use std::fs;
use std::path::{Path, PathBuf};

const ALERTS_YAML: &str = "deploy/docker/grafana/provisioning/alerting/alerts.yml";
const GRAFANA_UID_MAX_LEN: usize = 40;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn collect_uids(yaml: &str) -> Vec<(usize, String)> {
    let mut uids = Vec::new();
    for (idx, line) in yaml.lines().enumerate() {
        let trimmed = line.trim_start();
        let rest = match trimmed
            .strip_prefix("- uid:")
            .or_else(|| trimmed.strip_prefix("uid:"))
        {
            Some(r) => r,
            None => continue,
        };
        let uid = rest
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string();
        if !uid.is_empty() {
            uids.push((idx + 1, uid));
        }
    }
    uids
}

#[test]
fn every_grafana_alert_uid_is_within_40_chars() {
    let path = workspace_root().join(ALERTS_YAML);
    let yaml = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let offenders: Vec<_> = collect_uids(&yaml)
        .into_iter()
        .filter(|(_, uid)| uid.len() > GRAFANA_UID_MAX_LEN)
        .collect();

    assert!(
        offenders.is_empty(),
        "Grafana 13 rejects alert-rule UIDs longer than {GRAFANA_UID_MAX_LEN} chars \
         (causes provisioning module to fail → container crash-loop). \
         Offenders in {ALERTS_YAML}: {offenders:?}"
    );
}

#[test]
fn at_least_one_uid_is_parsed_so_the_guard_is_not_vacuous() {
    let path = workspace_root().join(ALERTS_YAML);
    let yaml = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let uids = collect_uids(&yaml);
    assert!(
        !uids.is_empty(),
        "guard parsed zero UIDs from {ALERTS_YAML} — parser regressed or file moved"
    );
}
