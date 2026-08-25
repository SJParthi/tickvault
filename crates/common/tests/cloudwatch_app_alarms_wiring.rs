//! Ratchet: every metric named in `deploy/aws/terraform/app-alarms.tf`
//! must be emitted somewhere in the Rust codebase, AND must be present
//! in the CloudWatch agent's prometheus EMF metric_declaration filter.
//!
//! Three-way drift check:
//!   1. alarm metric_name → has matching `counter!` / `gauge!` call in crates/
//!   2. alarm metric_name → appears in the EMF filter list
//!   3. EMF filter list metric → appears in at least one alarm
//!
//! Without this guard, renaming a Rust metric (or dropping it from the
//! EMF filter) silently breaks the alarm — operator gets no Telegram.
//!
//! # 2026-08-25: where "deployed" now lives
//!
//! Until 2026-08-25 the agent config was embedded in
//! `deploy/aws/terraform/user-data.sh.tftpl` AND duplicated in
//! `deploy/aws/cloudwatch-agent.json`, with a separate lockstep guard keeping
//! the two byte-identical. That duplicate was ~1.6 KB and it pinned the
//! user-data template at EXACTLY its 15,872-byte budget with zero bytes free,
//! which blocked every further boot-script change.
//!
//! The template now writes a minimal host-only fallback and copies the repo
//! file into place after the Step 5 clone, so `deploy/aws/cloudwatch-agent.json`
//! IS the deployed config — there is one copy and it cannot drift from
//! itself. Every check below therefore reads that file. That the template
//! still installs it (and no longer embeds a selector of its own) is pinned
//! separately by `cw_agent_selector_lockstep_guard.rs`.

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

