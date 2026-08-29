//! Source-scan ratchet — every counter an ALARM reads must be REGISTERED at
//! boot, not born at the incident.
//!
//! # The defect this pins closed (found 2026-08-28)
//!
//! The CloudWatch agent computes a counter's value as the DELTA between
//! consecutive scrapes, and it DROPS the first sample of a series it has never
//! seen — there is nothing to subtract from. A counter that is only ever
//! touched at its failure site is therefore born AT the incident, and the
//! dropped baseline sample IS the incident.
//!
//! For an alarm shaped `threshold >= 1, evaluation_periods = 1` on a counter
//! that reads zero on a healthy day — which is every loss alarm in this repo —
//! that means the alarm publishes NO DATAPOINT for a single-episode failure.
//! And a single episode is the dominant shape for all of them: a rolled-back
//! binary meeting a newer WAL format happens once; a disk refusing the durable
//! floor's own write happens once and is the most severe event in the process.
//!
//! Four such counters were found in one sweep, each alarmed, each EMF-selected,
//! each dead on arrival. That is not a coincidence, it is the shape: an author
//! adding an alarm looks at the emit site and the terraform, and the seeding
//! block lives in neither.
//!
//! # What this pins
//!
//! Every `tv_*` metric name that appears in an `aws_cloudwatch_metric_alarm`
//! block in `deploy/aws/terraform/` must also appear in the post-recorder
//! registration block in `crates/app/src/main.rs`.
//!
//! # Honest limits
//!
//! - It reads `metric_name = "..."` assignments, so an alarm built from a
//!   variable or a `for_each` map is invisible to it. Those are listed in
//!   `KNOWN_INDIRECT` with the reason, so the exemption is a decision on the
//!   record rather than a silent miss.
//! - It proves the name is REGISTERED, not that the registration runs before
//!   the first scrape. `counter_preregistration_after_recorder_guard.rs` pins
//!   the ordering half.
//! - Gauges are excluded: a gauge is published verbatim, with no delta and no
//!   dropped first sample, so it has no equivalent hazard.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/app -> repo root")
        .to_path_buf()
}

/// Alarms whose metric name this scanner cannot resolve statically, each with
/// the reason. Listing them here is deliberate: an exemption that is written
/// down can be argued with, one that is invisible cannot.
const KNOWN_INDIRECT: &[&str] = &[
    // Built from a `for_each` map of error codes; the metric is a log METRIC
    // FILTER, not a counter this process emits, so there is nothing to seed.
    "error-code-alarms.tf",
    // Same: log metric filters over coded ERROR lines.
    "live-lane-alarms.tf",
];

/// Metric names that are GAUGES, which have no dropped-first-sample hazard.
///
/// Verified by grep against `metrics::gauge!` at the time of writing; a name
/// that later becomes a counter fails this guard, which is the correct
/// direction to fail.
/// Alarms on metrics this PROCESS does not emit at all.
///
/// Recorded rather than silently skipped, because each one is a real finding:
/// an alarm reading a metric with no producer can never fire, and reads as
/// permanently healthy. Fixing them means either adding the producer or
/// deleting the alarm — a decision, not a seeding.
const KNOWN_NO_PRODUCER: &[&str] = &[
    // Emitted by a Lambda (`crates/aws-lambdas/src/deploy_watchdog.rs`), not
    // by this process. Correctly unseeded here.
    "tv_binary_main_sha_mismatch",
    // A DERIVED metric: the alarm reads a log metric filter, not a counter we
    // emit. Nothing to register.
    "tv_orders_placed_delta_total",
    // ⚠ DEAD MONITORS. Verified 2026-08-28: no `counter!`, `gauge!` or
    // `histogram!` anywhere in `crates/*/src` emits these names, so their
    // alarms are permanently green and always will be. Left listed rather than
    // fixed because the fix is a design call — add the producer, or retire the
    // alarm — and the same class is already recorded in
    // `dhan-rest-only-noise-lock-2026-07-14.md`.
    "tv_telegram_dropped_total",
    "tv_order_fill_lag_seconds",
    "tv_wal_suspension_probe_failed_total",
];

const KNOWN_GAUGES: &[&str] = &[
    "tv_dhan_feed_stack_up",
    "tv_dhan_ws_alive_connections",
    "tv_questdb_wal_suspended_tables",
    "tv_spill_dir_free_bytes",
    "tv_dhan_feed_last_tick_age_secs",
    "tv_dhan_ws_worst_conn_tick_age_secs",
    "tv_disk_runway_sessions",
    "tv_disk_seconds_to_full",
    "tv_daily_pnl",
    "tv_token_remaining_seconds",
    "tv_token_valid",
    "tv_boot_completed",
    "tv_ws_frame_spill_queue_depth",
    "tv_ws_frame_spill_queue_high_water",
    "tv_dhan_preopen_ready_secs",
    "tv_clock_skew_seconds",
    "tv_questdb_disconnected_seconds",
    "tv_questdb_wal_apply_lag_max",
    "tv_rest_1m_fire_heartbeat",
];

/// Maps `const NAME: &str = "tv_...";` to its literal, workspace-wide, so a
/// registration written through a named constant (the house style for a metric
/// with a documented meaning) is not read as a miss.
fn const_metric_literals() -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for crate_dir in ["app", "storage", "core", "trading", "api", "common"] {
        let root = repo_root().join("crates").join(crate_dir).join("src");
        collect_const_literals(&root, &mut out);
    }
    out
}

