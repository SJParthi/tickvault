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

/// Pull every `metric_name = "tv_..."` literal out of the app-level alarm
/// terraform files. 2026-07-06 (silent-feed incident hardening): scope
/// EXTENDED from app-alarms.tf alone to ALSO cover silent-feed-alarms.tf —
/// the 3 new alarms (SLO degraded dead-band, per-feed BOUNDARY-01 catch-up
/// storm, Dhan exchange-lag p99) live there for PR-conflict isolation and
/// must pass the same emit-site + EMF-filter drift checks.
fn alarm_metric_names() -> Vec<String> {
    let mut tf = read("deploy/aws/terraform/app-alarms.tf");
    tf.push('\n');
    tf.push_str(&read("deploy/aws/terraform/silent-feed-alarms.tf"));
    let mut out = Vec::new();
    for line in tf.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("metric_name") {
            // Form: metric_name = "tv_..."
            if let Some(start) = rest.find('"')
                && let Some(end) = rest[start + 1..].find('"')
            {
                let name = &rest[start + 1..start + 1 + end];
                if name.starts_with("tv_") {
                    out.push(name.to_string());
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
/// NOTE: this parses only the FIRST `metric_selectors` occurrence — i.e. the
/// MAIN host-only declaration. Use `emf_all_declared_names` for the union
/// across ALL declarations (the 2026-07-06 per-feed second declaration has
/// a single-name `^tv_boundary_catchup_total$` selector with no `(...)`).
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

/// The union of `tv_*` names across EVERY `metric_selectors` entry in an
/// agent config body — handles BOTH the anchored alternation form
/// (`^(a|b|c)$`, the main host-only declaration) and the single-name form
/// (`^tv_boundary_catchup_total$`, the 2026-07-06 per-feed declaration).
fn emf_all_declared_names(body: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = body;
    while let Some(idx) = rest.find("\"metric_selectors\":") {
        rest = &rest[idx + "\"metric_selectors\":".len()..];
        let Some(open) = rest.find("[\"") else { break };
        let after = &rest[open + 2..];
        let Some(close) = after.find('"') else { break };
        let regex = &after[..close];
        // Strip the anchors: `^(...)$` (alternation) or `^...$` (single name).
        let inner = regex
            .trim_start_matches("^(")
            .trim_start_matches('^')
            .trim_end_matches(")$")
            .trim_end_matches('$');
        for n in inner.split('|') {
            let n = n.trim();
            if n.starts_with("tv_") {
                names.push(n.to_string());
            }
        }
        rest = &after[close..];
    }
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
fn test_emf_metric_selectors_name_count_is_twenty_six() {
    // Pin the MAIN (host-only) EMF publish list: 19 alarm-backing signals
    // + 2 memory-measurement gauges added 2026-07-02 for the 2K-universe RAM
    // measurement (tv_process_rss_bytes — crates/storage/src/resource_monitor.rs;
    // tv_subsystem_memory_estimated_bytes — crates/app/src/metrics_catalog.rs
    // SUBSYSTEM_MEMORY_GAUGE_NAME)
    // + 2 silent-feed lag signals added 2026-07-06 (incident hardening):
    // tv_dhan_exchange_lag_p99_seconds (feed_lag_monitor gauge, alarmed in
    // silent-feed-alarms.tf) + tv_dhan_lag_samples_excluded_total (the
    // WAL-replay exclusion visibility counter — Rule 11: exclusions must be
    // visible, never silent). tv_boundary_catchup_total is NOT in this list —
    // it publishes ONLY via the SECOND [host,feed] declaration (host-only
    // folding would mask a Dhan storm under the Groww baseline).
    // Cost note: each custom metric series is ~$0.30/mo.
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
        26,
        "Z+ L2 VERIFY ratchet: expected exactly 26 names in the MAIN EMF \
         metric_selectors list (24 post-#1437 groww feed-down alerting + 2 \
         silent-feed lag names 2026-07-06); found {}: {names:?}",
        names.len()
    );
    for required in [
        "tv_process_rss_bytes",
        "tv_subsystem_memory_estimated_bytes",
        "tv_dhan_exchange_lag_p99_seconds",
        "tv_dhan_lag_samples_excluded_total",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "Z+ L2 VERIFY ratchet: {required} must be in the MAIN EMF metric_selectors \
             list (2K-universe memory measurement + 2026-07-06 silent-feed lag signals \
             read them as real CloudWatch metrics)."
        );
    }
}

/// Split an agent-config body into its individual `metric_declaration`
/// array objects (brace-balanced scan from the array opener).
///
/// Round-2 hardening (2026-07-07, finding 2): the per-feed ratchet
/// previously used three INDEPENDENT whole-file substring checks — a
/// multi-declaration reshuffle that kept a `[host,feed]` declaration for a
/// DIFFERENT selector while moving `^tv_boundary_catchup_total$` into a new
/// host-only declaration would have passed every check while the
/// boundary_catchup_storm_dhan alarm (dimensions { host, feed = "dhan" })
/// evaluated against a permanently-empty series. This parser lets the test
/// bind selector and dimensions WITHIN one declaration object.
fn emf_declaration_objects(body: &str) -> Vec<String> {
    let marker = "\"metric_declaration\": [";
    let mut out = Vec::new();
    let Some(idx) = body.find(marker) else {
        return out;
    };
    let rest = &body[idx + marker.len()..];
    let mut depth = 0usize;
    let mut start = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(s) = start.take()
                {
                    out.push(rest[s..=i].to_string());
                }
            }
            ']' if depth == 0 => break, // end of the metric_declaration array
            _ => {}
        }
    }
    out
}

