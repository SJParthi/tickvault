//! Phase 3 guard — Loki + Alloy must stay gated behind the `logs`
//! compose profile so the default stack (t4g.medium 4 GiB RAM) is
//! unaffected.
//!
//! If someone accidentally removes `profiles: [logs]` from either
//! service, both would start with the default stack and blow the
//! memory budget (+640MB). This guard catches that regression.

use std::fs;
use std::path::{Path, PathBuf};

const COMPOSE: &str = "deploy/docker/docker-compose.yml";
const LOKI_CONFIG: &str = "deploy/docker/loki/loki-config.yml";
const LOKI_RULES: &str = "deploy/docker/loki/rules.yml";
const ALLOY_CONFIG: &str = "deploy/docker/alloy/alloy-config.alloy";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_text(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn loki_and_alloy_configs_exist() {
    for rel in [LOKI_CONFIG, LOKI_RULES, ALLOY_CONFIG] {
        let path = workspace_root().join(rel);
        assert!(
            path.exists(),
            "{} missing — Phase 3 scaffold broken",
            path.display()
        );
    }
}

#[test]
fn loki_service_is_gated_behind_logs_profile() {
    let src = load_text(COMPOSE);
    let idx = src
        .find("tv-loki:")
        .unwrap_or_else(|| panic!("tv-loki service not declared in compose"));
    // The `profiles: [logs]` block must appear within ~30 lines after
    // the service declaration. If not, the service starts by default.
    let block: String = src[idx..].chars().take(1500).collect();
    assert!(
        block.contains("profiles:"),
        "tv-loki service must declare `profiles:` so it's opt-in. Default \
         stack cannot afford the +384MB RAM cost."
    );
    assert!(
        block.contains("- logs"),
        "tv-loki service must be gated behind the `logs` profile specifically"
    );
}

#[test]
fn alloy_service_is_gated_behind_logs_profile() {
    let src = load_text(COMPOSE);
    let idx = src
        .find("tv-alloy:")
        .unwrap_or_else(|| panic!("tv-alloy service not declared in compose"));
    let block: String = src[idx..].chars().take(1500).collect();
    assert!(
        block.contains("profiles:"),
        "tv-alloy service must declare `profiles:`"
    );
    assert!(
        block.contains("- logs"),
        "tv-alloy service must be gated behind the `logs` profile"
    );
}

#[test]
fn loki_retention_stays_at_30_days() {
    let src = load_text(LOKI_CONFIG);
    assert!(
        src.contains("retention_period: 720h"),
        "Loki retention must stay at 720h (30 days) — the zero-touch \
         triage chain looks back this far for novel-signature detection"
    );
}

#[test]
fn loki_memory_limit_is_set() {
    let src = load_text(COMPOSE);
    let idx = src
        .find("tv-loki:")
        .unwrap_or_else(|| panic!("tv-loki service not declared"));
    let block: String = src[idx..].chars().take(1500).collect();
    assert!(
        block.contains("mem_limit:"),
        "tv-loki must declare mem_limit so it can't OOM the host"
    );
}

#[test]
fn alloy_memory_limit_is_set() {
    let src = load_text(COMPOSE);
    let idx = src
        .find("tv-alloy:")
        .unwrap_or_else(|| panic!("tv-alloy service not declared"));
    let block: String = src[idx..].chars().take(1500).collect();
    assert!(
        block.contains("mem_limit:"),
        "tv-alloy must declare mem_limit"
    );
}

#[test]
fn alloy_mounts_data_logs_for_errors_jsonl_tail() {
    let src = load_text(COMPOSE);
    assert!(
        src.contains("../../data/logs:/var/log/tv-app")
            || src.contains("./data/logs:/var/log/tv-app")
            || src.contains("../../data/logs:/var/log/tv-app:ro"),
        "tv-alloy must bind-mount data/logs/ so it can tail \
         errors.jsonl.* — otherwise Phase 2's structured ERROR stream \
         never reaches Loki"
    );
}

#[test]
fn alloy_config_parses_errors_jsonl_with_json_stage() {
    let src = load_text(ALLOY_CONFIG);
    assert!(
        src.contains("errors_jsonl") || src.contains("errors.jsonl"),
        "alloy-config must reference errors.jsonl so the Phase 2 stream \
         is parsed, not just forwarded raw"
    );
    assert!(
        src.contains("stage.json") || src.contains("stage \"json\""),
        "alloy must have a stage.json block to extract code/severity \
         labels from errors.jsonl lines"
    );
}

#[test]
fn loki_rules_reference_all_four_required_alerts() {
    let src = load_text(LOKI_RULES);
    for alert in [
        "ErrorBurst",
        "FlushErrorStorm",
        "AuthFailureBurst",
        "NovelErrorSignature",
    ] {
        assert!(
            src.contains(&format!("alert: {alert}")),
            "Loki ruler rules must define {alert} alert"
        );
    }
}

#[test]
fn alertmanager_is_removed_from_loki_ruler() {
    // Observability → CloudWatch-only (2026-05-20, #O2): Alertmanager
    // is removed. The opt-in `logs`-profile Loki ruler must NOT
    // reference the deleted service.
    let src = load_text(LOKI_CONFIG);
    assert!(
        !src.contains("tv-alertmanager") && !src.contains("alertmanager_url:"),
        "Loki ruler must not reference Alertmanager — it was removed \
         when observability narrowed to CloudWatch-only (#O2)"
    );
}

#[test]
fn compose_preserves_default_services() {
    // Observability → CloudWatch-only program
    // (.claude/plans/active-plan-observability-cloudwatch-only.md):
    // Grafana removed in #O1, Alertmanager in #O2, Prometheus in #O3,
    // Valkey in #O4. The default profile now contains exactly 1 service:
    //   tv-questdb.
    // Loki + Alloy remain profile-gated for opt-in dev use.
    let src = load_text(COMPOSE);
    let default_profile_services: Vec<&str> = ["tv-questdb:"]
        .iter()
        .filter(|s| src.contains(*s))
        .copied()
        .collect();
    assert_eq!(
        default_profile_services.len(),
        1,
        "Expected exactly 1 default-profile service, found {:?}",
        default_profile_services
    );
    assert!(
        !src.contains("tv-alertmanager:")
            && !src.contains("tv-grafana:")
            && !src.contains("tv-prometheus:"),
        "Grafana (#O1), Alertmanager (#O2) and Prometheus (#O3) must be absent from compose"
    );
}

fn _suppress(_p: &Path) {}
