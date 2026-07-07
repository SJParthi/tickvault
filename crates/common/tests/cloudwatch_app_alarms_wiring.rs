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

/// Strip `//`-line-comments from a source body BEFORE needle matching.
///
/// 2026-07-06 anti-vacuity fix (mutation-proven hole): the whitespace
/// compaction below made COMMENT text needle-matchable too — a doc comment
/// in THIS very file mentioning a metric's emit macro self-satisfied the
/// guard, so renaming the only real emit site left the guard green (the
/// exact false-OK class, audit-findings Rule 11). Comments can never be an
/// emit site, so they are removed before matching. `://` (URL scheme
/// separators inside string literals) is treated as code, not a comment
/// start — the `http_client_fallback_guard.rs` precedent.
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// True iff the literal metric name appears inside any
/// `counter!`/`gauge!`/`histogram!` call in the workspace.
///
/// 2026-07-06 fix: each source body has its `//`-line-comments stripped
/// (see [`strip_line_comments`]) and is then whitespace-STRIPPED before
/// matching, so a rustfmt-wrapped multi-line `metrics::counter!` invocation
/// naming e.g. `tv_feed_sidecar_stall_restart_total` normalizes to one
/// contiguous needle and is guard-visible, while a mere COMMENT mention of
/// the same macro-plus-name can never satisfy the guard (pinned by
/// `test_emit_site_guard_ignores_comment_only_mentions`). Before the
/// compaction fix the contiguous needles matched ONLY single-line emits —
/// a multi-line emit made a real metric look missing (false-negative on
/// the emit site, false-positive "missing" panic here).
fn is_metric_emitted(name: &str) -> bool {
    // No needle contains whitespace, so matching against the compacted
    // body is exact. `counter!("name")` is covered by the `counter!("name`
    // prefix, so three needles suffice.
    let needles = [
        format!("counter!(\"{name}\""),
        format!("gauge!(\"{name}\""),
        format!("histogram!(\"{name}\""),
    ];
    let mut sources = Vec::new();
    collect_rs_sources(&workspace_root().join("crates"), &mut sources);
    for path in sources {
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        let code_only = strip_line_comments(&body);
        let compact: String = code_only.chars().filter(|c| !c.is_whitespace()).collect();
        for needle in &needles {
            if compact.contains(needle) {
                return true;
            }
        }
    }
    false
}

#[test]
fn test_emit_site_guard_ignores_comment_only_mentions() {
    // Anti-vacuity self-test (2026-07-06 mutation finding): a metric name
    // that appears ONLY inside a comment must NOT count as an emit site.
    // The sentinel below exists in the workspace exclusively inside the
    // next comment line: counter!("tv_guard_vacuity_sentinel_comment_only_total"
    assert!(
        !is_metric_emitted("tv_guard_vacuity_sentinel_comment_only_total"),
        "is_metric_emitted matched a name that appears ONLY in a comment — \
         the emit-site guard is vacuous again (comment stripping regressed)."
    );
    // Positive control: the real FEED-STALL-01 multi-line emit in
    // crates/app/src/groww_sidecar_supervisor.rs must still be found —
    // proves comment stripping did not break REAL emit detection.
    assert!(
        is_metric_emitted("tv_feed_sidecar_stall_restart_total"),
        "comment stripping broke detection of a REAL multi-line emit site \
         (groww_sidecar_supervisor.rs FEED-STALL-01 counter)."
    );
}

#[test]
fn test_strip_line_comments_keeps_code_and_urls_drops_comments() {
    let src = "let a = 1; // trailing comment counter!(\"tv_fake_total\"\n\
               /// doc comment counter!(\"tv_fake_total\"\n\
               let url = \"https://example.com\";\n";
    let stripped = strip_line_comments(src);
    assert!(
        !stripped.contains("tv_fake_total"),
        "comment text survived stripping: {stripped}"
    );
    assert!(
        stripped.contains("let a = 1;") && stripped.contains("https://example.com"),
        "code or URL text was wrongly removed: {stripped}"
    );
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
    //
    // 24 (was 21) since 2026-07-06 (Groww feed-down alerting, operator
    // directive): added `tv_groww_ws_active` (connected-level 0/1 gauge),
    // `tv_feed_last_tick_age_seconds{feed}` (feed liveness age gauge — both
    // emitted from crates/app/src/groww_bridge.rs), and
    // `tv_feed_sidecar_stall_restart_total` (FEED-STALL-01 stall-kill
    // counter — crates/app/src/groww_sidecar_supervisor.rs). Cost: +3
    // custom metrics ≈ +$0.90/mo per the app-alarms.tf header cost note.
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    let names = emf_declared_names(&user_data, "metric_selectors");
    assert_eq!(
        names.len(),
        24,
        "Z+ L2 VERIFY ratchet: expected exactly 24 names in the EMF metric_selectors \
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
    // 19 (was 17) since 2026-07-06 (Groww feed-down alerting, operator
    // directive): added `tv_groww_ws_active` (alarm
    // tv-<env>-groww-ws-inactive — Groww WS lost after being up this
    // session) + `tv_feed_sidecar_stall_restart_total` (alarm
    // tv-<env>-groww-stall-restart-storm — 3+ FEED-STALL-01 silent-feed
    // kills within an hour = provider-side reject). Cost: +2 alarms
    // (~$0.20/mo) + 3 custom metrics (~$0.90/mo incl. the un-alarmed
    // tv_feed_last_tick_age_seconds), per the app-alarms.tf header note.
    let count = alarm_metric_names().len();
    assert_eq!(
        count, 19,
        "Z+ L2 VERIFY ratchet: expected exactly 19 app-level CloudWatch alarms \
         (one per critical app signal). Found {count}. If you intentionally added \
         or removed one, update aws-budget.md custom-metric cost line AND this guard."
    );
}