/// The CloudWatch agent config the box actually loads.
///
/// Copied into `/opt/aws/amazon-cloudwatch-agent/etc/` by user-data Step
/// 5b-ii, after the repo clone. Before 2026-08-25 this content was embedded in
/// the user-data template and this constant pointed there; see the module
/// header for why it moved.
const DEPLOYED_CW_AGENT_CONFIG: &str = "deploy/aws/cloudwatch-agent.json";

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
    let entries = fs::read_dir(dir).unwrap_or_else(|e| {
        // 2026-08-10: was a silent `else { return; }` — an unreadable or
        // MISSING directory became "nothing to check, pass", so the guard
        // could report green while scanning zero files.
        panic!("guard corpus unreadable {:?}: {}", dir, e)
    });
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
    // Positive control (retuned 2026-07-15 — the FEED-STALL-01 emit died
    // with the Groww live feed): the Trap-A heartbeat emit in
    // crates/app/src/spot_1m_rest_boot.rs (+ groww_spot_1m_boot.rs) must be
    // found — proves comment stripping did not break REAL emit detection,
    // and pins the re-pointed liveness alarm's emit site.
    assert!(
        is_metric_emitted("tv_rest_1m_fire_heartbeat"),
        "comment stripping broke detection of a REAL emit site \
         (spot_1m_rest_boot.rs tv_rest_1m_fire_heartbeat gauge)."
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
    // DEAD-MONITOR allowlist — EMPTIED 2026-07-18 (stage-4 dead-producer
    // sweep): the 4 dead-tick alarm resources (spill-dropped, dlq-ticks,
    // ticks-dropped, late-tick-after-boundary) were deleted from
    // app-alarms.tf in the same PR, so the stage-2 allowlist entries were
    // removed per the lockstep contract below. The scaffold stays so any
    // future deliberate dead-monitor window re-uses it.
    let dead_monitor_pending_tf_retirement: &[&str] = &[];
    let mut missing = Vec::new();
    for name in &names {
        if dead_monitor_pending_tf_retirement.contains(&name.as_str()) {
            continue;
        }
        if !is_metric_emitted(name) {
            missing.push(name.clone());
        }
    }
    // Anti-drift: every allowlist entry must still BE an alarm metric in
    // app-alarms.tf AND still have no emit site — a stale entry (alarm
    // deleted by the dashboard PR, or an emit site reborn) fails loudly.
    for stale in dead_monitor_pending_tf_retirement {
        assert!(
            names.iter().any(|n| n == stale),
            "dead-monitor allowlist entry `{stale}` is no longer an alarm metric in \
             app-alarms.tf — remove it from this allowlist (dashboard-PR lockstep)."
        );
        assert!(
            !is_metric_emitted(stale),
            "dead-monitor allowlist entry `{stale}` has a live emit site again — \
             remove it from this allowlist."
        );
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
    let user_data = read(DEPLOYED_CW_AGENT_CONFIG);
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
    for rel in [DEPLOYED_CW_AGENT_CONFIG, "deploy/aws/cloudwatch-agent.json"] {
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
fn test_emf_metric_selectors_name_count_is_pinned() {
    // Pin the MAIN (host-only) EMF publish list: 19 alarm-backing signals
    // + 2 memory-measurement gauges added 2026-07-02 for the 2K-universe RAM
    // measurement (tv_process_rss_bytes — crates/storage/src/resource_monitor.rs;
    // tv_subsystem_memory_estimated_bytes — crates/app/src/metrics_catalog.rs
    // SUBSYSTEM_MEMORY_GAUGE_NAME)
    // + 2 silent-feed lag signals added 2026-07-06 (incident hardening):
    // tv_dhan_exchange_lag_p99_seconds (feed_lag_monitor gauge, alarmed in
    // silent-feed-alarms.tf) + tv_dhan_lag_samples_excluded_total (the
    // WAL-replay exclusion visibility counter — Rule 11: exclusions must be
    // visible, never silent)
    // + 1 Groww lag signal added 2026-07-11 (scoreboard PR-C):
    // tv_groww_exchange_lag_p99_seconds (the Groww feed_lag_monitor gauge —
    // its OWN name, never a feed label on the Dhan gauge; alarmed in
    // silent-feed-alarms.tf S4). The Groww exclusion/clamp counters stay
    // /metrics-only (₹0). tv_boundary_catchup_total is NOT in this list —
    // it published ONLY via the SECOND [host,feed] declaration until that
    // declaration retired 2026-07-17 with the tick aggregator (stage-3
    // dead-WS sweep).
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
    // 22 (was 27) since 2026-07-13 (PR-C2 — Dhan live-WS lane deletion):
    // RETIRED the 5 names whose emitters died with the lane —
    // tv_websocket_pool_all_dead, tv_websocket_failed_connections_count,
    // tv_realtime_guarantee_score (SLO publisher PARKED per the wave-3-d
    // banner), tv_ws_frame_dropped_no_wal_total and
    // tv_ws_reconnect_gap_seconds_total. Cost: -5 selected series
    // (~-$1.50/mo) vs the pre-C2 bill.
    // 21 (was 22 on this branch / 26 on main) since 2026-07-14 (Dhan noise
    // lock fix round, M4, reconciled through the PR-C2 merge): REMOVED
    // `tv_order_update_ws_active` — the order-update WS spawn is retired
    // (scope-lock §A.1), so the gauge has zero reachable writers; shipping
    // a dead name in the EMF list would publish nothing while implying
    // coverage. Cost: -1 custom metric (~-$0.30/mo).
    //
    // 23 (was 21 on this branch / 28 on main) since 2026-07-14 (cluster-C
    // order-side, reconciled through the PR-C2 merge): +tv_daily_pnl
    // +tv_order_fill_lag_seconds — both DORMANT in dry-run (emit sites ship
    // with cluster A / Phase-1); $0 until data.
    // 22 (was 23) since 2026-07-14 (PR-C3 — tick-gap detector deletion,
    // operator Q4-ii 2026-07-13): REMOVED `tv_tick_gap_instruments_silent`
    // — its gauge producer (the per-SID tick-gap detector) was deleted with
    // the Dhan WS lane, so the name would never be published again. Cost:
    // -1 custom metric series (~-$0.30/mo) — dated note in app-alarms.tf.
    // 19 (was 22) since 2026-07-15 (Groww live-feed retirement): REMOVED the
    // 4 Groww-live names whose producers died with the bridge / sidecar
    // stall watchdog / lag publisher — tv_groww_ws_active,
    // tv_feed_last_tick_age_seconds, tv_feed_sidecar_stall_restart_total,
    // tv_groww_exchange_lag_p99_seconds — and ADDED tv_rest_1m_fire_heartbeat
    // (the re-pointed market-hours liveness alarm's per-fire gauge). Net
    // -3 series; dated note in aws-budget.md (COST NOTE 2026-07-15).
    // 17 (was 19) since 2026-07-17 (stage-3 dead-WS sweep — the 21-TF tick
    // aggregator deletion): REMOVED tv_aggregator_seals_emitted_total (its
    // emit site, the seal_routing fan-in, was deleted with the aggregator)
    // + tv_aggregator_close_pct_nonzero_total (its emit site, the deleted
    // per-tick seal write boundary). Dated notes in app-alarms.tf header +
    // aws-budget.md (COST NOTE 2026-07-17).
    // 15 (was 17) since 2026-07-17 (dashboard tidy): REMOVED the 2 Dhan lag
    // names — tv_dhan_exchange_lag_p99_seconds +
    // tv_dhan_lag_samples_excluded_total — their only emit sites (the Dhan
    // lag ring/publisher half of feed_lag_monitor.rs) were deleted with the
    // dead Dhan-lag chain (spawn sites + tick source died PR-C2 2026-07-13 /
    // stage-2 sweep 2026-07-17). Cost: -2 custom metric series (~-$0.60/mo)
    // — dated note in aws-budget.md (COST NOTE 2026-07-17).
    // 11 (was 15) since 2026-07-18 (stage-4 dead-producer sweep): REMOVED
    // the 4 dead-tick names — tv_spill_dropped_total, tv_dlq_ticks_total,
    // tv_ticks_dropped_total, tv_late_tick_after_boundary_total — their
    // emit sites (tick_persistence.rs ring/spill/DLQ + the tick_processor.rs
    // post-close check) were deleted in the stage-2 sweep (2026-07-17), so
    // the names could never publish a datapoint again. Cost: -4 custom
    // metric series (~-$1.20/mo, Assumed — series-hours decay to $0 once
    // producers stop) — dated note in aws-budget.md (COST NOTE 2026-07-18).
    // 41 (was 11) since 2026-08-09 (METRIC-BLINDNESS fix): the selector had
    // shrunk to 11 names through the retirement sweeps above while the
    // workspace emits ~352 distinct metric names — so ~95% of everything the
    // binary measured was scraped locally and DISCARDED, and with Grafana +
    // Prometheus retired (CloudWatch-only migration #O1/#O3, 2026-05-19)
    // CloudWatch is the ONLY sink. +30 EXACT names ≈ +$9.00/mo (11 → 41
    // names ≈ $3.30 → $12.30/mo at CloudWatch's ~$0.30/custom-metric/month).
    //
    // WHY EXACT NAMES AND NOT A `tv_.*` PREFIX: the live budget kill-ceiling
    // is $100/mo and its AUTOMATIC budget actions fire STOP_EC2_INSTANCES at
    // 90% ($90) — a prefix selector publishes whatever the binary happens to
    // emit and could stop the trading box mid-session. An exact alternation
    // keeps the bill a function of THIS list, which this assertion pins.
    //
    // 48 (was 41) since 2026-08-11 (DHAN LIVE LANE SWITCHED ON): the live
    // Dhan WebSocket lane went from dark to carrying data, and its SEVEN loss
    // counters were emitted by the binary while selected by nothing — every
    // way the lane can lose data (WAL write refused, frame ring full by
    // count, frame ring full by BYTES, frame refused, subscribe failed, seal
    // dropped, sequence refused) was counted in-process and discarded at the
    // agent. A lane whose drop paths are invisible reports healthy while
    // losing ticks, which is the precise false-OK class rule 11 forbids.
    // +7 EXACT names ≈ +$2.10/mo (41 → 48 names ≈ $12.30 → $14.40/mo at
    // CloudWatch's ~$0.30/custom-metric/month) against the $100 ceiling whose
    // budget actions STOP the box at $90 — headroom is unaffected at this
    // scale. Dated note: aws-budget.md (COST NOTE 2026-08-11).
    //
    // FLAGGED, deliberately NOT taken here: the exclusion ledger below still
    // excludes tv_ws_frame_spill_write_errors_total on the stated ground that
    // "no WS frame producer exists since the 2026-07-13/15 live-feed
    // retirements". That premise is FALSE as of today — the revived lane IS a
    // WS frame producer, so the name now has a reachable emitter and its
    // exclusion rests on a reason that no longer holds. It is recorded rather
    // than silently added: adding it is a cost decision of its own and would
    // make this commit's delta something other than the seven names it claims.
    //
    // INCLUSION RULE: a name is selected only if it means FAILURE,
    // SATURATION or DATA LOSS *and* has a reachable producer on the REST-only
    // runtime. 311 names are deliberately not selected — success/volume
    // counters and latency histograms (they answer "how much", not "what
    // broke"), names emitted only by the stood-down per-minute boot legs
    // ([spot_1m_rest]/[option_chain_1m]/[groww_*_1m] enabled=false since
    // 2026-07-17), names behind the non-default `groww_orders` cargo feature,
    // tv_ws_frame_spill_write_errors_total (no WS frame producer exists since
    // the 2026-07-13/15 live-feed retirements), and tv_api_auth_failed_total
    // (already published by its own log metric filter — auth-failed-alarm.tf;
    // EMF-selecting it would double-bill). Full rationale + the exclusion
    // ledger: the COST NOTE above the CWCFG heredoc in user-data.sh.tftpl.
    // 54 (was 52) since 2026-08-12 (SILENCE READ-OUT): the lane seeded every
    // subscribed instrument into a TickGapDetector and called observe() on
    // every tick, while `scan_silence` had ZERO production callers — a fully
    // wired sensor with no read-out, which reads greener than dead code
    // because every part of it looks connected. The scan now runs on its own
    // 30s timer and publishes two gauges, both selected here:
    // tv_dhan_feed_instruments_silent (quiet beyond the instrument's OWN
    // learned cadence, sparse instruments excluded per the §36.4 precedent)
    // and tv_dhan_feed_instruments_never_ticked. The second matters most: a
    // subscribe that silently did not take produces NO other signal — there
    // is no payload to count, no parse to fail, no error to log — so absence
    // against a seeded key is the only evidence that exists, and leaving it
    // in a /metrics endpoint nothing on the box scrapes would repeat the
    // exact mistake the 2026-08-11 and 2026-08-12 additions above corrected.
    // +2 EXACT names ≈ +$0.60/mo (52 → 54 ≈ $15.60 → $16.20/mo). Dated note:
    // aws-budget.md (COST NOTE 2026-08-12, silence read-out).
    //
    // FLAGGED, deliberately NOT taken here: neither gauge has an ALARM, so
    // today they are visible but not pageable. An alarm needs the
    // market-hours window gate (its ALARM_NAMES list arms a named set), which
    // is its own terraform change — recorded rather than left to be
    // discovered from a quiet dashboard.
    // +1 EXACT name 2026-08-12 (54 → 55 ≈ $16.20 → $16.50/mo):
    // `tv_dhan_feed_stack_connections` — the per-endpoint OPEN-SOCKET gauge
    // (`FEED_STACK_CONNECTIONS_GAUGE`, dhan_feed_stack.rs), labelled
    // main_feed / depth_20 / depth_200.
    //
    // Why it belongs here rather than in the "visible enough" pile: the
    // operator asked how many of the 16 authorized connections were actually
    // open, and the ONLY way to answer was to `filter-log-events` the app log
    // for the planning line. The gauge was emitted by the binary and selected
    // by NOTHING — the same shape as `tv_dhan_ws_ring_bytes_full_total` before
    // 2026-08-09 and `tv_ws_frame_spill_write_errors_total` before 2026-08-12.
    // A socket count that lives only in a log line cannot carry an alarm and
    // cannot be charted, so "is the feed actually connected?" stayed a
    // grep-the-logs question on a system whose whole point is live capture.
    //
    // FLAGGED, deliberately NOT taken here: this gauge has no ALARM either.
    // The obvious one — main_feed drops to 0 during market hours — needs the
    // market-hours window gate's ALARM_NAMES list, which is its own terraform
    // change. Recorded rather than left to be discovered from a quiet
    // dashboard.
    // +9 EXACT names 2026-08-14 (55 → 64): the lane's own LIVENESS and LOSS
    // signals. The trigger is a finding that is worse than any single missing
    // metric — of every `tv_dhan_feed_*` series the binary emits, the ONLY one
    // selected was `tv_dhan_feed_stack_connections`, a BOOT-TIME CONSTANT that
    // reports "5 depth-20" whether or not a single byte ever arrives. So the
    // one lane signal reaching CloudWatch was the one that cannot be wrong,
    // while every signal that could reveal a dark feed — frames drained, ticks
    // ingested, the stack's own up bit, dial failures — reached only the log
    // sink. That is the exact shape of the 2026-08-12 blackout: 12 consecutive
    // HTTP 400 dial failures and `compared: 0` for a whole session, with a
    // connection gauge that read healthy throughout.
    //
    // The nine: tv_dhan_feed_stack_up (the lane's own alive bit),
    // tv_dhan_feed_drain_frames_total (frames actually drained, by outcome),
    // tv_dhan_feed_ingest_ticks_total (ticks actually folded),
    // tv_dhan_feed_ingest_refused_total, tv_dhan_ws_reconnect_total,
    // tv_dhan_ws_park_total (a parked socket is a PERMANENTLY dark shard —
    // ParkReason::FatalDisconnect is never re-dialed),
    // tv_dhan_ws_dial_failed_total (the 2026-08-12 class itself),
    // tv_ticks_lost_total (the workspace's only explicit tick-loss SLA
    // counter), tv_ws_frame_spill_drop_critical (the WAL durable-floor breach).
    //
    // HONEST COST: +9 NAMES ≈ +$2.70/mo by the per-name arithmetic used above
    // ($16.50 → ~$19.20), but the true bill is HIGHER than the name count
    // implies and this note must not understate it: CloudWatch bills per
    // metric, and a name with labels is many metrics.
    // tv_dhan_feed_drain_frames_total carries 8 `outcome` values and
    // tv_ticks_lost_total carries source × ws_type, so the realistic addition
    // is ~20 series ≈ $6/mo, not $2.70. Against the $100 kill-ceiling and a
    // ~$58–74/mo envelope that is affordable; it is recorded at the honest
    // number rather than the flattering one.
    //
    // FLAGGED, deliberately NOT taken here, and it is the bigger gap: NONE of
    // these nine has an ALARM — and neither does any of the 14 Dhan-lane names
    // already in this list. The lane's entire failure surface is now published
    // and nothing looks at it, so the chain still ends at "a human opens a
    // dashboard", which the zero-manual-intervention mandate forbids. Alarms
    // need the market-hours window gate's ALARM_NAMES list (its own terraform
    // change) and a sustained baseline these series do not yet have. Recorded
    // here rather than left to be discovered from a quiet dashboard.
    let user_data = read(DEPLOYED_CW_AGENT_CONFIG);
    let names = emf_declared_names(&user_data, "metric_selectors");
    // 2026-08-14: 64 -> 65. ONE name added, `tv_dhan_ws_lag_excluded_total`,
    // alongside the first live-socket delivery-lag measurement. Cost ~$0.30/mo
    // against the $100 kill-ceiling — deliberate, not a drive-by.
    //
    // A SECOND name was added and then REMOVED again in the same change, and
    // the reason is worth keeping: `tv_dhan_ws_lag_ms` is a HISTOGRAM. A
    // Prometheus histogram is exposed as `_bucket{le=…}` / `_sum` / `_count`,
    // and this selector is anchored `^(…)$`, so the bare name matches NOTHING.
    // Adding it would have published no datapoint while looking correct in the
    // diff — a false-OK, and one this list is especially prone to because every
    // name in it today is a counter or a gauge. (`tv_order_fill_lag_seconds`
    // reads like a counter-example but has ZERO source references: it is a dead
    // selector entry, not a working histogram precedent.)
    //
    // The histogram is therefore NOT EMF-shipped. It lives on `/metrics` for
    // the operator console to scrape. Shipping its buckets would also multiply
    // cost by the bucket count × 16 connections, which is precisely the kind of
    // cardinality this ratchet exists to make someone think about.
    //
    // 2026-08-15: 65 -> 67. TWO gauges added with host-adaptive ring sizing —
    // `tv_host_total_ram_bytes` (what the process actually measured about its
    // machine) and `tv_dhan_feed_ring_max_bytes` (what it decided as a result).
    // Cost ~$0.60/mo; dated note in `aws-budget.md`.
    //
    // They are published as a PAIR deliberately. The ring budget is now derived
    // at runtime rather than being a compile-time constant, so "what is the
    // buffer?" stops being answerable by reading the source. Publishing only the
    // decision would leave a number nobody can check; publishing only the input
    // would leave the decision invisible. Together they make a mis-sized ring —
    // a fallback on an unreadable /proc/meminfo, or a clamp firing — visible as
    // an arithmetic disagreement between two series, without opening a shell on
    // the box.
    // 2026-08-15 (+1, ~$0.30/mo): `tv_dhan_feed_depth_total`, added when
    // depth-20 and depth-200 stopped being captured-then-discarded and became a
    // persisted stream (operator directive: one common `market_depth` table,
    // "we cannot miss or hide or wipe off anything").
    //
    // ONE name carrying an `outcome` label — rows / refused / dropped /
    // disconnects / length_mismatch — rather than five names. That was not a
    // stylistic preference: five names pushed `user-data.sh.tftpl` 64 bytes
    // past the size guard's budget, and that guard explicitly forbids buying
    // room by shaving unrelated blocks. The label shape (the same one
    // `tv_dhan_feed_drain_frames_total` already uses) fits in one selector
    // entry AND ships every outcome, so nobody had to choose which losses were
    // worth seeing. The two that matter most are the two that answer "did a
    // level that arrived fail to reach the table": `refused` (never stored —
    // parse error, unmappable segment, truncated tail, ILP append failure) and
    // `dropped` (stored in the buffer, then lost at a failed flush — the drain
    // mirrors the writer's discard DELTA into this counter precisely so a
    // database-side depth loss is visible in CloudWatch at all).
    //
    // They are published as a PAIR deliberately. The ring budget is now derived
    // at runtime rather than being a compile-time constant, so "what is the
    // buffer?" stops being answerable by reading the source. Publishing only the
    // decision would leave a number nobody can check; publishing only the input
    // would leave the decision invisible. Together they make a mis-sized ring —
    // a fallback on an unreadable /proc/meminfo, or a clamp firing — visible as
    // an arithmetic disagreement between two series, without opening a shell on
    // the box.
    // 2026-08-15 (+1, ~$0.30/mo): `tv_dhan_feed_depth_total`, added when
    // depth-20 and depth-200 stopped being captured-then-discarded and became a
    // persisted stream (operator directive: one common `market_depth` table,
    // "we cannot miss or hide or wipe off anything").
    //
    // ONE name carrying an `outcome` label — rows / refused / dropped /
    // disconnects / length_mismatch — rather than five names. That was not a
    // stylistic preference: five names pushed `user-data.sh.tftpl` 64 bytes
    // past the size guard's budget, and that guard explicitly forbids buying
    // room by shaving unrelated blocks. The label shape (the same one
    // `tv_dhan_feed_drain_frames_total` already uses) fits in one selector
    // entry AND ships every outcome, so nobody had to choose which losses were
    // worth seeing. The two that matter most are the two that answer "did a
    // level that arrived fail to reach the table": `refused` (never stored —
    // parse error, unmappable segment, truncated tail, ILP append failure) and
    // `dropped` (stored in the buffer, then lost at a failed flush — the drain
    // mirrors the writer's discard DELTA into this counter precisely so a
    // database-side depth loss is visible in CloudWatch at all).
    //
    // FLAGGED, not hidden: the template now renders to 15,870 bytes against a
    // 15,872 budget. TWO bytes. The next selector addition WILL fail this
    // guard, and the correct response is the one the guard itself prescribes —
    // move content out of user-data into a file copied in after the repo
    // clone — not another round of name-shortening.
    //
    // HONEST: this is SHIPPED but not ALARMED. It is queryable and
    // dashboard-able today; paging on it is a new Dhan-scoped alert, which
    // `dhan-rest-only-noise-lock-2026-07-14.md` §3 REJECTs without its own
    // dated row in THAT file first. Visible now, pageable after that edit.
    //
    // MERGE 2026-08-15: 65 -> 71. Both sides added to this list independently
    // (2 here, 4 on main) and the union is the only correct resolution — an
    // allowlist is what makes a metric reach CloudWatch at all, so dropping
    // either side's entries would silently un-ship metrics whose alarms and
    // dashboard widgets landed with them.
    // 2026-08-18: 71 -> 72. Added `tv_dhan_feed_seals_emitted_total`.
    //
    // The loss widget charted "candles discarded" while NOTHING charted
    // candles produced, so a flat-zero discard line read identically whether
    // the lane was healthy or emitting nothing at all — the exact ambiguity
    // the throughput widget's own header says it exists to kill, left open one
    // stage further down the chain (frames -> ticks -> [gap] -> discarded).
    // Dated cause: the "Candle seals emitted" widget was retired 2026-07-17
    // when its metric died with the tick aggregator; the aggregator was
    // REBUILT 2026-08-09 under a new name and nothing restored it.
    //
    // ON THE SIZE WARNING ABOVE: that paragraph says the template renders to
    // 15,870 bytes against a 15,872 budget and that "the next selector
    // addition WILL fail this guard". Measured today it renders to 15,740
    // against the real 16,384 EC2 cap — 644 bytes free — and
    // `user_data_size_guard` passes with this addition in place. The template
    // shrank after that note was written. The note was not wrong when written;
    // it went stale, which is why this one states the MEASURED number and the
    // command that produces it rather than repeating a remembered one.
    //
    // COST: one additional EMF metric, ~$0.30/mo. This is a SUCCESS signal,
    // deliberately: every one of the 2026-08-11/12 additions was a loss
    // counter, and a dashboard that can only show failure cannot distinguish
    // "nothing broke" from "nothing ran".
    // 2026-08-21: 72 -> 73. Added `tv_dhan_contract_universe_failed_total`.
    //
    // `dhan_contract_universe.rs` carried ZERO metrics calls until this date.
    // "No options resolved today", "the ATM window shrank to three", "the
    // artifact was unreadable" were error! lines and struct fields that nothing
    // consumed — a session missing ~22,000 authorized contracts left no number
    // anywhere for an alarm or a triage path to read. The 2026-08-20 incident
    // is the shape: atm_window_reason = "no_ladders" was recorded, printed, and
    // ignored.
    //
    // ONE name, not six, and the `reason` label carries the classification.
    // That is forced by two things: the EMF processor folds labels into the
    // single declared dimension set by summing, so a name that also carried
    // successes would alarm on a healthy day (hence every reason value is a
    // defect); and the rendered user-data template has limited headroom, so
    // each added name is measured rather than assumed.
    //
    // MEASURED with this addition in place: user-data.sh.tftpl renders to
    // 15,795 bytes against the 15,872 budget (16,384 EC2 cap minus the 512
    // required margin) — 77 bytes free, and `user_data_size_guard` passes.
    // Reproduce with `wc -c deploy/aws/terraform/user-data.sh.tftpl`. That is
    // thin: the NEXT selector addition should follow the size guard's own
    // prescription and move content OUT of user-data rather than shave
    // comments to make room.
    //
    // COST: one additional EMF metric, ~$0.30/mo, plus its alarm at ~$0.10/mo.
    // One series, since the reason label folds. Authorization for the alarm is
    // the dated §2.3b row in dhan-rest-only-noise-lock-2026-07-14.md.
    // 2026-08-21 — 73 -> 76: the SPILL TIER becomes visible.
    //
    // tv_ticks_spilled_total, tv_tick_spill_replay_failed_total and
    // tv_tick_spill_replayed_bytes_total. The rescue tier had NO CloudWatch
    // presence at all: an ILP flush failing and being rescued to disk, and the
    // automatic drain failing to put it back, were both visible only in the
    // box's own log. A spill that is never drained becomes a real tick loss at
    // the 512 MiB cap, and nothing outside the box would have said so.
    //
    // Two carry alarms (tv-<env>-ticks-spilling, tv-<env>-tick-spill-replay-failing,
    // both market-hours gated); replayed_bytes ships WITHOUT one deliberately —
    // it is the SUCCESS signal, and a chart of recoveries belongs beside the two
    // failure alarms without adding a third pager. That is the one exception to
    // this ratchet's own "a metric with nothing watching it is the
    // paid-for-and-unwatched shape" rule, and it is exercised knowingly: the
    // success series is what makes the two failure alarms interpretable.
    //
    // +3 names ~= $0.90/mo, +2 alarms ~= $0.20/mo => ~$1.10/mo against the $130
    // kill-ceiling (90% action line $117). Authorization: the dated §2.3c row in
    // dhan-rest-only-noise-lock-2026-07-14.md.
    //
    // 2026-08-21, SAME DAY, on the merge with the operator-lock branch:
    // 73 -> 76. Net +3, from four additions and one removal.
    //
    //   + tv_dhan_feed_last_tick_age_secs   the no-ticks alarm's new source.
    //     It replaced tv_dhan_feed_ingest_ticks_total because a COUNTER's
    //     meaning depends on whether the agent publishes per-scrape deltas or
    //     the running total, and under the second reading `Sum < 1` stops
    //     being true after the first tick of the morning — the alarm written
    //     to prove ticks are flowing would have reported health all day. A
    //     gauge means the same thing under either reading.
    //   + tv_depth_rows_dropped_total       depth row LOSS, emitted for months
    //   + tv_depth_persist_errors_total     and shipped by NOTHING, while the
    //     rows-WRITTEN counter beside them WAS shipped. Depth read healthy
    //     off-box while its losses were unobservable, and HOT-PATH-02 has no
    //     errcode alarm either, so there was no second path.
    //   + tv_ilp_rows_discarded_total       the new ILP retention bound. A
    //     bound that discards invisibly is worse than the leak it replaced.
    //   - tv_order_fill_lag_seconds         the ONLY entry in this whole list
    //     with zero emit sites in crates/*/src. Nothing could ever publish it,
    //     so removing it changes no observable behaviour — but it DOES change
    //     the Phase-1 arming contract, and order-side-alarms.tf now records
    //     that the arming PR must restore it alongside the emit site or it
    //     arms a permanently-INSUFFICIENT_DATA pager.
    //
    // COST: +4 names, -1 name = net +3 x ~$0.30/mo ~= +$0.90/mo, plus one
    // alarm at ~$0.10/mo (the no-ticks alarm was repointed, not added).
    //
    // The comment above says the NEXT addition should move content OUT of
    // user-data rather than shave comments. That prescription was FOLLOWED,
    // and it is why two further signals are absent: tv_risk_mark_rejected_total
    // and tv_dhan_feed_silence_detector_refused are emitted by production code
    // and reach NO selector, because they did not fit. Nothing was shaved to
    // make room for them. deploy/aws/EMF-METRIC-SELECTOR-NOTES.md carries the
    // full record, including the append-config restructure that ends the
    // rationing and why it was not attempted blind.
    // 2026-08-21, ON THE MERGE of the two blocks above: 77.
    //
    // They were written in parallel against the same list. This branch added
    // the three SPILL-TIER names; main added four (last_tick_age, two depth
    // loss counters, the ILP retention bound) and removed one
    // (tv_order_fill_lag_seconds, zero emit sites). This branch also removed
    // the two Groww REST persist-error counters with the feed that wrote
    // them -- see the note in the failure message below.
    //
    // 76 (main) - 2 (Groww) + 2 (spill) = 76. Two notes on that arithmetic:
    //
    // tv_order_fill_lag_seconds is NOT restored -- main's removal was
    // deliberate and its Phase-1 arming contract in order-side-alarms.tf
    // still holds.
    //
    // And only TWO of the three spill names ship, not three. The block above
    // argued for shipping tv_tick_spill_replayed_bytes_total without an alarm
    // because a chart of successful recoveries makes the two failure alarms
    // interpretable. That argument loses to a hard wall: with all three the
    // rendered user-data came out 16 bytes past the guard's 512-byte margin
    // under AWS's 16,384-byte cap. Given a real byte budget, the ALARMED
    // names win and the unalarmed one is the one that goes -- which is this
    // ratchet's own stated rule applied to its own exception. The counter is
    // still emitted and still on the box's /metrics; only its CloudWatch
    // series is gone, and live-lane-alarms.tf records that.
    assert_eq!(
        names.len(),
        76,
        "Z+ L2 VERIFY ratchet: expected exactly 76 names in the MAIN EMF \
         metric_selectors list (11 post-stage-4, plus the 30 failure/saturation/loss \
         names added 2026-08-09 for the metric-blindness fix, plus the 7 Dhan live-lane \
         loss counters added 2026-08-11 when the lane was switched on, plus the 4 \
         PERSIST-side loss counters added 2026-08-12 — tv_ticks_dropped_total, \
         tv_tick_persist_errors_total, tv_tick_rows_refused_total, \
         tv_ws_frame_spill_write_errors_total. The 2026-08-11 addition instrumented \
         the lane at the SOCKET and left it blind at the DATABASE; the spill-write \
         counter had been excluded on the stated grounds that 'no WS frame producer \
         exists', which stopped being true the day the lane was revived, plus the 2 \
         host-sizing gauges added 2026-08-15 — tv_host_total_ram_bytes and \
         tv_dhan_feed_ring_max_bytes — which make a runtime-derived buffer budget \
         checkable instead of source-readable; plus the 4 added on main the same \
         day, see below); \
         plus the 2 \
         UNIVERSE-INTEGRITY names added 2026-08-14 — tv_dhan_live_universe_fallback_total \
         and tv_dhan_live_universe_instruments. Those close the worst blind spot an \
         adversarial sweep found: when the resolved-master artifact is missing, the lane \
         subscribes 4 instruments instead of 4,565 and every OTHER signal reads healthy \
         — lane up, ticks flowing, never-ticked zero, because only the subscribed set is \
         seeded. +$0.60/mo; plus tv_dhan_ws_alive_connections, added the same day — \
         the lane had NO signal for PARTIAL socket loss, because stack_up clears only \
         when the frame ring closes (every sender dropped) and the planned-connections \
         gauge is a boot-time constant, so four of five sockets could park with both \
         reading healthy. +$0.30/mo; plus tv_ws_frame_wal_reinjected_dropped_total, added \
         the same day — the write-ahead log was WRITE-ONLY until 2026-08-14: frames staged \
         at boot were counted, logged and discarded, so a session that died mid-market lost \
         everything captured since its last flush and no metric anyone watched moved. \
         +$0.30/mo. \
         \
         DELIBERATELY NOT SHIPPED, on the merge with #1753: tv_dhan_live_universe_instruments \
         and tv_dhan_wal_refold_total. Both are DIAGNOSTIC — no alarm consumes either — and \
         the EMF selector lives inside user-data.sh.tftpl, which #1753 had just trimmed to fit \
         the EC2 16 KiB cap. Rather than shave unrelated blocks (which the size guard \
         explicitly forbids), the two unalarmed names were cut. That is the consistent call, \
         not a byte workaround: a metric shipped to CloudWatch with nothing watching it is \
         the paid-for-and-unwatched shape these very alarms were added to end. Both remain \
         on /metrics for the operator console to scrape. \
         \
         2026-08-21, MINUS 2: tv_groww_spot1m_persist_errors_total and \
         tv_groww_chain1m_persist_errors_total left the selector with the feed that \
         wrote them. Both were persist-error counters on the Groww REST 1m legs, which \
         the operator ordered removed entirely; with no producer they would have become \
         permanently-empty paid series and two flat-zero dashboard lines that read as \
         proof of health. Removed in lockstep with the two dashboard.tf widget rows and \
         the /health runtime-subsystem rows, per the dated authorization in \
         websocket-connection-scope-lock.md. -$0.60/mo.); \
         found {}: \
         {names:?}. Adding a name costs ~$0.30/mo against a $100 kill-ceiling whose \
         budget actions STOP the prod box at 90% — update this count deliberately, \
         with a dated cost note, never as a drive-by.",
        names.len()
    );
    for required in [
        "tv_process_rss_bytes",
        "tv_subsystem_memory_estimated_bytes",
        // tv_dhan_exchange_lag_p99_seconds + tv_dhan_lag_samples_excluded_total
        // retired 2026-07-17 (dashboard tidy — dead Dhan-lag chain deleted).
        "tv_rest_1m_fire_heartbeat",
        // 2026-07-14 cluster-C order-side (dormant until cluster A / Phase-1):
        "tv_daily_pnl",
        // tv_order_fill_lag_seconds REMOVED from this required list 2026-08-21,
        // deliberately and not to make an unrelated change fit.
        //
        // It was the ONLY entry in the whole selector with zero emit sites in
        // crates/*/src — `grep -rl tv_order_fill_lag_seconds crates/*/src`
        // returns nothing — so shipping it published nothing at all. Its own
        // alarm is `actions_enabled = false` and its description says so.
        //
        // The user-data byte budget is a hard 16,384-byte AWS limit that
        // terraform refuses a PLAN above, and on 2026-08-21 three genuinely
        // BLIND live counters (depth row drops, depth persist errors, the ILP
        // overflow bound) could not fit beside it. Between a name nothing can
        // publish and three that report real data loss, the three won.
        //
        // This is NOT the silent shrinkage this list exists to catch. That
        // failure is a live family quietly losing a member; this is a staging
        // placeholder for work Phase-1 owns, and the staging contract survived
        // the removal rather than being dropped with it:
        // order-side-alarms.tf now records that the Phase-1 arming PR must
        // restore this name to BOTH selector copies in the same change as the
        // emit site, and re-check the byte budget, or it arms a
        // permanently-INSUFFICIENT_DATA pager.
        //
        // Re-adding it here is correct AT THAT POINT and wrong before it.
        // 2026-08-09 metric-blindness fix — one representative per family, so
        // a partial revert of the widening fails loudly instead of silently
        // shrinking the operator's only metric sink:
        "tv_spot1m_persist_errors_total", // Dhan REST leg persist failure
        "tv_chain1m_persist_errors_total", // Dhan chain leg persist failure
        "tv_cadence_ladder_exhausted_total", // retry ladder gave up = minute lost
        "tv_questdb_wal_suspended_tables", // QuestDB silently stops accepting writes
        "tv_order_audit_rows_discarded_total", // SEBI 5-yr audit row loss
        "tv_oom_kills_total",             // host memory saturation
        "tv_ram_store_dropped_total",     // RAM decision surface dropping rows
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "Z+ L2 VERIFY ratchet: {required} must be in the MAIN EMF metric_selectors \
             list (2K-universe memory measurement + 2026-07-06 silent-feed lag signals \
             read them as real CloudWatch metrics)."
        );
    }
}

#[test]
fn test_boundary_catchup_emf_declaration_stays_retired() {
    // RETIRED 2026-07-17 (stage-3 dead-WS sweep): the original
    // test_second_emf_declaration_publishes_boundary_catchup_per_feed pinned
    // the [host,feed] declaration for tv_boundary_catchup_total. Its writer
    // — the watermark catch-up sealer inside the 21-TF tick aggregator — is
    // DELETED (no tick publisher exists on the REST-only runtime), so the
    // declaration was a dead selector. Negative pin: neither agent config
    // may re-declare the metric (re-adding it without a live writer would
    // ship a permanently-empty paid series).
    for rel in [DEPLOYED_CW_AGENT_CONFIG, "deploy/aws/cloudwatch-agent.json"] {
        let body = read(rel);
        assert!(
            !body.contains("tv_boundary_catchup_total"),
            "stage-3 retirement pin: {rel} must NOT declare \
             tv_boundary_catchup_total — its writer (the tick aggregator's \
             catch-up sealer) was deleted 2026-07-17; a re-added selector \
             would publish a permanently-empty series."
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
    // PR-C2 (2026-07-13): tv_realtime_guarantee_score left this list — its
    // fallback filter retired with the PARKed SLO publisher.
    let tf = read("deploy/aws/terraform/metrics-log-metric-filters.tf");
    {
        let metric = "tv_boot_completed";
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
    // 2026-07-06: union across ALL metric_declaration entries. (2026-07-17:
    // the per-feed [host,feed] boundary-catchup declaration + its alarm
    // retired with the stage-3 tick-aggregator deletion.)
    let user_data = read(DEPLOYED_CW_AGENT_CONFIG);
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
fn the_deployed_agent_config_is_the_repo_file_and_has_no_second_copy() {
    // RETIRED-AND-REPLACED 2026-08-25.
    //
    // This test used to compare the EMF name-set in `cloudwatch-agent.json`
    // against a byte-identical duplicate embedded in `user-data.sh.tftpl`.
    // That duplicate is gone: the template now writes a minimal host-only
    // fallback and copies the repo file into place after the Step 5 clone, so
    // there is exactly ONE declaration and a drift comparison would be the
    // file against itself — vacuously green, which is worse than absent.
    //
    // What replaces it is the property that actually has to hold now: the
    // template must not re-embed a selector, and it must still install the
    // repo file. Dropping the install while the selector stays absent would
    // leave every box on the host-only fallback publishing no app metrics at
    // all — a much quieter failure than drift ever was, so it needs a pin.
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    let reference_names = emf_declared_names(&read(DEPLOYED_CW_AGENT_CONFIG), "metric_selectors");
    assert!(
        !reference_names.is_empty(),
        "ratchet self-check: could not parse metric_selectors from {DEPLOYED_CW_AGENT_CONFIG}"
    );
    assert!(
        emf_declared_names(&user_data, "metric_selectors").is_empty(),
        "the EMF metric-selector list is back inside user-data.sh.tftpl. Two copies means \
         two things to keep in sync, and the ~1.6 KB duplicate is what pinned that template \
         at exactly 0 bytes free under the EC2 16 KiB cap."
    );
    assert!(
        user_data.contains(DEPLOYED_CW_AGENT_CONFIG) && user_data.contains("fetch-config"),
        "user-data must copy {DEPLOYED_CW_AGENT_CONFIG} into place after the clone AND \
         apply it with amazon-cloudwatch-agent-ctl -a fetch-config. Without both, the box \
         runs the host-only fallback forever and no app metric reaches CloudWatch."
    );
}

#[test]
fn test_emf_metric_namespace_is_tickvault_prod_in_both_configs() {
    // The alarms in app-alarms.tf all key on namespace="Tickvault/Prod".
    // If the agent's emf_processor.metric_namespace ever changes, every
    // metric lands in a namespace no alarm reads → silent 0 datapoints.
    for rel in [DEPLOYED_CW_AGENT_CONFIG, "deploy/aws/cloudwatch-agent.json"] {
        let body = read(rel);
        assert!(
            body.contains("\"metric_namespace\": \"Tickvault/Prod\""),
            "Z+ L2 VERIFY drift-guard: {rel} must set emf_processor.metric_namespace to \
             exactly \"Tickvault/Prod\" — the namespace every app-alarms.tf alarm reads."
        );
    }
}

// Helpers alarm_resource_block / block_has_attr DELETED 2026-07-17 with
// their last caller (test_silent_feed_alarms_are_window_gated — both
// silent-feed alarms retired the same day; see
// test_silent_feed_alarms_fully_retired). Recover from git history if a
// silent-feed alarm is ever re-added.

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
    for rel in [DEPLOYED_CW_AGENT_CONFIG, "deploy/aws/cloudwatch-agent.json"] {
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

// RETIRED (PR-C3, 2026-07-14 — tick-gap detector deletion, operator Q4-ii
// 2026-07-13 per websocket-connection-scope-lock.md "2026-07-13 Amendment"
// §B item 4): test_tick_gap_silent_alarm_threshold_is_forty,
// test_tick_gap_silent_alarm_is_window_gated and
// test_tick_gap_silent_gauge_producer_pins_pre_open_to_zero died with the
// `tv-<env>-tick-gap-instruments-silent` alarm + its main.rs gauge producer
// — the per-SID tick-gap detector was deleted (fed only by the retired Dhan
// WS pipeline), so `tv_tick_gap_instruments_silent` is never written again
// and keeping the alarm would orphan a dead monitor. The 2026-07-06/07-08
// retune history (threshold 40, 10-of-12, pre-open pin) lives in git
// history + the dated note in app-alarms.tf.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion):
// test_realtime_guarantee_degraded_alarm_threshold_matches_slo_warn died with
// the alarm it pinned — realtime_guarantee_degraded was removed from
// silent-feed-alarms.tf because the SLO publisher is PARKED (wave-3-d
// banner; no tv_realtime_guarantee_score is ever published again). The
// slo_score.rs contract stub itself (with SLO_WARN_THRESHOLD and the
// SLO-01/02/03 variants) was DELETED in the C4 sweep (2026-07-15) —
// a future Groww-scoped SLO re-design starts fresh with its own dated
// operator quote.

#[test]
fn test_silent_feed_alarms_fully_retired() {
    // 2026-07-17: BOTH remaining silent-feed alarms retired the same day —
    // boundary_catchup_storm_dhan with the stage-3 tick-aggregator deletion
    // (its metric's writer died) and dhan_exchange_lag_p99_high with the
    // dead Dhan-lag publisher chain (dashboard tidy). This replaces
    // test_silent_feed_alarms_are_window_gated (its per-alarm loop is now
    // empty) with a NON-VACUOUS retirement pin: no alarm resource may
    // reappear in silent-feed-alarms.tf, and neither retired alarm may
    // reappear in the window-gate Lambda ALARM_NAMES join.
    let tf = read("deploy/aws/terraform/silent-feed-alarms.tf");
    let gate = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");
    assert!(
        !tf.contains("resource \"aws_cloudwatch_metric_alarm\""),
        "silent-feed-alarms.tf must carry ZERO alarm resources after the \
         2026-07-17 retirements — re-adding one needs a dated rule-file \
         note + window-gate wiring + a cost note (aws-budget.md)"
    );
    for name in ["boundary_catchup_storm_dhan", "dhan_exchange_lag_p99_high"] {
        assert!(
            !gate.contains(&format!("aws_cloudwatch_metric_alarm.{name}.alarm_name")),
            "{name} was retired 2026-07-17 and must NOT reappear in the \
             window-gate Lambda ALARM_NAMES join (market-hours-liveness-alarm.tf)"
        );
    }
}

// RETIRED (2026-07-15 — Groww live-feed retirement):
// test_groww_exchange_lag_alarm_shape_is_pinned died with the alarm it
// pinned — groww_exchange_lag_p99_high left silent-feed-alarms.tf (its
// gauge's only sample producer, the Groww bridge, was deleted); the
// market-hours liveness alarm was re-pointed to tv_rest_1m_fire_heartbeat.

/// Strip `#`-comments from an HCL (terraform) body, STRING-AWARE: a `#`
/// inside a double-quoted string (e.g. an alarm_description's "drop #1")
/// is kept as code. Adapted from the self-tested house pattern in
/// `crates/app/tests/seal_drop_paging_wiring_guard.rs` after the
/// 2026-07-10 hostile review proved (empirically — mutation stayed GREEN)
/// that `test_ws_pool_alarms_are_window_gated_not_always_armed` passed
/// vacuously with a join member commented out: HCL `#` comments already
/// live INSIDE the ALARM_NAMES join body (the dated trailing comments),
/// so a commented-out member line still satisfied a raw `.contains` —
/// the alarm would ship actions_enabled=false and never be armed, with
/// the ratchet green (false-OK, audit Rule 11). Comments can never be
/// terraform configuration, so they are removed before matching.
fn strip_hcl_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut in_str = false;
        let mut esc = false;
        let mut cut = line.len();
        for (i, &b) in bytes.iter().enumerate() {
            if esc {
                esc = false;
                continue;
            }
            match b {
                b'\\' if in_str => esc = true,
                b'"' => in_str = !in_str,
                b'#' if !in_str => {
                    cut = i;
                    break;
                }
                _ => {}
            }
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Locate the ALARM_NAMES join body in a COMMENT-STRIPPED gate-file body.
/// Whitespace-tolerant around the `=` (a future terraform-fmt alignment
/// group padding `ALARM_NAMES        = join(` must not panic the locator),
/// and — because the input is comment-stripped — immune to a stale
/// commented-out copy of the join hijacking the FIRST-occurrence search
/// (the 2026-07-10 review's join-locator finding). The legacy Lambda's
/// `ALARM_NAMES = [n.strip()...]` line is skipped (not followed by
/// `join(`).
fn alarm_names_join_body(stripped_gate: &str) -> &str {
    let mut from = 0;
    while let Some(rel) = stripped_gate[from..].find("ALARM_NAMES") {
        let start = from + rel;
        let after = &stripped_gate[start + "ALARM_NAMES".len()..];
        let after_eq = after.trim_start();
        if let Some(rest) = after_eq.strip_prefix('=') {
            let rest = rest.trim_start();
            if let Some(body) = rest.strip_prefix("join(") {
                let end = body
                    .find("])")
                    .expect("ALARM_NAMES join must close with `])`"); // APPROVED: test
                return &body[..end];
            }
        }
        from = start + "ALARM_NAMES".len();
    }
    panic!("market-hours-liveness-alarm.tf must carry the ALARM_NAMES join"); // APPROVED: test
}

/// True iff a LIVE (non-commented) line of the comment-stripped join body
/// names the alarm member. Line-level prefix matching is the second layer
/// of defense on top of comment stripping: a member can only count when a
/// line's code content STARTS with its resource reference.
fn join_member_present(stripped_join_body: &str, name: &str) -> bool {
    let needle = format!("aws_cloudwatch_metric_alarm.{name}.alarm_name");
    stripped_join_body
        .lines()
        .any(|l| l.trim_start().starts_with(&needle))
}

#[test]
fn test_hcl_stripper_and_join_locator_reject_commented_out_members() {
    // Anti-vacuity MUTATION self-test (2026-07-10 review, HIGH finding):
    // the exact regression shape that made the original ratchet pass
    // vacuously — a member commented out inside the join — must now FAIL
    // the membership check, while a live member (even one carrying a
    // trailing `#` comment, the real file's shape) still passes. Also
    // pins the locator against (a) a stale commented-out COPY of the
    // whole join above the live one and (b) terraform-fmt alignment
    // padding around the `=`.
    let fixture = "      # stale refactor residue — a commented-out copy of the join:\n\
                   # ALARM_NAMES = join(\",\", [\n\
                   #   aws_cloudwatch_metric_alarm.ws_pool_all_dead.alarm_name,\n\
                   # ])\n\
                   ALARM_NAMES        = join(\",\", [\n\
                   aws_cloudwatch_metric_alarm.market_hours_liveness_missing.alarm_name,\n\
                   # aws_cloudwatch_metric_alarm.ws_pool_all_dead.alarm_name,\n\
                   aws_cloudwatch_metric_alarm.ws_failed_connections.alarm_name, # 2026-07-10\n\
                   ])\n";
    let stripped = strip_hcl_comments(fixture);
    let body = alarm_names_join_body(&stripped);
    assert!(
        !join_member_present(body, "ws_pool_all_dead"),
        "MUTATION MUST FAIL: a commented-out join member satisfied the \
         membership check — the vacuous-pass hole is back. Body:\n{body}"
    );
    assert!(
        join_member_present(body, "ws_failed_connections"),
        "a LIVE member with a trailing # comment must still pass:\n{body}"
    );
    assert!(
        join_member_present(body, "market_hours_liveness_missing"),
        "a plain live member must pass:\n{body}"
    );
    // Stripper string-awareness: a `#` inside a double-quoted string is
    // code, not a comment start.
    let s = strip_hcl_comments("alarm_description = \"drop #1 kept\" # trailing gone\n");
    assert!(
        s.contains("drop #1 kept") && !s.contains("trailing gone"),
        "strip_hcl_comments must keep in-string # and drop trailing comments: {s}"
    );
}

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion):
// test_ws_pool_alarms_are_window_gated_not_always_armed died with the alarms
// it pinned — ws_pool_all_dead + ws_failed_connections were removed from
// app-alarms.tf (their pool-watchdog gauge emitters were deleted with the
// lane; dated notes in app-alarms.tf + the gate ALARM_NAMES join).

// RETIRED (2026-07-17 — stage-3 dead-WS sweep):
// test_boundary_catchup_alarm_uses_per_feed_dimensions died with the alarm
// it pinned — boundary_catchup_storm_dhan was removed from
// silent-feed-alarms.tf because its metric's writer (the watermark catch-up
// sealer inside the 21-TF tick aggregator) was deleted; the series can
// never publish again. The stays-retired negative pin for the EMF selector
// lives in test_boundary_catchup_emf_declaration_stays_retired.

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
    // 23 (was 22) since 2026-07-11 (scoreboard PR-C): added
    // `tv_groww_exchange_lag_p99_seconds` (alarm
    // tv-<env>-groww-exchange-lag-p99-high — the Groww mirror of the Dhan
    // lag signal at Groww's millisecond resolution, threshold 5s x10min,
    // window-gated). Cost: +1 custom metric series (~$0.30/mo) + 1 alarm
    // (~$0.10/mo) — dated note in aws-budget.md.
    // 17 (was 23) since 2026-07-13 (PR-C2 — Dhan live-WS lane deletion,
    // operator retirement directive): RETIRED the 6 entries whose emitters
    // died with the lane — tv_websocket_pool_all_dead,
    // tv_websocket_failed_connections_count, tv_realtime_guarantee_score
    // (BOTH the critical + degraded alarms; the SLO publisher is PARKED per
    // the wave-3-d banner), tv_ws_frame_dropped_no_wal_total and
    // tv_ws_reconnect_gap_seconds_total. Cost: -6 alarms / -5 selected
    // series vs the pre-C2 bill (dated notes in app-alarms.tf +
    // silent-feed-alarms.tf).
    // 16 (was 17 on this branch / 22 on main) since 2026-07-14 (operator
    // Dhan noise lock, dhan-rest-only-noise-lock-2026-07-14.md, reconciled
    // through the PR-C2 merge): REMOVED `tv_order_update_ws_active` (alarm
    // tv-<env>-order-update-ws-inactive — deleted with the order-update WS
    // spawn; the alarm was missing-data-blind on dhan-off boots). Cost:
    // -1 alarm (~-$0.10/mo) — dated note in app-alarms.tf output
    // description.
    // 15 (was 16) since 2026-07-14 (PR-C3 — tick-gap detector deletion,
    // operator Q4-ii 2026-07-13): REMOVED `tv_tick_gap_instruments_silent`
    // (alarm tv-<env>-tick-gap-instruments-silent — its gauge producer was
    // deleted, so the alarm would orphan a dead monitor). Cost: -1 alarm
    // (~-$0.10/mo) — dated notes in app-alarms.tf +
    // market-hours-liveness-alarm.tf.
    // 12 (was 15) since 2026-07-15 (Groww live-feed retirement): REMOVED
    // tv_groww_ws_active (alarm tv-<env>-groww-ws-inactive),
    // tv_feed_sidecar_stall_restart_total (alarm
    // tv-<env>-groww-stall-restart-storm) and
    // tv_groww_exchange_lag_p99_seconds (alarm
    // tv-<env>-groww-exchange-lag-p99-high) — their producers (the Groww
    // bridge + sidecar stall watchdog + lag publisher) were deleted with
    // the Groww live feed. Cost: -3 alarms (~-$0.30/mo) — dated note in
    // aws-budget.md (COST NOTE 2026-07-15).
    // 11 (was 12) since 2026-07-15 (same PR, fix round): REMOVED
    // tv_aggregator_seals_emitted_total (alarm
    // tv-<env>-aggregator-no-seals) — the seals metric lost its LAST live
    // producer with the Groww bridge deletion (the Dhan broadcast has been
    // publisher-less since PR-C2), so the alarm was a permanently-dead
    // monitor the window gate kept arming daily. Cost: -1 alarm
    // (~-$0.10/mo) — dated notes in app-alarms.tf section 9 +
    // aws-budget.md (COST NOTE 2026-07-15).
    // 10 (was 11) since 2026-07-17 (stage-3 dead-WS sweep): REMOVED
    // tv_boundary_catchup_total (alarm tv-<env>-boundary-catchup-storm-dhan)
    // — its writer, the tick aggregator's watermark catch-up sealer, is
    // deleted; a retained alarm would orphan a dead monitor the window gate
    // kept arming daily. Cost: -1 alarm (~-$0.10/mo) — dated notes in
    // silent-feed-alarms.tf S2 + aws-budget.md (COST NOTE 2026-07-17).
    // 9 (was 10) since 2026-07-17 (dashboard tidy): REMOVED
    // tv_dhan_exchange_lag_p99_seconds (alarm
    // tv-<env>-dhan-exchange-lag-p99-high) — its only publisher
    // (run_dhan_lag_publisher, dormant since PR-C2) was deleted with the
    // dead Dhan-lag chain, so the alarm was a permanently-missing-data
    // dead monitor. Cost: -1 alarm (~-$0.10/mo) — dated notes in
    // silent-feed-alarms.tf + aws-budget.md (COST NOTE 2026-07-17).
    // 5 (was 9) since 2026-07-18 (stage-4 dead-producer sweep): REMOVED
    // tv_spill_dropped_total (alarm tv-<env>-spill-dropped),
    // tv_dlq_ticks_total (tv-<env>-dlq-ticks), tv_ticks_dropped_total
    // (tv-<env>-ticks-dropped) and tv_late_tick_after_boundary_total
    // (tv-<env>-late-tick-after-boundary) — their emit sites (the
    // tick_persistence.rs ring/spill/DLQ counters + the tick_processor.rs
    // post-close check) were deleted in the stage-2 sweep (2026-07-17), so
    // all four alarms were permanently-dead monitors. Cost: -4 alarms
    // (~-$0.40/mo) — dated notes in app-alarms.tf + aws-budget.md
    // (COST NOTE 2026-07-18).
    let count = alarm_metric_names().len();
    assert_eq!(
        count, 5,
        "Z+ L2 VERIFY ratchet: expected exactly 5 app-level CloudWatch alarm \
         metric_name entries across app-alarms.tf + silent-feed-alarms.tf \
         (one per critical app signal). Found {count}. If you intentionally \
         added or removed one, update aws-budget.md custom-metric cost line \
         AND this guard."
    );
}

// ---------------------------------------------------------------------------
// FULL-CORPUS, CONST-AWARE DEAD-MONITOR CHECK (added 2026-08-25)
//
// `test_every_alarm_metric_has_a_rust_emit_site` above is the original
// dead-monitor ratchet and it is NOT weakened here — it stays exactly as it
// was. What it cannot do was measured on 2026-08-25 and is worth stating
// precisely, because the shape is the one this repository has now been caught
// by eight times: THE GUARD DECIDES WHAT TO READ FROM A HARDCODED LIST.
//
//   * its corpus is two files, named literally at `alarm_metric_names()` —
//     and one of them (`silent-feed-alarms.tf`) has held zero alarms since its
//     retirement, so the effective corpus is ONE file. The tree has 20 files
//     declaring 55 alarm resources; it inspects 5 metric names, about 11% of
//     the ~45 distinct `tv_*` alarm metrics;
//   * its parser is line-wise, so a `metric_name` nested inside a
//     `metric_query { metric { … } }` is invisible — three alarms in the one
//     file it does read are unexamined for that reason;
//   * its needle is `counter!("name"` — LITERAL ONLY. Nine of the fourteen
//     metrics in `live-lane-alarms.tf` are emitted through a `const`
//     (`gauge!(FEED_STACK_UP_GAUGE)`), so merely widening the file list would
//     have produced ~11 false "missing emit site" failures on perfectly
//     healthy metrics — which is why the widening had to come WITH the
//     const resolution, not before it.
//
// The hole that mattered most is none of those three individually. It is that
// a metric name can be declared as a `const`, listed in the EMF selector, and
// alarmed on — and NEVER PASSED TO AN EMIT MACRO. `emf_selector_producer_guard`
// matches the const DECLARATION literal and is satisfied;
// `alarm_metric_has_a_route_guard` checks the TRANSPORT and is satisfied; and
// the ratchet above cannot see consts at all. Three guards, all green, over a
// metric nothing writes.
//
// This test closes that. It reads every `.tf` in the terraform directory,
// takes every `tv_*` metric name (nesting included, since it is not line-
// scoped), and requires each to reach ONE of four honest routes.
// ---------------------------------------------------------------------------

/// Every `metric_name = "tv_..."` in every terraform file, nesting included.
///
/// Deliberately NOT block-scoped to alarm resources: a name declared by a
/// `metric_transformation` is classified as log-derived below and passes
/// trivially, so including it costs nothing and removes a parser that could
/// drift. Terraform-interpolated names (`tv_errcode_${each.key}`) are skipped —
/// they are a template, not a metric.
fn every_tf_metric_name() -> Vec<String> {
    let dir = workspace_root().join("deploy/aws/terraform");
    let mut out: Vec<String> = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        panic!("terraform directory unreadable: {}", dir.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        for line in strip_line_comments(&body).lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("metric_name") else {
                continue;
            };
            if !rest.trim_start().starts_with('=') {
                continue;
            }
            if let Some(start) = rest.find('"')
                && let Some(end) = rest[start + 1..].find('"')
            {
                let name = &rest[start + 1..start + 1 + end];
                if name.starts_with("tv_") && !name.contains("${") {
                    out.push(name.to_string());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Names a `metric_transformation` block publishes — i.e. metrics CloudWatch
/// derives from a log filter, which legitimately have no Rust emit macro.
fn log_filter_derived_names() -> Vec<String> {
    let dir = workspace_root().join("deploy/aws/terraform");
    let mut out: Vec<String> = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        let stripped = strip_line_comments(&body);
        // A `metric_transformation` block is short; 400 bytes covers its
        // name/namespace/value/default_value without reaching the next
        // resource.
        //
        // The attribute is `name`, NOT `metric_name` — that is the terraform
        // spelling, and getting it wrong made three genuinely log-derived
        // metrics (`tv_seal_writer_drain_dropped_total`,
        // `tv_orders_placed_delta_total`, `tv_errcode_hot_path_02`) look like
        // dead monitors on the first run of this guard. `namespace` also
        // begins with "name", so the `=` check below is what separates them.
        for (at, _) in stripped.match_indices("metric_transformation") {
            let end = (at + 400).min(stripped.len());
            for line in stripped[at..end].lines() {
                let trimmed = line.trim();
                let Some(rest) = trimmed.strip_prefix("name") else {
                    continue;
                };
                if !rest.trim_start().starts_with('=') {
                    continue;
                }
                if let Some(start) = rest.find('"')
                    && let Some(stop) = rest[start + 1..].find('"')
                {
                    let name = &rest[start + 1..start + 1 + stop];
                    // A terraform-interpolated name (`tv_errcode_${...}`) is a
                    // template; record the literal PREFIX so the concrete
                    // alarm names it expands to are recognised as derived.
                    let literal = name.split("${").next().unwrap_or(name);
                    if literal.starts_with("tv_") {
                        out.push(literal.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Production Rust sources only — `tests/`, `benches/` and `fuzz/` excluded.
///
/// The ratchet above walks all of `crates/`, so an emit-shaped string inside a
/// test file satisfies it. That is the difference between "someone wrote this
/// metric" and "the running binary writes this metric", and only the second
/// one keeps an alarm from being green over nothing.
fn production_sources() -> Vec<(PathBuf, String)> {
    let mut paths = Vec::new();
    collect_rs_sources(&workspace_root().join("crates"), &mut paths);
    paths
        .into_iter()
        .filter(|p| {
            let s = p.to_string_lossy();
            !s.contains("/tests/") && !s.contains("/benches/") && !s.contains("/fuzz/")
        })
        .filter_map(|p| {
            fs::read_to_string(&p).ok().map(|body| {
                let code_only = strip_line_comments(&body);
                let compact: String = code_only.chars().filter(|c| !c.is_whitespace()).collect();
                (p, compact)
            })
        })
        .collect()
}

/// Identifiers bound to `name` by a `const IDENT: &str = "name";` declaration.
///
/// Both `&str` and `&'static str` compact to something ending in `str=` before
/// the literal, so the search anchors on the literal and walks BACK to the
/// `const` keyword — which is stable under either spelling and under any
/// visibility modifier.
fn const_aliases_for(name: &str, sources: &[(PathBuf, String)]) -> Vec<String> {
    let literal = format!("=\"{name}\";");
    let mut out = Vec::new();
    for (_, compact) in sources {
        for (at, _) in compact.match_indices(&literal) {
            let back = &compact[at.saturating_sub(200)..at];
            let Some(kw) = back.rfind("const") else {
                continue;
            };
            let after = &back[kw + "const".len()..];
            let Some(colon) = after.find(':') else {
                continue;
            };
            let ident = &after[..colon];
            if !ident.is_empty() && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.push(ident.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Is `arg` a reference to `ident`, allowing any path prefix?
///
/// `counter!(NAME)`, `counter!(self::NAME)` and `counter!(crate::a::b::NAME)`
/// are all the same emit; matching on the bare identifier alone would miss the
/// qualified forms, and matching on a substring would let `OTHER_NAME` pass.
fn arg_names(arg: &str, ident: &str) -> bool {
    arg == ident || arg.ends_with(&format!("::{ident}"))
}

/// Does any production source pass `ident` (or `"literal"`) as the FIRST
/// argument of a metrics emit macro?
fn is_emitted_as(first_arg_matches: &dyn Fn(&str) -> bool, sources: &[(PathBuf, String)]) -> bool {
    for macro_name in ["counter!(", "gauge!(", "histogram!("] {
        for (_, compact) in sources {
            for (at, _) in compact.match_indices(macro_name) {
                let rest = &compact[at + macro_name.len()..];
                // The first argument ends at the first `,` or the closing `)`.
                let stop = rest
                    .find(|c| c == ',' || c == ')')
                    .unwrap_or(rest.len().min(120));
                if first_arg_matches(&rest[..stop]) {
                    return true;
                }
            }
        }
    }
    false
}

#[test]
fn every_alarm_metric_reaches_an_emit_or_a_declared_route() {
    let sources = production_sources();
    assert!(
        sources.len() > 50,
        "ratchet self-check: production source walk returned {} files — the \
         crates/ walk or the tests/ filter is broken, and a guard that reads \
         nothing passes everything",
        sources.len()
    );

    let names = every_tf_metric_name();
    assert!(
        names.len() >= 30,
        "ratchet self-check: only {} tv_* metric names found across the \
         terraform tree — the previous measurement was ~45, so the parser or \
         the directory walk has regressed",
        names.len()
    );

    let log_derived = log_filter_derived_names();

    // Metrics published by a Lambda through PutMetricData rather than by the
    // app through a metrics macro. Scoped to the lambda crate so an ordinary
    // app metric can never be excused this way.
    let lambda_sources: Vec<(PathBuf, String)> = sources
        .iter()
        .filter(|(p, _)| p.to_string_lossy().contains("/aws-lambdas/"))
        .cloned()
        .collect();

    // DORMANT: alarmed on purpose ahead of its producer. Each entry needs a
    // reason, and the anti-drift assertions below delete it the moment the
    // reason stops being true.
    let dormant: &[(&str, &str)] = &[(
        "tv_order_fill_lag_seconds",
        "order fill lag — the order path is paper-mode; declared dormant in \
         alarm_metric_has_a_route_guard.rs and cloudwatch_dormant_alarms_guard.rs",
    )];

    let mut missing = Vec::new();
    for name in &names {
        // An entry ending in `_` came from a terraform-interpolated name, so it
        // is a PREFIX covering every concrete metric that template expands to
        // (`tv_errcode_` → `tv_errcode_hot_path_02`, and every other coded
        // error). Anything else must match exactly — a full metric name used
        // as a prefix would silently excuse its neighbours.
        if log_derived
            .iter()
            .any(|d| d == name || (d.ends_with('_') && name.starts_with(d.as_str())))
        {
            continue;
        }
        if dormant.iter().any(|(d, _)| d == name) {
            continue;
        }
        let literal = format!("\"{name}\"");
        if is_emitted_as(&|arg: &str| arg == literal, &sources) {
            continue;
        }
        let aliases = const_aliases_for(name, &sources);
        if aliases
            .iter()
            .any(|ident| is_emitted_as(&|arg: &str| arg_names(arg, ident), &sources))
        {
            continue;
        }
        if lambda_sources
            .iter()
            .any(|(_, compact)| compact.contains(&literal))
        {
            continue;
        }
        missing.push(name.clone());
    }

    assert!(
        missing.is_empty(),
        "DEAD MONITOR: these terraform metrics have no emit macro (literal or \
         const), are not log-filter-derived, are not published by a Lambda, and \
         are not declared dormant. An alarm on such a metric sits permanently \
         green over a signal nothing writes — the exact false-OK this \
         repository has retired twice. Missing: {missing:?}"
    );

    // Anti-drift on the dormant list: an entry that gained a producer, or whose
    // alarm was deleted, must be removed rather than left as a standing excuse.
    for (name, reason) in dormant {
        assert!(
            names.iter().any(|n| n == name),
            "dormant entry `{name}` is no longer a terraform metric — delete it \
             from this list ({reason})"
        );
        let literal = format!("\"{name}\"");
        assert!(
            !is_emitted_as(&|arg: &str| arg == literal, &sources),
            "dormant entry `{name}` has a live emit site now — delete it from \
             this list ({reason})"
        );
    }
}

#[test]
fn the_dead_monitor_check_resolves_consts_not_just_literals() {
    // Anti-vacuity. `tv_dhan_feed_stack_up` is emitted ONLY through the const
    // `FEED_STACK_UP_GAUGE`; if const resolution silently stopped working, the
    // test above would report it (and ten of its neighbours) as dead monitors
    // rather than passing — a loud failure, but for the wrong reason. This
    // pins the mechanism itself, so a resolution regression is diagnosed here
    // instead of read as eleven real defects.
    let sources = production_sources();

    let literal = "\"tv_dhan_feed_stack_up\"".to_string();
    assert!(
        !is_emitted_as(&|arg: &str| arg == literal, &sources),
        "tv_dhan_feed_stack_up is expected to be emitted through a const, not a \
         literal — if it gained a literal emit, pick another const-only metric \
         for this self-test"
    );

    let aliases = const_aliases_for("tv_dhan_feed_stack_up", &sources);
    assert!(
        !aliases.is_empty(),
        "const resolution found no identifier bound to tv_dhan_feed_stack_up — \
         the declaration parser has regressed"
    );
    assert!(
        aliases
            .iter()
            .any(|ident| is_emitted_as(&|arg: &str| arg_names(arg, ident), &sources)),
        "const resolution found {aliases:?} but none is passed to an emit macro \
         — the macro-argument matcher has regressed"
    );

    // A name nothing declares must resolve to nothing, or the parser is
    // matching on something other than the name.
    assert!(
        const_aliases_for("tv_guard_vacuity_sentinel_comment_only_total", &sources).is_empty(),
        "const resolution invented an alias for a metric that is not declared"
    );
}

// ---------------------------------------------------------------------------
// Every Lambda is watched — or its exemption is written down
// (2026-08-25, operator "Fix wbrytjonf dude oaku"; the dated authorization is
//  §2.3f of dhan-rest-only-noise-lock-2026-07-14.md).
//
// A Lambda with no `Errors` alarm fails SILENTLY. Nothing else reports it: a
// throwing invocation writes to its own log group and stops there, and for the
// scheduled ones the next signal is simply the absence of whatever they were
// supposed to do — a box that did not start, a digest that did not arrive, a
// gate that never disarmed.
//
// This was measured, not assumed: 13 `aws_lambda_function` resources, 7 with
// an Errors alarm, SIX without — start_watchdog, hard_stop_guard,
// boot_heartbeat_gate, deploy_watchdog, daily_budget_digest and
// questdb_console_proxy. All six are alarmed in the same change as this guard.
//
// The guard is the part that lasts. Six alarms fix today's list; they do not
// stop the SEVENTH Lambda from arriving unwatched next month, which is exactly
// how these six accumulated — each added by a PR that did not think to, and
// nothing ever decided otherwise. The house failure mode is a set nobody
// enumerated; this enumerates it on every build.
// ---------------------------------------------------------------------------

/// Lambdas deliberately shipped without an `Errors` alarm, each with the
/// reason. EMPTY TODAY — every Lambda in the tree is alarmed.
///
/// Kept as a declared escape hatch rather than a hard rule so a future
/// genuinely-exempt Lambda has an honest home instead of forcing someone to
/// weaken the guard. An entry costs a written reason and is checked from both
/// ends below: it must name a Lambda that still exists AND that is still
/// unalarmed, so an exemption can never outlive what it excuses.
const LAMBDA_ERRORS_ALARM_EXEMPT: &[(&str, &str)] = &[
    // These three DO have an `Errors` alarm; what they lack is an SNS route,
    // so they change state in the console and page nobody. Until 2026-08-25
    // the guard counted them as watched, which certified three dead pagers.
    //
    // They are listed rather than routed because all three are operator
    // CONVENIENCE surfaces, not the trading path: a 3am page because a console
    // proxy threw while nobody was looking at the console is noise, and this
    // file has spent the day removing noise, not adding it. What was wrong was
    // never the choice — it was that the choice was invisible.
    //
    // NOTE the asymmetry deliberately preserved: `questdb_console_front` and
    // `operator_control` predate this work and were always action-less;
    // `questdb_console_proxy` was added on 2026-08-25 mirroring its front
    // sibling exactly. Giving one half of a two-Lambda surface a pager while
    // the other stays silent is a worse inconsistency than either state.
    (
        "questdb_console_front",
        "dashboard-only by design: operator console surface, alarm exists without \
         alarm_actions. Pre-dates 2026-08-25.",
    ),
    (
        "questdb_console_proxy",
        "dashboard-only by design: mirrors questdb_console_front, including its \
         lack of alarm_actions. Added 2026-08-25.",
    ),
    (
        "operator_control",
        "dashboard-only by design: operator control surface, alarm exists without \
         alarm_actions. Pre-dates 2026-08-25.",
    ),
];

/// Every `resource "aws_lambda_function" "<name>"` in the terraform directory.
fn declared_lambda_resources(bodies: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_file, body) in bodies {
        for line in body.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("resource \"aws_lambda_function\" \"") else {
                continue;
            };
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Extract the `aws_lambda_function.<name>` referenced immediately after a
/// `FunctionName` dimension. Tolerates the `[0]` count-index form and the
/// single-line `dimensions = { FunctionName = ... }` form, both of which are
/// live in this tree.
fn function_name_ref(block: &str) -> Option<String> {
    let at = block.find("FunctionName")?;
    let rest = &block[at..];
    let at = rest.find("aws_lambda_function.")?;
    let rest = &rest[at + "aws_lambda_function.".len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(rest[..end].to_string())
}

/// Lambdas covered by an `Errors` alarm on the `AWS/Lambda` namespace.
///
/// Block-scoped rather than line-scoped: `metric_name = "Errors"`,
/// `namespace = "AWS/Lambda"` and the `FunctionName` dimension are three
/// separate lines, and pairing them by proximity would mis-associate two
/// adjacent alarms. Top-level resources in this tree close on a bare `}` at
/// column 0, which is what delimits a block here.
fn lambdas_with_errors_alarm(bodies: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_file, body) in bodies {
        let mut block = String::new();
        let mut inside = false;
        for line in body.lines() {
            if line.starts_with("resource \"aws_cloudwatch_metric_alarm\"") {
                inside = true;
                block.clear();
            }
            if inside {
                block.push_str(line);
                block.push('\n');
                if line == "}" {
                    inside = false;
                    let is_lambda_errors =
                        block.contains("\"Errors\"") && block.contains("\"AWS/Lambda\"");
                    // COVERAGE REQUIRES A ROUTE, not just an alarm (2026-08-25).
                    //
                    // An adversarial re-read found this guard counted
                    // `questdb_console_proxy_errors` as "watched" while it has
                    // no `alarm_actions` at all — it changes state in the
                    // console and pages nobody. The guard therefore certified
                    // a dead pager, which is the exact false-OK its own rule
                    // section forbids elsewhere. A dashboard-only alarm is a
                    // legitimate CHOICE; it just has to be a declared one, so
                    // it goes in DASHBOARD_ONLY_ALARM_LAMBDAS below rather
                    // than passing silently as a pager.
                    let has_route = block.lines().any(|l| {
                        let t = l.trim();
                        t.starts_with("alarm_actions") && !t.contains("= []") && !t.ends_with('[')
                    });
                    if is_lambda_errors && has_route {
                        if let Some(name) = function_name_ref(&block) {
                            out.push(name);
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn terraform_bodies() -> Vec<(String, String)> {
    let dir = workspace_root().join("deploy/aws/terraform");
    let Ok(entries) = fs::read_dir(&dir) else {
        panic!("terraform directory unreadable: {}", dir.display()); // APPROVED: test
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unnamed>")
            .to_string();
        // Strip `#` comments: this guard's OWN comment blocks name Lambdas,
        // and a commented mention must never satisfy or trip it.
        out.push((name, strip_hcl_comments(&body)));
    }
    assert!(
        !out.is_empty(),
        "terraform corpus is EMPTY — the guard would pass vacuously"
    );
    out
}

#[test]
fn every_lambda_has_an_errors_alarm_or_a_declared_exemption() {
    let bodies = terraform_bodies();
    let lambdas = declared_lambda_resources(&bodies);
    let covered = lambdas_with_errors_alarm(&bodies);

    assert!(
        lambdas.len() >= 13,
        "expected at least the 13 Lambdas known on 2026-08-25, found {} — \
         did the enumerator stop matching? A guard that finds no Lambdas \
         passes vacuously, which is the failure this test exists to prevent",
        lambdas.len()
    );

    let mut unwatched: Vec<&str> = Vec::new();
    for l in &lambdas {
        if covered.iter().any(|c| c == l) {
            continue;
        }
        if LAMBDA_ERRORS_ALARM_EXEMPT.iter().any(|(n, _)| n == l) {
            continue;
        }
        unwatched.push(l);
    }

    assert!(
        unwatched.is_empty(),
        "these Lambdas have no ROUTED `Errors` alarm and no declared exemption: {unwatched:?}\n\
         (An alarm with no `alarm_actions` does not count — it pages nobody. Either give \
         it an SNS route, or add it to LAMBDA_ERRORS_ALARM_EXEMPT with a written reason.)\n\
         A Lambda that throws writes to its own log group and stops there — nothing pages, \
         and for a scheduled one the only other signal is the absence of whatever it was \
         supposed to do.\n\
         Fix: add an `aws_cloudwatch_metric_alarm` with metric_name=\"Errors\", \
         namespace=\"AWS/Lambda\" and a FunctionName dimension (copy the \
         `market_open_readiness_lambda_errors` block), or add an entry with a reason to \
         LAMBDA_ERRORS_ALARM_EXEMPT."
    );

    // Both ends of every exemption — an exemption that outlives its reason is
    // a lie that reads like a decision.
    for (name, reason) in LAMBDA_ERRORS_ALARM_EXEMPT {
        assert!(
            !reason.trim().is_empty(),
            "exemption for {name} carries no reason"
        );
        assert!(
            lambdas.iter().any(|l| l == name),
            "LAMBDA_ERRORS_ALARM_EXEMPT names {name}, which is no longer a Lambda in the \
             terraform — delete the stale entry"
        );
        assert!(
            !covered.iter().any(|c| c == name),
            "LAMBDA_ERRORS_ALARM_EXEMPT still excuses {name}, but it now HAS an Errors \
             alarm — delete the entry so the exemption list keeps meaning something"
        );
    }
}

#[test]
fn lambda_errors_alarm_extraction_self_test() {
    // Proves the two extractors bite, so a real regression is reported as
    // itself rather than as an empty corpus.
    let fixture = r#"
resource "aws_lambda_function" "alpha" {
  function_name = "tv-prod-alpha"
}

resource "aws_lambda_function" "beta" {
  count         = var.enabled ? 1 : 0
  function_name = "tv-prod-beta"
}

resource "aws_cloudwatch_metric_alarm" "alpha_errors" {
  metric_name   = "Errors"
  namespace     = "AWS/Lambda"
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  dimensions = {
    FunctionName = aws_lambda_function.alpha.function_name
  }
}

resource "aws_cloudwatch_metric_alarm" "delta_errors" {
  metric_name   = "Errors"
  namespace     = "AWS/Lambda"
  alarm_actions = []
  dimensions = {
    FunctionName = aws_lambda_function.delta.function_name
  }
}

resource "aws_cloudwatch_metric_alarm" "unrelated" {
  metric_name = "Errors"
  namespace   = "Tickvault/Prod"
  dimensions  = { FunctionName = aws_lambda_function.beta[0].function_name }
}
"#;
    let bodies = vec![("fixture.tf".to_string(), fixture.to_string())];
    assert_eq!(declared_lambda_resources(&bodies), vec!["alpha", "beta"]);

    // `beta` is NOT covered: its alarm is on the Tickvault namespace, not
    // AWS/Lambda, so it is not a Lambda-Errors alarm at all. This is the case
    // a namespace-blind matcher would get wrong.
    assert_eq!(lambdas_with_errors_alarm(&bodies), vec!["alpha"]);

    // `delta` has a textbook Lambda-Errors alarm and `alarm_actions = []`, so
    // it pages NOBODY and must NOT count as covered. Before 2026-08-25 it
    // would have — that is how three dead pagers were certified as watched.
    assert!(
        !lambdas_with_errors_alarm(&bodies).contains(&"delta".to_string()),
        "an alarm with `alarm_actions = []` routes nowhere and must not count as coverage"
    );

    // The `[0]` count-index form must resolve to the bare resource name.
    let indexed = "FunctionName = aws_lambda_function.gamma[0].function_name }";
    assert_eq!(function_name_ref(indexed).as_deref(), Some("gamma"));

    // A block with no FunctionName yields nothing rather than panicking.
    assert_eq!(function_name_ref("metric_name = \"Errors\""), None);
}

// ---------------------------------------------------------------------------
// A WS-GAP-03 filter must name WHICH WS-GAP-03
// (2026-08-25, §2.3f REJECT row: "Filters the cross-verify alarm on
//  `WS-GAP-03` alone, or drops the `source` conditions".)
//
// WS-GAP-03 is the WebSocket connection-state code, and it has ~50 emit sites
// in dhan_feed_stack.rs alone — every dial failure, every reconnect, every
// pool-supervisor event. A `{ $.code = "WS-GAP-03" }` filter therefore pages on
// ordinary connection churn, which is the RISK-GAP-03 noise trap (25 pages in
// one session) with fifty times the surface.
//
// §2.3d-i records that exact filter being approved by the operator on a
// recommendation, and caught only because someone counted the emit sites before
// writing the terraform. Nothing pinned the outcome, so the next person to add a
// WS-GAP-03 alarm would have had to rediscover it. This pins it.
// ---------------------------------------------------------------------------

#[test]
fn every_ws_gap_03_filter_carries_a_source_discriminator() {
    let body = read("deploy/aws/terraform/error-code-alarms.tf");
    let stripped = strip_hcl_comments(&body);

    let patterns: Vec<&str> = stripped
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("pattern") && l.contains("WS-GAP-03"))
        .collect();

    assert!(
        patterns.len() >= 3,
        "expected at least the 3 WS-GAP-03 filters known on 2026-08-25 \
         (universe-collapse, xverify-vacuous, xverify-failed), found {} — if they were \
         renamed or removed, update this guard deliberately rather than letting it pass \
         vacuously",
        patterns.len()
    );

    for p in patterns {
        assert!(
            p.contains("$.source"),
            "this WS-GAP-03 filter has no `$.source` discriminator, so it matches all \
             ~50 connection-state emit sites and will page on every reconnect:\n  {p}\n\
             Add the `$.source = \"...\"` condition that identifies the specific emit \
             (see ws-gap-03-universe-collapse / ws-gap-03-xverify-blind)."
        );
    }
}

#[test]
fn ws_gap_03_discriminator_guard_self_test() {
    // The stripper must not let a COMMENTED-OUT bare filter satisfy or trip the
    // check — this guard's own §2.3f comment quotes the bad pattern verbatim.
    let commented = strip_hcl_comments("  # pattern = \"{ $.code = \\\"WS-GAP-03\\\" }\"\n");
    assert!(
        !commented.contains("pattern"),
        "a commented pattern line must be stripped, else the guard reads its own prose"
    );

    // And a real bare filter must be detectable as missing the discriminator.
    let bare = "      pattern     = \"{ $.code = \\\"WS-GAP-03\\\" && $.level = \\\"ERROR\\\" }\"";
    assert!(bare.contains("WS-GAP-03") && !bare.contains("$.source"));
}

// ---------------------------------------------------------------------------
// A Metrics Insights alarm must fit inside AWS's 3-hour evaluation cap
// (2026-08-25 — found by reading the terraform APPLY log after PR #1809
//  merged, not by any PR check.)
//
// `aws_cloudwatch_metric_alarm.disk_fill_rate_high` shipped in #1805 with
// `period = 21600` (6h) x `evaluation_periods = 2` = 12 hours. AWS caps a
// Metrics Insights alarm — one whose metric_query uses a `SELECT ... FROM`
// expression — at a 3-hour evaluation range, and rejected it:
//
//     ValidationError: MetricsInsights monitors cannot be checked across
//     more than 3 hours
//
// `terraform validate` and `terraform plan` both PASSED it. The window is
// checked only by the real PutMetricAlarm call at APPLY time — exactly like a
// CloudWatch filter PATTERN, which plan also treats as an opaque string.
//
// The consequence is the part worth pinning. Terraform stops at the first
// failing resource, so from the moment #1805 merged the apply lane was red and
// EVERY terraform change merged after it sat on main UNDEPLOYED — including
// the eight alarms from #1809, which were green, merged, and doing nothing.
// A red apply lane is not one broken alarm; it is a silent freeze on all
// infrastructure delivery, and nothing in the PR gates can see it.
// ---------------------------------------------------------------------------

/// AWS's documented ceiling for a Metrics Insights alarm's evaluation range.
const METRICS_INSIGHTS_MAX_EVAL_RANGE_SECS: u64 = 3 * 60 * 60;

/// Pull `(name, period, evaluation_periods, is_metrics_insights)` for every
/// alarm resource in the terraform directory.
///
/// Block-scoped for the same reason as the Lambda guard: the three facts sit
/// on separate lines and pairing them by proximity would mis-associate
/// adjacent alarms.
fn alarm_eval_windows(bodies: &[(String, String)]) -> Vec<(String, u64, u64, bool)> {
    let mut out = Vec::new();
    for (_file, body) in bodies {
        let mut block = String::new();
        let mut inside = false;
        for line in body.lines() {
            if line.starts_with("resource \"aws_cloudwatch_metric_alarm\"") {
                inside = true;
                block.clear();
            }
            if !inside {
                continue;
            }
            block.push_str(line);
            block.push('\n');
            if line != "}" {
                continue;
            }
            inside = false;

            // A Metrics Insights alarm is one whose EXPRESSION is a query.
            //
            // Deliberately scoped to the `expression =` line, not the whole
            // block. An adversarial re-read found that `block.contains("SELECT ")`
            // also matches PROSE: `partition_archive_failed` is a plain alarm
            // whose alarm_description quotes "SELECT outcome, count() FROM
            // partition_archive_audit". That misclassified it as Insights (a
            // false failure waiting on any legal window widening) AND let it
            // satisfy the non-vacuity assertion below, so deleting every real
            // Insights alarm would have left this guard "passing" on a comment.
            let is_insights = block.lines().any(|l| {
                let t = l.trim();
                t.starts_with("expression") && t.contains("SELECT ") && t.contains(" FROM ")
            });
            if !is_insights {
                continue;
            }

            let name = block
                .lines()
                .find_map(|l| {
                    l.trim()
                        .strip_prefix("resource \"aws_cloudwatch_metric_alarm\" \"")
                        .and_then(|r| r.find('"').map(|e| r[..e].to_string()))
                })
                .unwrap_or_else(|| "<unnamed>".to_string());

            // Returns (parsed value, saw_the_key_but_could_not_parse).
            //
            // MAX, not first-match. An alarm may carry several `period` lines
            // (a metric_query with a plain `metric {}` block alongside one with
            // a SELECT). Taking the FIRST let a 60-second decorative period
            // mask a 21600-second query period — a 12-hour window reported as
            // 120 seconds, i.e. the exact defect this guard exists to catch,
            // sailing through it.
            let num = |key: &str| -> (Option<u64>, bool) {
                let mut best: Option<u64> = None;
                let mut unparsed = false;
                for l in block.lines() {
                    let t = l.trim();
                    let Some(rest) = t.strip_prefix(key) else {
                        continue;
                    };
                    let Some(rest) = rest.trim_start().strip_prefix('=') else {
                        continue;
                    };
                    match rest.trim().split_whitespace().next().map(str::parse::<u64>) {
                        Some(Ok(v)) => best = Some(best.map_or(v, |b: u64| b.max(v))),
                        // A `period = local.six_hours` or `= var.x` parsed to
                        // None and then `unwrap_or(0)` reported a ZERO-second
                        // window, which always passes. Absent is safe; present
                        // but unreadable is NOT, and must fail loudly.
                        _ => unparsed = true,
                    }
                }
                (best, unparsed)
            };

            let (period_opt, period_unparsed) = num("period");
            let (evals_opt, evals_unparsed) = num("evaluation_periods");
            assert!(
                !period_unparsed && !evals_unparsed,
                "Metrics Insights alarm `{name}` has a non-literal `period` or \
                 `evaluation_periods` (an interpolation or variable), so this guard \
                 CANNOT verify it stays inside AWS's 3-hour cap.\n\
                 Use a literal, or extend this guard to resolve the value — do not \
                 leave it unverifiable, because an over-wide window is rejected only \
                 at APPLY time and freezes the entire apply lane."
            );
            let period = period_opt.unwrap_or(0);
            let evals = evals_opt.unwrap_or(1);
            out.push((name, period, evals, true));
        }
    }
    out
}

#[test]
fn metrics_insights_alarms_stay_inside_the_three_hour_cap() {
    let bodies = terraform_bodies();
    let windows = alarm_eval_windows(&bodies);

    assert!(
        !windows.is_empty(),
        "no Metrics Insights alarm found — this guard would pass vacuously. \
         If the last one was removed, delete this test deliberately."
    );

    for (name, period, evals, _) in &windows {
        let range = period.saturating_mul(*evals);
        assert!(
            range <= METRICS_INSIGHTS_MAX_EVAL_RANGE_SECS,
            "Metrics Insights alarm `{name}` evaluates across {range}s \
             (period {period} x {evals} periods), over AWS's {}s cap.\n\
             PutMetricAlarm will REJECT it with `MetricsInsights monitors cannot be \
             checked across more than 3 hours` — and terraform plan will NOT catch \
             that, so the whole apply lane goes red and every later terraform change \
             stops deploying.\n\
             Fix: shrink `period` (and/or `evaluation_periods`) so their product is \
             at most {}s.",
            METRICS_INSIGHTS_MAX_EVAL_RANGE_SECS,
            METRICS_INSIGHTS_MAX_EVAL_RANGE_SECS
        );
    }
}

#[test]
fn metrics_insights_window_guard_self_test() {
    // The extractor must find the query, the period and the evaluation count,
    // and must ignore a NON-Insights alarm (which has no such cap).
    let over = r#"
resource "aws_cloudwatch_metric_alarm" "too_wide" {
  evaluation_periods = 2
  metric_query {
    id          = "q"
    period      = 21600
    expression  = "SELECT MAX(disk_used_percent) FROM \"CWAgent\" WHERE path = '/'"
  }
}
"#;
    let bodies = vec![("f.tf".to_string(), over.to_string())];
    let w = alarm_eval_windows(&bodies);
    assert_eq!(w.len(), 1, "the Insights alarm must be found");
    assert_eq!(w[0].0, "too_wide");
    assert_eq!(w[0].1 * w[0].2, 43200, "12h must be computed as 12h");
    assert!(w[0].1 * w[0].2 > METRICS_INSIGHTS_MAX_EVAL_RANGE_SECS);

    // A plain metric alarm has no Insights cap and must not be collected —
    // otherwise every long-window alarm in the tree becomes a false failure.
    let plain = r#"
resource "aws_cloudwatch_metric_alarm" "plain" {
  evaluation_periods = 24
  period             = 21600
  metric_name        = "Errors"
  namespace          = "AWS/Lambda"
}
"#;
    let bodies = vec![("f.tf".to_string(), plain.to_string())];
    assert!(
        alarm_eval_windows(&bodies).is_empty(),
        "a non-Insights alarm must not be subject to the Insights cap"
    );

    // --- The three bypasses an adversarial re-read found on 2026-08-25 ---
    // Each defeated the guard at exactly the job it exists for. Pinned here so
    // they cannot come back quietly.

    // BYPASS 1: two metric_query blocks, the SHORT decorative period first.
    // `find_map` took 60 and reported a 120s window for a real 43200s one.
    let two_queries = r#"
resource "aws_cloudwatch_metric_alarm" "masked" {
  evaluation_periods = 2
  metric_query {
    id = "a"
    metric {
      period = 60
    }
  }
  metric_query {
    id          = "b"
    period      = 21600
    expression  = "SELECT MAX(x) FROM \"CWAgent\" WHERE y = 'z'"
  }
}
"#;
    let w = alarm_eval_windows(&[("f.tf".to_string(), two_queries.to_string())]);
    assert_eq!(w.len(), 1, "the Insights alarm must still be found");
    assert_eq!(
        w[0].1 * w[0].2,
        43200,
        "MAX period must win: a decorative 60s period must not mask the 21600s \
         query period (this reported 120s before the fix)"
    );

    // BYPASS 3: `SELECT ... FROM` quoted in PROSE must not classify a plain
    // alarm as Insights. This is live in the tree (partition_archive_failed).
    let prose = r#"
resource "aws_cloudwatch_metric_alarm" "prose_only" {
  evaluation_periods = 1
  period             = 21600
  metric_name        = "Errors"
  alarm_description  = "check retention: SELECT outcome, count() FROM partition_archive_audit"
}
"#;
    assert!(
        alarm_eval_windows(&[("f.tf".to_string(), prose.to_string())]).is_empty(),
        "a SELECT quoted in an alarm_description must NOT be treated as a Metrics \
         Insights query — it caused both a false failure and a vacuity hole"
    );
}

#[test]
#[should_panic(expected = "CANNOT verify")]
fn metrics_insights_guard_refuses_an_unreadable_window() {
    // BYPASS 2: a non-literal period parsed to None, then `unwrap_or(0)`
    // reported a ZERO-second window, which passes every cap. Absent is safe;
    // present-but-unreadable must fail loudly rather than silently pass.
    let interpolated = r#"
resource "aws_cloudwatch_metric_alarm" "unreadable" {
  evaluation_periods = 2
  metric_query {
    id          = "q"
    period      = local.six_hours
    expression  = "SELECT MAX(x) FROM \"CWAgent\" WHERE y = 'z'"
  }
}
"#;
    let _ = alarm_eval_windows(&[("f.tf".to_string(), interpolated.to_string())]);
}