#[test]
fn test_second_emf_declaration_publishes_boundary_catchup_per_feed() {
    // 2026-07-06 (silent-feed incident hardening): tv_boundary_catchup_total
    // MUST be published under dimensions [host, feed] via a SECOND
    // metric_declaration in BOTH agent configs. Per-feed is Rule-11-mandatory:
    // Groww's 60s catch-up lateness margin makes catch-up sealing its ROUTINE
    // steady-state path for quiet SIDs — a host-only folded series would
    // either mask a Dhan storm under the Groww baseline or page on healthy
    // Groww behaviour. The boundary_catchup_storm_dhan alarm
    // (silent-feed-alarms.tf) keys on { host, feed = "dhan" }.
    for rel in [
        "deploy/aws/terraform/user-data.sh.tftpl",
        "deploy/aws/cloudwatch-agent.json",
    ] {
        let body = read(rel);
        assert!(
            body.contains("\"dimensions\": [[\"host\", \"feed\"]]"),
            "Z+ L2 VERIFY ratchet: {rel} must carry the SECOND metric_declaration with \
             dimensions [[\"host\", \"feed\"]] — the per-feed BOUNDARY-01 export."
        );
        assert!(
            body.contains("\"metric_selectors\": [\"^tv_boundary_catchup_total$\"]"),
            "Z+ L2 VERIFY ratchet: {rel} must select ^tv_boundary_catchup_total$ in the \
             per-feed declaration — without it the boundary-catchup-storm-dhan alarm \
             evaluates against a permanently-empty metric."
        );
        // The per-feed declaration must NOT fold the metric host-only too:
        // publishing BOTH a host-only and a [host,feed] series would double
        // the cost and re-introduce the masked/folded series.
        let main_names = emf_declared_names(&body, "metric_selectors");
        assert!(
            !main_names.iter().any(|n| n == "tv_boundary_catchup_total"),
            "Z+ L2 VERIFY ratchet: {rel} must NOT list tv_boundary_catchup_total in the \
             MAIN host-only declaration — it publishes ONLY under [host, feed]."
        );
        // Round-2 hardening (2026-07-07, finding 2): bind selector ↔
        // dimensions WITHIN a single declaration object. The three checks
        // above are independent whole-file substrings — a reshuffle that
        // kept a [host,feed] declaration for a DIFFERENT selector while
        // moving the boundary-catchup selector into a host-only declaration
        // (outside the main alternation) would pass them all while the
        // per-feed alarm watched a permanently-empty series.
        let decls = emf_declaration_objects(&body);
        assert!(
            decls.len() >= 2,
            "parser self-check: expected >= 2 metric_declaration objects in {rel}, found {} — \
             emf_declaration_objects broken or the array collapsed",
            decls.len()
        );
        let catchup: Vec<&String> = decls
            .iter()
            .filter(|d| d.contains("tv_boundary_catchup_total"))
            .collect();
        assert_eq!(
            catchup.len(),
            1,
            "Z+ L2 VERIFY ratchet: {rel} must have EXACTLY ONE metric_declaration selecting \
             tv_boundary_catchup_total (found {}) — a second declaration (e.g. host-only) \
             would double-publish or fold the per-feed series",
            catchup.len()
        );
        assert!(
            catchup[0].contains("\"dimensions\": [[\"host\", \"feed\"]]")
                && catchup[0].contains("\"metric_selectors\": [\"^tv_boundary_catchup_total$\"]"),
            "Z+ L2 VERIFY ratchet: {rel} — the declaration selecting tv_boundary_catchup_total \
             must ITSELF carry dimensions [[\"host\", \"feed\"]] + the anchored single-name \
             selector (the boundary_catchup_storm_dhan alarm keys on {{ host, feed = dhan }}):\n{}",
            catchup[0]
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
    // 2026-07-06: union across ALL metric_declaration entries — the per-feed
    // [host,feed] second declaration carries tv_boundary_catchup_total, which
    // backs the boundary-catchup-storm-dhan alarm.
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    let declared = emf_all_declared_names(&user_data);
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
    // 2026-07-06: the SECOND (per-feed) declaration must not drift either —
    // compare the union across ALL declarations in both files.
    let deployed_all = emf_all_declared_names(&user_data);
    let reference_all = emf_all_declared_names(&reference);
    assert_eq!(
        deployed_all, reference_all,
        "Z+ L3 RECONCILE drift-guard: the UNION of EMF-declared names (all \
         metric_declaration entries, incl. the 2026-07-06 per-feed \
         tv_boundary_catchup_total declaration) has drifted between the deployed \
         user-data.sh.tftpl and the reference cloudwatch-agent.json."
    );
    assert!(
        deployed_all
            .iter()
            .any(|n| n == "tv_boundary_catchup_total"),
        "Z+ L3 RECONCILE drift-guard: tv_boundary_catchup_total must be declared \
         (per-feed second declaration) — emf_all_declared_names parser broken or \
         the declaration was deleted."
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
    // `.2*` (date-stamped rotations only) matches the form main landed in
    // PR #1438 — it excludes the bare errors.jsonl compat symlink AND the
    // 0-byte machine/app.log placeholder, tailing only real rotated files.
    let errors_glob = format!("/opt/tickvault/{errors_dir}/errors.jsonl.2*");
    let app_glob = format!("/opt/tickvault/{app_dir}/app.2*");
    for rel in [
        "deploy/aws/terraform/user-data.sh.tftpl",
        "deploy/aws/cloudwatch-agent.json",
    ] {
        let body = read(rel);
        assert!(
            body.contains(&errors_glob),
            "Z+ L2 VERIFY ratchet: {rel} must tail the ERROR JSONL glob \
             {errors_glob} (derived from observability.rs::ERRORS_JSONL_DIR; \
             dotted + date-stamped, so the bare errors.jsonl compat symlink \
             is excluded). \
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
    // `.2*` (date-stamped rotations only) matches the form main landed in
    // PR #1438 — it excludes the bare errors.jsonl compat symlink AND the
    // 0-byte machine/app.log placeholder, tailing only real rotated files.
    let errors_glob = format!("/opt/tickvault/{errors_dir}/errors.jsonl.2*");
    let app_glob = format!("/opt/tickvault/{app_dir}/app.2*");
    for rel in [
        "deploy/aws/terraform/user-data.sh.tftpl",
        "deploy/aws/cloudwatch-agent.json",
    ] {
        let body = read(rel);
        assert!(
            body.contains(&errors_glob),
            "Z+ L2 VERIFY ratchet: {rel} must tail the ERROR JSONL glob \
             {errors_glob} (derived from observability.rs::ERRORS_JSONL_DIR; \
             dotted + date-stamped, so the bare errors.jsonl compat symlink \
             is excluded). \
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
fn test_tick_gap_silent_alarm_threshold_is_forty() {
    // 2026-07-06 incident pin: 29-67 of 776 instruments were silent EVERY
    // minute while the old threshold=100 never crossed — zero pages all day.
    // Round-3 correction 2026-07-08 (review finding 4): the first retune
    // shipped 25, BELOW the documented ~33 always-silent healthy floor
    // (main.rs D2 note 2026-07-03 — the gauge is set from the same scan
    // with no always-silent exclusion), so 25 would have breached every
    // healthy in-session minute and paged daily. 40 (fires at >= 41,
    // PROVISIONAL, one-trading-week soak mandated) clears the floor with
    // margin and aligns with the SLO-degraded alarm's >= 39-silent
    // freshness breach point; the 29-40 marginal band is owned by the
    // SLO-degraded + lag-p99 alarms. 10-of-12 M-of-N at 60s/Maximum pages
    // on the sustained upper band while a 1-3 min reconnect blip cannot
    // reach 10 breaching minutes. Regressing ANY of these values either
    // reproduces the zero-page miss (raise) or the daily-false-page
    // inversion (lower below the floor).
    let tf = read("deploy/aws/terraform/app-alarms.tf");
    let block = alarm_resource_block(&tf, "tick_gap_instruments_silent");
    assert!(
        block_has_attr(&block, "threshold", "40"),
        "tick_gap_instruments_silent threshold must be 40 (2026-07-08 round-3 \
         retune — above the ~33 always-silent healthy floor):\n{block}"
    );
    assert!(
        block_has_attr(&block, "comparison_operator", "\"GreaterThanThreshold\""),
        "tick_gap_instruments_silent must use GreaterThanThreshold (fires at >= 41)"
    );
    assert!(
        block_has_attr(&block, "evaluation_periods", "12")
            && block_has_attr(&block, "datapoints_to_alarm", "10"),
        "tick_gap_instruments_silent must latch M-of-N 10-of-12 (not strict-consecutive; \
         a threshold-adjacent value flapping 39/41/39 must neither page nor let one clean \
         scrape erase 9 min of evidence — the incident band was 29-67 silent/min)"
    );
    assert!(
        block_has_attr(&block, "period", "60")
            && block_has_attr(&block, "statistic", "\"Maximum\""),
        "tick_gap_instruments_silent must evaluate period=60/Maximum (1 datapoint per 60s scrape)"
    );
    assert!(
        block_has_attr(&block, "treat_missing_data", "\"notBreaching\""),
        "tick_gap_instruments_silent must be notBreaching (nightly box stop)"
    );
}

#[test]
fn test_tick_gap_silent_alarm_is_window_gated() {
    // Rule 3 (market-hours gate, MANDATORY): the silent-instruments gauge is
    // written only in-session, so the LAST in-session value keeps being
    // re-scraped 15:30->16:30 IST — a stale post-close value must never page.
    // actions_enabled=false + the window-gate Lambda ALARM_NAMES entry is the
    // enforcement pair; losing EITHER half re-opens the off-hours false-page.
    let tf = read("deploy/aws/terraform/app-alarms.tf");
    let block = alarm_resource_block(&tf, "tick_gap_instruments_silent");
    assert!(
        block_has_attr(&block, "actions_enabled", "false"),
        "tick_gap_instruments_silent must ship actions_enabled=false (gate Lambda owns the window)"
    );
    let gate = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");
    assert!(
        gate.contains("aws_cloudwatch_metric_alarm.tick_gap_instruments_silent.alarm_name"),
        "tick_gap_instruments_silent must be in the window-gate Lambda ALARM_NAMES join \
         (market-hours-liveness-alarm.tf) — without it the alarm stays actions-disabled forever"
    );
}

#[test]
fn test_tick_gap_silent_gauge_producer_pins_pre_open_to_zero() {
    // Round-4 fix pin (2026-07-08, final-review findings 1/2/4): the
    // tick-gap gauge producer in main.rs MUST pin the gauge to 0.0 during
    // the NSE pre-open/auction window [09:00, 09:15) IST — the mirror of
    // the round-3 SLO tick_freshness pre-open pin. Without it, the
    // 09:08-09:15 matching/buffer freeze writes ~6-7 guaranteed breaching
    // datapoints (boot-seeded ~775 SIDs silent, far above threshold 40)
    // into the retuned alarm's 10-of-12 / 12-min lookback — the gate
    // Lambda's forced-OK at 09:20 does NOT purge datapoints, so ~3
    // open-ramp minutes > 40 would false-page at ~09:21 on ordinary days.
    // This scan matches CODE only (comments stripped) so a comment mention
    // can never satisfy it, and it asserts the UNPINNED raw write form is
    // absent so the pin cannot be silently bypassed.
    let body = fs::read_to_string(workspace_root().join("crates/app/src/main.rs"))
        .expect("read crates/app/src/main.rs"); // APPROVED: test
    let code_only = strip_line_comments(&body);
    let compact: String = code_only.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("constTICK_GAP_GAUGE_SESSION_OPEN_SECS_OF_DAY_IST:u32=9*3600+15*60"),
        "main.rs must define TICK_GAP_GAUGE_SESSION_OPEN_SECS_OF_DAY_IST = 09:15:00 IST \
         (the continuous-session open — the pre-open pin boundary)"
    );
    assert!(
        compact.contains("now_ist_secs_of_day()<TICK_GAP_GAUGE_SESSION_OPEN_SECS_OF_DAY_IST"),
        "main.rs must gate the tick-gap gauge value on now_ist_secs_of_day() < \
         TICK_GAP_GAUGE_SESSION_OPEN_SECS_OF_DAY_IST (pre-open pin to 0.0)"
    );
    assert!(
        compact.contains("gauge!(\"tv_tick_gap_instruments_silent\").set(gauge_silent)"),
        "the tv_tick_gap_instruments_silent gauge write must use the pre-open-pinned \
         gauge_silent value"
    );
    assert!(
        !compact.contains("gauge!(\"tv_tick_gap_instruments_silent\").set(total_silent"),
        "the tv_tick_gap_instruments_silent gauge must NOT be written from the raw \
         total_silent count — that bypasses the [09:00, 09:15) IST pre-open pin"
    );
}

#[test]
fn test_realtime_guarantee_degraded_alarm_threshold_matches_slo_warn() {
    // The degraded alarm's 0.95 threshold MUST equal SLO_WARN_THRESHOLD in
    // crates/core/src/instrument/slo_score.rs — score == 0.95 is Healthy in
    // Rust, and LessThanThreshold keeps it correctly non-breaching. If the
    // Rust constant ever moves, this pin forces the alarm to move with it.
    let slo = read("crates/core/src/instrument/slo_score.rs");
    assert!(
        slo.contains("pub const SLO_WARN_THRESHOLD: f64 = 0.95;"),
        "SLO_WARN_THRESHOLD moved from 0.95 — update the realtime_guarantee_degraded \
         alarm threshold in silent-feed-alarms.tf IN THE SAME PR, then update this pin"
    );
    let tf = read("deploy/aws/terraform/silent-feed-alarms.tf");
    let block = alarm_resource_block(&tf, "realtime_guarantee_degraded");
    assert!(
        block_has_attr(&block, "threshold", "0.95"),
        "realtime_guarantee_degraded threshold must equal SLO_WARN_THRESHOLD (0.95)"
    );
    assert!(
        block_has_attr(&block, "comparison_operator", "\"LessThanThreshold\""),
        "realtime_guarantee_degraded must use LessThanThreshold (score == 0.95 is Healthy)"
    );
    assert!(
        block_has_attr(&block, "evaluation_periods", "15")
            && block_has_attr(&block, "datapoints_to_alarm", "9"),
        "realtime_guarantee_degraded must latch 9-of-15 (round-2 correction 2026-07-07: with \
         universe 776, tick_freshness breaches < 0.95 only at >= 39 silent, so the incident's \
         29-38-silent minutes SAMPLE Healthy on the once-per-60s point scrape — 12-of-15 could \
         fail to latch on the very incident it closes; strict 15/15 would never latch on the \
         125-crossing oscillation. 9-of-15 is the honest latch; a 2-3 min reconnect dip still \
         cannot reach 9 breaching points)"
    );
    // Medium tier: the name/description must never carry a "critical" token —
    // the < 0.80 sibling alarm owns that word.
    for needle in ["alarm_name", "alarm_description"] {
        let line = block
            .lines()
            .find(|l| l.trim_start().starts_with(needle))
            .unwrap_or_else(|| panic!("{needle} missing from realtime_guarantee_degraded")); // APPROVED: test
        assert!(
            !line.to_lowercase().contains("critical"),
            "realtime_guarantee_degraded {needle} must NOT contain a 'critical' token \
             (Medium tier; the < 0.80 sibling owns Critical): {line}"
        );
    }
}

#[test]
fn test_silent_feed_alarms_are_window_gated() {
    // All 3 silent-feed alarms follow the house market-hours-gate pattern
    // (Rule 3): actions_enabled=false + appended to the window-gate Lambda
    // ALARM_NAMES (09:20-15:35 IST Mon-Fri). The SLO publisher runs 24/7
    // with off-hours dimension dips; the lag gauge + tick-gap gauge go stale
    // after close — every one of them false-pages without the gate.
    let tf = read("deploy/aws/terraform/silent-feed-alarms.tf");
    let gate = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");
    for name in [
        "realtime_guarantee_degraded",
        "boundary_catchup_storm_dhan",
        "dhan_exchange_lag_p99_high",
    ] {
        let block = alarm_resource_block(&tf, name);
        assert!(
            block_has_attr(&block, "actions_enabled", "false"),
            "{name} must ship actions_enabled=false (gate Lambda owns the window)"
        );
        assert!(
            gate.contains(&format!("aws_cloudwatch_metric_alarm.{name}.alarm_name")),
            "{name} must be in the window-gate Lambda ALARM_NAMES join \
             (market-hours-liveness-alarm.tf)"
        );
    }
}

#[test]
fn test_boundary_catchup_alarm_uses_per_feed_dimensions() {
    // Rule-11 pin: the catch-up storm alarm must key on the EXPLICIT
    // { host, feed = "dhan" } dimensions map — NOT local.app_dimensions
    // (host-only). Groww's 60s catch-up margin makes catch-up sealing its
    // ROUTINE steady state; a host-only series either masks a Dhan storm
    // under the Groww baseline or pages on healthy Groww behaviour.
    let tf = read("deploy/aws/terraform/silent-feed-alarms.tf");
    let block = alarm_resource_block(&tf, "boundary_catchup_storm_dhan");
    assert!(
        block_has_attr(&block, "host", "\"tickvault-prod\"")
            && block_has_attr(&block, "feed", "\"dhan\""),
        "boundary_catchup_storm_dhan must carry the explicit {{ host, feed = dhan }} \
         dimensions map:\n{block}"
    );
    assert!(
        !block.contains("local.app_dimensions"),
        "boundary_catchup_storm_dhan must NOT use local.app_dimensions (host-only folding \
         masks a Dhan storm under the Groww catch-up baseline)"
    );
    assert!(
        block_has_attr(&block, "statistic", "\"Sum\"") && block_has_attr(&block, "period", "300"),
        "boundary_catchup_storm_dhan must evaluate Sum/300s — the CW agent ships counters \
         as per-scrape DELTAS, so Sum over 5m IS increase(5m) (Rule 12)"
    );
}

#[test]
fn test_app_alarms_count_is_twenty_two() {
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
    // directive, #1437): added `tv_groww_ws_active` (alarm
    // tv-<env>-groww-ws-inactive — Groww WS lost after being up this
    // session) + `tv_feed_sidecar_stall_restart_total` (alarm
    // tv-<env>-groww-stall-restart-storm — 3+ FEED-STALL-01 silent-feed
    // kills within an hour = provider-side reject). Cost: +2 alarms
    // (~$0.20/mo) + 3 custom metrics (~$0.90/mo incl. the un-alarmed
    // tv_feed_last_tick_age_seconds), per the app-alarms.tf header note.
    // 22 (was 19) since 2026-07-06 (silent-feed incident hardening — the Dhan
    // feed degraded all day with 4 independent signals and zero pages): scope
    // now ALSO covers silent-feed-alarms.tf, which adds
    // `tv_realtime_guarantee_score` (degraded 0.80-0.95 dead-band, 9-of-15),
    // `tv_boundary_catchup_total` (per-feed dhan catch-up storm, PROVISIONAL
    // 2000/5m x2) and `tv_dhan_exchange_lag_p99_seconds` (exchange->receive
    // lag p99 > 10s x10). Note: the score name appears TWICE in the count
    // (critical + degraded alarms watch the same metric). Cost: +4 custom
    // metric series (~$1.20/mo) + 3 alarms (~$0.30/mo) — dated note in
    // aws-budget.md.
    let count = alarm_metric_names().len();
    assert_eq!(
        count, 22,
        "Z+ L2 VERIFY ratchet: expected exactly 22 app-level CloudWatch alarm \
         metric_name entries across app-alarms.tf + silent-feed-alarms.tf \
         (one per critical app signal; tv_realtime_guarantee_score counts twice — \
         critical + degraded). Found {count}. If you intentionally added \
         or removed one, update aws-budget.md custom-metric cost line AND this guard."
    );
}