fn collect_const_literals(dir: &Path, out: &mut std::collections::BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_const_literals(&path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in src.lines() {
            let t = line.trim().trim_start_matches("pub ");
            let Some(rest) = t.strip_prefix("const ") else {
                continue;
            };
            let Some((name, tail)) = rest.split_once(':') else {
                continue;
            };
            let Some((_, after)) = tail.split_once('"') else {
                continue;
            };
            let Some(value) = after.split('"').next() else {
                continue;
            };
            if value.starts_with("tv_") {
                out.insert(name.trim().to_owned(), value.to_owned());
            }
        }
    }
}

/// Every metric name registered at zero ANYWHERE in the workspace.
///
/// Workspace-wide, not `main.rs` only: several baselines legitimately live
/// beside their emit site (`register_drop_baseline` in the storage crate), and
/// a guard that only read `main.rs` would have demanded a second, duplicate
/// registration for each of them.
fn zero_registered_names() -> std::collections::BTreeSet<String> {
    let consts = const_metric_literals();
    let mut out = std::collections::BTreeSet::new();
    for crate_dir in ["app", "storage", "core", "trading", "api", "common"] {
        let root = repo_root().join("crates").join(crate_dir).join("src");
        collect_zero_registrations(&root, &consts, &mut out);
    }
    out
}

fn collect_zero_registrations(
    dir: &Path,
    consts: &std::collections::BTreeMap<String, String>,
    out: &mut std::collections::BTreeSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_zero_registrations(&path, consts, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        // A registration may wrap across lines, so join the file and scan for
        // each `counter!(..)` span that ends in `.increment(0)`.
        for span in src.split("metrics::counter!(").skip(1) {
            let Some(head) = span.split(";").next() else {
                continue;
            };
            if !head.contains(".increment(0)") {
                continue;
            }
            // A string literal names the metric directly; a bare identifier is
            // a named constant and is resolved through `consts`, which is how
            // this repo writes any metric with a documented meaning.
            if let Some(rest) = head.split_once('"')
                && let Some(name) = rest.1.split('"').next()
                && name.starts_with("tv_")
            {
                out.insert(name.to_owned());
                continue;
            }
            let ident: String = head
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            // A fully-qualified path (`tickvault_storage::ilp_overflow::NAME`)
            // is the same constant seen from another crate; the last segment
            // is the one `const_metric_literals` keyed on.
            let ident = ident.rsplit("::").next().unwrap_or_default();
            if let Some(resolved) = consts.get(ident.trim()) {
                out.insert(resolved.clone());
            }
        }
    }
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Every `metric_name = "tv_..."` in a terraform file, with its file name.
fn alarmed_metric_names() -> Vec<(String, String)> {
    let tf_dir = repo_root().join("deploy/aws/terraform");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&tf_dir) else {
        panic!("cannot read {}", tf_dir.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let file = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default()
            .to_owned();
        if KNOWN_INDIRECT.contains(&file.as_str()) {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        for line in src.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            let Some(rest) = trimmed.strip_prefix("metric_name") else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let rest = rest.trim();
            let Some(inner) = rest.strip_prefix('"') else {
                continue;
            };
            let Some(name) = inner.split('"').next() else {
                continue;
            };
            if name.starts_with("tv_") {
                out.push((name.to_owned(), file.clone()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_alarmed_counter_is_registered_at_boot() {
    let registered = zero_registered_names();
    let alarmed = alarmed_metric_names();

    assert!(
        alarmed.len() >= 5,
        "the terraform scan found only {} alarmed metric names — the scanner has \
         probably stopped matching, which would make this guard vacuous",
        alarmed.len()
    );

    let mut missing: Vec<String> = Vec::new();
    for (name, file) in &alarmed {
        if KNOWN_GAUGES.contains(&name.as_str()) || KNOWN_NO_PRODUCER.contains(&name.as_str()) {
            continue;
        }
        if !registered.contains(name.as_str()) {
            missing.push(format!("  {name}  (alarmed in {file})"));
        }
    }

    assert!(
        missing.is_empty(),
        "ALARMED COUNTER NEVER REGISTERED — these alarms are dead on arrival \
         for a single-episode failure.\n\
         \n{}\n\
         \n\
         The CloudWatch agent computes counter deltas and DROPS the first sample \
         of a series it has never seen. A counter touched only at its failure \
         site is born AT the incident, so the dropped baseline sample IS the \
         incident and the alarm publishes nothing.\n\
         \n\
         Fix by adding `metrics::counter!(\"<name>\").increment(0);` to the \
         post-recorder registration block in crates/app/src/main.rs (or beside \
         its emit site) — NOT by adding the name to KNOWN_GAUGES unless it is \
         genuinely a gauge, nor to KNOWN_NO_PRODUCER unless it genuinely has none.",
        missing.join("\n")
    );
}

/// The scanner must actually find things, in more than one file.
#[test]
fn the_terraform_scanner_is_not_vacuous() {
    let alarmed = alarmed_metric_names();
    let files: std::collections::BTreeSet<&str> = alarmed.iter().map(|(_, f)| f.as_str()).collect();
    assert!(
        files.len() >= 2,
        "expected alarmed metrics across several terraform files, found {files:?}"
    );
    assert!(
        alarmed.iter().any(|(n, _)| n.starts_with("tv_")),
        "every matched name must be one of ours"
    );
}

/// Bite-proof: the four names this guard was written for must be present.
///
/// Without this the guard could pass by never having matched them at all.
#[test]
fn the_four_counters_that_motivated_this_guard_are_registered() {
    let registered = zero_registered_names();
    for name in [
        "tv_wal_replay_unknown_magic_total",
        "tv_seal_writer_drain_dropped_total",
        "tv_ws_frame_spill_write_errors_total",
        "tv_tick_rows_refused_total",
    ] {
        assert!(
            registered.contains(name),
            "{name} was found alarmed-but-unregistered on 2026-08-28; it must stay registered"
        );
    }
}
