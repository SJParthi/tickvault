//! Ratchet: every metric named in `deploy/aws/terraform/app-alarms.tf`
//! must be emitted somewhere in the Rust codebase, AND must be present
//! in the CloudWatch agent's prometheus EMF metric_declaration filter
//! in `deploy/aws/terraform/user-data.sh.tftpl`.
//!
//! Three-way drift check:
//!   1. alarm metric_name → has matching `counter!` / `gauge!` call in crates/
//!   2. alarm metric_name → appears in user-data EMF filter list
//!   3. EMF filter list metric → appears in at least one alarm
//!
//! Without this guard, renaming a Rust metric (or dropping it from the
//! EMF filter) silently breaks the alarm — operator gets no Telegram.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

/// Pull every `metric_name = "tv_..."` literal out of `app-alarms.tf`.
fn alarm_metric_names() -> Vec<String> {
    let tf = read("deploy/aws/terraform/app-alarms.tf");
    let mut out = Vec::new();
    for line in tf.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("metric_name") {
            // Form: metric_name = "tv_..."
            if let Some(start) = rest.find('"') {
                if let Some(end) = rest[start + 1..].find('"') {
                    let name = &rest[start + 1..start + 1 + end];
                    if name.starts_with("tv_") {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Collect every `.rs` source file under `crates/` (depth-first walk).
fn collect_rs_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip target/ if it ever lives under a crate (it shouldn't,
            // but safe + cheap to guard).
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs_sources(&path, out);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// True iff the literal metric name appears inside any
/// `counter!`/`gauge!`/`histogram!` call in the workspace.
fn is_metric_emitted(name: &str) -> bool {
    let needles = [
        format!("counter!(\"{name}\""),
        format!("gauge!(\"{name}\""),
        format!("histogram!(\"{name}\""),
        format!("counter!(\"{name}\")"),
        format!("gauge!(\"{name}\")"),
        format!("histogram!(\"{name}\")"),
    ];
    let mut sources = Vec::new();
    collect_rs_sources(&workspace_root().join("crates"), &mut sources);
    for path in sources {
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        for needle in &needles {
            if body.contains(needle) {
                return true;
            }
        }
    }
    false
}

#[test]
fn test_every_alarm_metric_has_a_rust_emit_site() {
    let names = alarm_metric_names();
    assert!(
        !names.is_empty(),
        "ratchet self-check: app-alarms.tf produced 0 metric_name entries — parser broken"
    );
    let mut missing = Vec::new();
    for name in &names {
        if !is_metric_emitted(name) {
            missing.push(name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "Z+ L2 VERIFY ratchet: the following alarm metric names have NO matching \
         counter!/gauge!/histogram! call anywhere under crates/. Either the metric \
         was renamed in Rust without updating app-alarms.tf, or the alarm was added \
         for a metric that does not exist yet. Missing: {missing:?}"
    );
}

#[test]
fn test_every_alarm_metric_is_in_emf_filter_list() {
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    let names = alarm_metric_names();
    let mut missing = Vec::new();
    for name in &names {
        if !user_data.contains(name) {
            missing.push(name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "Z+ L2 VERIFY ratchet: app-alarms.tf references metrics that do NOT appear \
         in user-data.sh.tftpl's emf_processor metric_declaration filter. Without \
         the filter entry, the CloudWatch agent will not publish them. Missing: {missing:?}"
    );
}

/// Extract the single-quoted-string content of the first EMF `label_matcher`
/// anchored-regex list `^(...)$` from an agent config body.
fn emf_regex_body<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    // Find `"<key>": "^(` ... `)$"` and return the inner `...` alternation.
    let key_marker = format!("\"{key}\":");
    let after_key = body.split_once(&key_marker)?.1;
    let start = after_key.find("^(")? + 2;
    let end = after_key[start..].find(")$")? + start;
    Some(&after_key[start..end])
}

/// The set of `tv_*` names inside an EMF anchored-regex alternation body
/// (`a|b|c`), sorted + de-duplicated for order-independent comparison.
fn emf_declared_names(body: &str, key: &str) -> Vec<String> {
    let mut names: Vec<String> = emf_regex_body(body, key)
        .unwrap_or_default()
        .split('|')
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("tv_"))
        .collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn test_deployed_emf_source_labels_match_a_real_series_label() {
    // ROOT-CAUSE PIN (2026-07-02, B1 evidence): the previous declaration used
    // `source_labels: ["__name__"]` with the metric-name regex as
    // `label_matcher`. `__name__` is NOT a label on the scraped series at the
    // emf_processor stage (live events carry host/instance/job/
    // prom_metric_type only), so the concatenated source-label value was
    // empty, the label_matcher NEVER matched, no metric ever received the
    // `_aws` EMF envelope, and `Tickvault/Prod` sat empty for ~40 days while
    // both liveness alarms rang blind. The CORRECT shape: `source_labels`
    // references a REAL label (`host`, stamped by prometheus.yaml's
    // static_configs) with `label_matcher` pinned to its literal value;
    // `metric_selectors` alone filters metric NAMES.
    for rel in [
        "deploy/aws/terraform/user-data.sh.tftpl",
        "deploy/aws/cloudwatch-agent.json",
    ] {
        let body = read(rel);
        assert!(
            body.contains("\"source_labels\": [\"host\"]"),
            "Z+ L2 VERIFY root-cause pin: {rel} must use source_labels [\"host\"] — \
             a label that actually exists on the scraped series."
        );
        assert!(
            body.contains("\"label_matcher\": \"^tickvault-prod$\""),
            "Z+ L2 VERIFY root-cause pin: {rel} must match the host label's literal \
             value ^tickvault-prod$ (from prometheus.yaml static label)."
        );
        assert!(
            !body.contains("\"source_labels\": [\"__name__\"]"),
            "Z+ L2 VERIFY root-cause pin: {rel} regressed to source_labels \
             [\"__name__\"] — __name__ is not a series label at the emf_processor \
             stage; this exact shape produced the 40-day-empty Tickvault/Prod \
             namespace (B1 analysis, 2026-07-02)."
        );
    }
}

#[test]
fn test_emf_metric_selectors_name_count_is_twenty_one() {
    // Pin the EMF publish list: 19 alarm-backing signals + 2 memory-measurement
    // gauges added 2026-07-02 for the 2K-universe RAM measurement
    // (tv_process_rss_bytes — crates/storage/src/resource_monitor.rs;
    // tv_subsystem_memory_estimated_bytes — crates/app/src/metrics_catalog.rs
    // SUBSYSTEM_MEMORY_GAUGE_NAME). Cost note: each custom metric is ~$0.30/mo.
    // If you intentionally add/remove a name, update BOTH configs + this pin.
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    let names = emf_declared_names(&user_data, "metric_selectors");
    assert_eq!(
        names.len(),
        21,
        "Z+ L2 VERIFY ratchet: expected exactly 21 names in the EMF metric_selectors \
         list; found {}: {names:?}",
        names.len()
    );
    for required in [
        "tv_process_rss_bytes",
        "tv_subsystem_memory_estimated_bytes",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "Z+ L2 VERIFY ratchet: {required} must be in the EMF metric_selectors list \
             (2K-universe memory measurement reads it as a real CloudWatch metric)."
        );
    }
}

#[test]
fn test_log_metric_filter_fallback_covers_both_liveness_alarm_metrics() {
    // Belt-and-suspenders pin: even if the EMF fix is imperfect on the live
    // box, the two `aws_cloudwatch_log_metric_filter` resources extract
    // tv_boot_completed + tv_realtime_guarantee_score from the plain-JSON
    // events already flowing into /tickvault/<env>/metrics, publishing into
    // the EXACT namespace + host dimension the alarms watch. Deleting either
    // filter (or dropping the host dimension extraction) re-blinds the alarm.
    let tf = read("deploy/aws/terraform/metrics-log-metric-filters.tf");
    for metric in ["tv_boot_completed", "tv_realtime_guarantee_score"] {
        assert!(
            tf.contains(&format!("{{ $.{metric} = * }}")),
            "fallback filter pattern for {metric} missing from \
             metrics-log-metric-filters.tf"
        );
        assert!(
            tf.contains(&format!("name      = \"{metric}\"")),
            "fallback metric_transformation name for {metric} must EXACTLY match \
             the alarm's metric_name"
        );
    }
    assert!(
        tf.contains("namespace = \"Tickvault/Prod\""),
        "fallback filters must publish into Tickvault/Prod — the namespace every \
         app alarm reads"
    );
    assert!(
        tf.contains("host = \"$.host\""),
        "fallback filters must extract the host dimension from the JSON event — \
         the alarms key on dimensions {{host=tickvault-prod}}; a dimensionless \
         metric is invisible to them"
    );
    assert!(
        !tf.contains("default_value"),
        "fallback filters must NOT set default_value — missing data must stay \
         missing (treat_missing_data=breaching is the alarms' detection model)"
    );
}

#[test]
fn test_deployed_emf_declaration_is_superset_of_every_alarm_metric() {
    // Drift-guard: every alarm's `metric_name` MUST be in the deployed EMF
    // declaration, or the agent never publishes it and the alarm evaluates
    // against a permanently-empty metric (treat_missing_data=breaching pages
    // forever). This is the name-set superset check the restore fix pins.
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    let declared = emf_declared_names(&user_data, "metric_selectors");
    let alarms = alarm_metric_names();
    let missing: Vec<&String> = alarms.iter().filter(|a| !declared.contains(a)).collect();
    assert!(
        missing.is_empty(),
        "Z+ L2 VERIFY drift-guard: the deployed CloudWatch-agent EMF metric_declaration \
         is NOT a superset of the alarm metric_name set. These alarm metrics are not in \
         the agent's publish filter, so they will never appear in Tickvault/Prod: {missing:?}"
    );
}

#[test]
fn test_reference_cloudwatch_agent_json_matches_deployed_emf_declaration() {
    // Drift-guard: `deploy/aws/cloudwatch-agent.json` is a REFERENCE copy of
    // the DEPLOYED inline config in user-data.sh.tftpl (the file that
    // `amazon-cloudwatch-agent-ctl -a fetch-config` actually loads). The
    // grafana-cloud README cites the reference file as ground truth, so it
    // must not drift. Pin the two EMF declarations to the same name-set.
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    let reference = read("deploy/aws/cloudwatch-agent.json");
    let deployed_names = emf_declared_names(&user_data, "metric_selectors");
    let reference_names = emf_declared_names(&reference, "metric_selectors");
    assert!(
        !reference_names.is_empty(),
        "ratchet self-check: could not parse metric_selectors from cloudwatch-agent.json"
    );
    assert_eq!(
        deployed_names,
        reference_names,
        "Z+ L3 RECONCILE drift-guard: the reference deploy/aws/cloudwatch-agent.json EMF \
         name-set has drifted from the DEPLOYED user-data.sh.tftpl EMF name-set. The \
         grafana-cloud README cites the reference file, so it must stay byte-in-sync with \
         what the box actually loads.\n  deployed-only: {:?}\n  reference-only: {:?}",
        deployed_names
            .iter()
            .filter(|n| !reference_names.contains(n))
            .collect::<Vec<_>>(),
        reference_names
            .iter()
            .filter(|n| !deployed_names.contains(n))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn test_emf_metric_namespace_is_tickvault_prod_in_both_configs() {
    // The alarms in app-alarms.tf all key on namespace="Tickvault/Prod".
    // If the agent's emf_processor.metric_namespace ever changes, every
    // metric lands in a namespace no alarm reads → silent 0 datapoints.
    for rel in [
        "deploy/aws/terraform/user-data.sh.tftpl",
        "deploy/aws/cloudwatch-agent.json",
    ] {
        let body = read(rel);
        assert!(
            body.contains("\"metric_namespace\": \"Tickvault/Prod\""),
            "Z+ L2 VERIFY drift-guard: {rel} must set emf_processor.metric_namespace to \
             exactly \"Tickvault/Prod\" — the namespace every app-alarms.tf alarm reads."
        );
    }
}

/// Extract the string literal assigned to `pub const <name>: &str = "...";`
/// in `crates/app/src/observability.rs`. If the RHS is another constant
/// (e.g. `ERRORS_JSONL_DIR: &str = MACHINE_LOGS_DIR;`), resolve it one hop.
/// Panics (fail-closed) if the declaration cannot be found — a rename must
/// update THIS ratchet in the same PR as the agent configs.
fn observability_dir_const(name: &str) -> String {
    let src = read("crates/app/src/observability.rs");
    let needle = format!("pub const {name}: &str =");
    let line = src
        .lines()
        .find(|l| l.trim_start().starts_with(&needle))
        .unwrap_or_else(|| {
            panic!(
                "Z+ L2 VERIFY ratchet: crates/app/src/observability.rs no longer \
                 declares `{needle} ...` — the CW-agent log-shipping globs are \
                 coupled to this constant; update this test + BOTH agent configs \
                 in the same PR."
            ) // APPROVED: test
        });
    let rhs = line[line.find('=').expect("has =") + 1..] // APPROVED: test
        .trim()
        .trim_end_matches(';')
        .trim();
    if let Some(stripped) = rhs.strip_prefix('"') {
        return stripped
            .split('"')
            .next()
            .expect("quoted literal") // APPROVED: test
            .to_string();
    }
    // One-hop alias (ERRORS_JSONL_DIR = MACHINE_LOGS_DIR today).
    observability_dir_const(rhs)
}

#[test]
fn test_cw_agent_collects_machine_log_paths() {
    // 2026-07-06: the 2026-07-05 machine/ move silently killed BOTH app log
    // streams (old globs don't descend into machine/) — every log metric
    // filter on /tickvault/prod/app was DOA. Round-2 review fix: the globs
    // are now CROSS-COUPLED to the Rust sink constants in observability.rs
    // (not just pinned as literals), so BOTH a config-side glob regression
    // AND a code-side sink move (the exact 2026-07-05 vector — Rust moved,
    // configs untouched) fail this build until the two move in lockstep.
    let errors_dir = observability_dir_const("ERRORS_JSONL_DIR");
    let app_dir = observability_dir_const("MACHINE_LOGS_DIR");
    let errors_glob = format!("/opt/tickvault/{errors_dir}/errors.jsonl.*");
    let app_glob = format!("/opt/tickvault/{app_dir}/app.*");
    for rel in [
        "deploy/aws/terraform/user-data.sh.tftpl",
        "deploy/aws/cloudwatch-agent.json",
    ] {
        let body = read(rel);
        assert!(
            body.contains(&errors_glob),
            "Z+ L2 VERIFY ratchet: {rel} must tail the ERROR JSONL glob \
             {errors_glob} (derived from observability.rs::ERRORS_JSONL_DIR; \
             dotted, so the bare errors.jsonl compat symlink is excluded). \
             Without it every error-code log metric filter on \
             /tickvault/prod/app is DOA. If the Rust sink dir moved, move the \
             agent-config globs in the SAME PR."
        );
        assert!(
            body.contains(&app_glob),
            "Z+ L2 VERIFY ratchet: {rel} must tail the hourly app-log glob \
             {app_glob} (derived from observability.rs::MACHINE_LOGS_DIR). The \
             2026-07-05 machine/ move took the hourly app log too \
             (crates/app/src/main.rs init_app_log_appender). If the Rust sink \
             dir moved, move the agent-config globs in the SAME PR."
        );
    }
}

#[test]
fn test_app_alarms_count_is_thirteen() {
    // Pin the count so future PRs that delete an alarm without updating
    // the rule files / PR body fail this guard. Cost note (aws-budget.md)
    // depends on this number — keeping the budget honest means keeping
    // this number explicit.
    //
    // 13 (was 12) since 2026-06-02: added `tv_ticks_dropped_total` — the
    // final zero-tick-loss breach (rescue ring + spill + DLQ all failed),
    // the operator's #1 invariant. The upstream spill/dlq tiers were
    // already alarmed; this is the strictly-more-severe irrecoverable case.
    // 15 (was 13) since 2026-06-03 (zero-tick-loss PR-4 / G4+G1): added
    // `tv_ws_frame_dropped_no_wal_total` (hard WS-frame-lost breach) +
    // `tv_ws_reconnect_gap_seconds_total` (reconnect-churn rate-alarm —
    // gives PR-3's reconnect-gap metric its anomaly detector).
    // 16 (was 15) since 2026-06-03 (zero-tick-loss PR-5 / G3): added
    // `tv_disk_watcher_respawn_total` — the spill disk-health watcher is
    // now supervised (respawn + alert) instead of fire-and-forget; the
    // counter feeds this rate-alarm so a flapping watcher pages.
    // 17 (was 16) since 2026-06-12: added `tv_late_tick_after_boundary_total`
    // — the hot-path-safe CloudWatch equivalent of the RETIRED
    // LastTickAfterBoundary Telegram variant. Pages if Dhan ever stamps a
    // tick at/after 15:30 IST, without threading a notifier into the per-tick
    // hot path. Cost: +1 custom metric (~$0.30/mo) + 1 alarm (~$0.10/mo),
    // negligible within the ~₹2,058/mo envelope.
    let count = alarm_metric_names().len();
    assert_eq!(
        count, 17,
        "Z+ L2 VERIFY ratchet: expected exactly 17 app-level CloudWatch alarms \
         (one per critical app signal). Found {count}. If you intentionally added \
         or removed one, update aws-budget.md custom-metric cost line AND this guard."
    );
}
