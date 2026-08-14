//! E1: Prometheus metrics catalog — ratchet test that every metric
//! introduced by the zero-tick-loss work is wired into production code.
//!
//! This file does NOT spin up a metrics recorder (that would require a
//! heavy integration test). Instead, it scans the source tree for the
//! literal metric name strings. If someone deletes an emission site, the
//! test fails loudly with a descriptive message.
//!
//! Tests in this file exist in pairs with the feature that introduced the
//! metric — they are the second line of defence after the feature's own
//! unit tests, specifically guarding against silent metric-deletion during
//! later refactors.
//!
//! To add a new metric: add it here AND emit it from prod code AND wire it
//! into Grafana (see E2).

use std::path::Path;

/// Reads all .rs files in the given directories and concatenates them
/// into a single haystack for substring matching. Returns the concatenation.
fn read_all_sources() -> String {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common"); // APPROVED: test

    let mut haystack = String::new();
    let crates = ["common", "core", "trading", "storage", "api", "app"];
    for crate_name in &crates {
        let crate_src = workspace_root.join("crates").join(crate_name).join("src");
        if !crate_src.exists() {
            continue;
        }
        walk_and_concat(&crate_src, &mut haystack);
    }
    haystack
}

fn walk_and_concat(dir: &Path, out: &mut String) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        // 2026-08-10: was a silent `else { return; }` — an unreadable or
        // MISSING directory became "nothing to check, pass", so the guard
        // could report green while scanning zero files.
        panic!("guard corpus unreadable {:?}: {}", dir, e)
    });
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_and_concat(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            out.push_str(&content);
            out.push('\n');
        }
    }
}

/// Every entry in this table MUST appear as a literal string in the
/// workspace source tree. The description column is the operator-facing
/// meaning of the metric — it's what you'd put on the Grafana tooltip.
const REQUIRED_METRICS: &[(&str, &str)] = &[
    // --- Zero-Tick-Loss SLA (Parthiban 2026-04-20) ---
    (
        "tv_ticks_lost_total",
        "Explicit zero-tick-loss SLA counter. Incremented from TWO sites \
         (both in `ws_frame_spill`): (1) `append` when the WAL spill channel \
         is full (source=\"spill_drop_critical\"); (2) `append` when the WAL \
         writer thread is dead at the append instant \
         (source=\"spill_writer_dead\", WS-SPILL-02). The former third site — \
         `tick_persistence::write_to_dlq` (source=\"dlq_fallback\") — died \
         with the deleted tick chain (stage-2 dead-WS sweep, 2026-07-17). \
         MUST be 0 in production. Labelled by `source` and `ws_type`.",
    ),
    (
        "tv_wal_replay_recovered_total",
        "Count of frames recovered from the WAL on process startup \
         (via `ws_frame_spill::replay_all`). Pair with `tv_ticks_lost_total` \
         to show the complete zero-tick-loss picture: if spill dropped 0 and \
         replay recovered N, the guarantee held for the last N frames. \
         Incremented once per startup with the total replay count.",
    ),
    (
        "tv_wal_replay_corrupted_segments_total",
        "Count of WAL segments found corrupt during startup replay \
         (CRC32 mismatch or truncated tail). MUST be 0 in production. \
         Non-zero indicates disk corruption, abrupt kills mid-write, or \
         tampering.",
    ),
    (
        "tv_ws_frame_spill_writer_respawn_total",
        "WS-SPILL-01: the WAL frame-spill writer thread died (panic or fatal \
         return) and the supervisor respawned it with the same channel, so the \
         durable WAL floor survives and `append` never sees Disconnected. \
         Labelled by `reason` (panic|error). A sustained non-zero rate means \
         the underlying disk is failing — High-severity Telegram page.",
    ),
    (
        "tv_ws_frame_spill_write_errors_total",
        "WS-SPILL-01: a transient disk write/flush/segment-open error inside the \
         WAL writer thread. The thread alarms, reopens a fresh segment, and keeps \
         draining instead of dying. Labelled by `stage` \
         (write_record|flush|open_segment|no_segment). MUST be 0 in production.",
    ),
    // --- Session 1 (A2 dead-letter queue) ---
    // RETIRED (stage-2 dead-WS sweep, 2026-07-17): `tv_dlq_ticks_total` +
    // `tv_spill_disk_available_mb` were emitted by the deleted tick chain
    // (`tick_persistence.rs` ring→spill→DLQ + its spill-file opener) — no
    // live `ticks` writer remains (Dhan lane retired 2026-07-13, Groww live
    // feed retired 2026-07-15). The surviving DLQ/disk metrics are the SEAL
    // chain's (`tv_seal_writer_drain_total{kind=rescued_dlq}`,
    // `tv_spill_dir_free_bytes` from disk_health_watcher.rs).
    // --- Session 2 A5 (graceful unsubscribe) + A4 (pool circuit breaker) ---
    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): the 7 entries below died with the
    // machinery that emitted them — tv_ws_graceful_unsub_total +
    // tv_ws_graceful_shutdown_signalled_total (the deleted
    // connection/connection_pool graceful-unsubscribe path) and
    // tv_pool_degraded_seconds / tv_pool_degraded_alerts_total /
    // tv_pool_halts_total / tv_pool_recoveries_total (the deleted
    // pool_watchdog). tv_pool_self_halts_total (further down) retired with
    // the WS-GAP-09 watchdog Halt path in main.rs.
    // --- Session 3 S3-1 (QuestDB health poller) ---
    (
        "tv_questdb_connected",
        "Binary gauge: 1.0 if the tick writer's ILP sender is connected, \
         0.0 otherwise. Flipping to 0 does NOT imply tick loss — the ring \
         buffer + spill path absorb writes while QuestDB is down.",
    ),
    (
        "tv_questdb_disconnected_seconds",
        "How long the current QuestDB outage has lasted (0 when connected).",
    ),
    (
        "tv_questdb_reconnects_total",
        "Total successful QuestDB reconnects since process startup.",
    ),
    (
        "tv_questdb_disconnect_events_total",
        "Total Connected → Disconnected transitions observed. Each outage \
         increments this once.",
    ),
    // --- Session 3 S3-2 (backfill worker wiring) — REMOVED 2026-04-17.
    //     Parthiban directive: the BackfillWorker is permanently DELETED.
    //     `ticks` table contains live WebSocket data only; historical
    //     candles live in `historical_candles`. See
    //     .claude/rules/project/live-feed-purity.md for the hard ban.
    //     The 7 `tv_backfill_*` metrics and `tv_backfill_ticks_persisted_total`
    //     have been de-registered along with the module.
    //     (Metric names intentionally left dangling in this catalog so
    //     Grafana queries referring to them raise a missing-metric error
    //     loudly rather than silently returning 0.)
    // --- Session 4 S4-T1a (pool self halts) ---
    // tv_pool_self_halts_total RETIRED (PR-C2, 2026-07-13) with the
    // WS-GAP-09 pool-watchdog Halt path — deleted with the Dhan live-WS
    // lane (see the block note above).
    // --- Session 4 S4-T1f (synth ticks forwarded to broadcast) ---
    // tv_backfill_ticks_forwarded_total + tv_backfill_ticks_persisted_total
    // removed with the BackfillWorker deletion on 2026-04-17. See above.
    // --- Session 4 S4-T4 (existing subsystem metrics catalogued for lockdown) ---
    // tv_order_update_ws_active REMOVED from the catalog 2026-07-14 (Dhan
    // noise lock fix round M4): the order-update WS spawn is retired
    // (scope-lock §A.1), so the gauge write site in the dormant
    // order_update_connection.rs module has zero reachable callers. The
    // EMF allowlist + dashboard panel + inactive alarm are removed in
    // lockstep; re-cataloguing requires the live-trading re-wire quote.
    (
        "tv_order_update_reconnections_total",
        "S4-T4: Reconnection attempt counter for the order update WebSocket.",
    ),
    (
        "tv_order_update_messages_total",
        "S4-T4: Total order update messages received (any type).",
    ),
    // `tv_valkey_ops_total` + `tv_valkey_errors_total` removed in
    // #O4 (2026-05-24) — Valkey deleted from the runtime.
];

// Note: `tv_sandbox_gate_blocks_total` was intentionally deferred from E1.
// Adding a metrics counter to `common::config` would require pulling the
// `metrics` crate into the common crate, which is currently
// framework-free. The ERROR log on sandbox-gate-block already fires a
// Telegram alert, so operator visibility exists without the counter.
// Revisit if we ever need a time-series of block attempts.

/// E1: Every metric in REQUIRED_METRICS MUST appear as a literal string
/// in the workspace source. If this test fails, you either:
/// - Deleted the emission site (regression — restore it)
/// - Renamed the metric (update REQUIRED_METRICS + Grafana dashboards)
/// - Added a new metric without registering it here (add it now)
#[test]
fn metrics_catalog_every_required_metric_is_emitted() {
    let haystack = read_all_sources();

    let mut missing = Vec::new();
    for (name, description) in REQUIRED_METRICS {
        // Search for the literal name inside a Rust string (quoted).
        // Any metric! macro use embeds the name as a quoted literal.
        let quoted = format!("\"{name}\"");
        if !haystack.contains(&quoted) {
            missing.push((*name, *description));
        }
    }

    assert!(
        missing.is_empty(),
        "E1: {} required metric(s) are NOT emitted anywhere in production source:\n{}",
        missing.len(),
        missing
            .iter()
            .map(|(n, d)| format!("  - {n}: {d}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Metrics retired with the Dhan live-WS lane (PR-C2 2026-07-13 / stage-2
/// sweep 2026-07-17) whose PRODUCERS return with the 16-connection revival
/// authorized by the operator quotes of 2026-08-09
/// (websocket-connection-scope-lock.md "2026-08-09 — DHAN LIVE MAIN-FEED WS
/// REVIVAL AUTHORIZED" + "SAME DAY, SECOND QUOTE"), operator-approved
/// 2026-08-11.
///
/// Why this list exists (the silent-rot this closes): every other test in
/// this file checks CATALOG → SOURCE ("is each catalogued metric emitted?").
/// Nothing checked SOURCE → CATALOG. So when these producers resurrect, the
/// catalog does not fail — it just silently stops describing them, and the
/// metrics ship undocumented, unpanelled and unalarmed. That is a coverage
/// hole that opens quietly, which is worse than a loud break.
///
/// These are deliberately NOT re-added to `REQUIRED_METRICS` yet: verified
/// 2026-08-11, all five have ZERO emission sites in `crates/*/src` (the
/// revival's connection/pool code is still being written), so cataloguing
/// them today would fail `metrics_catalog_every_required_metric_is_emitted`
/// and block the very work this re-blessing exists to unblock.
const REVIVAL_PENDING_METRICS: &[&str] = &[
    "tv_ws_graceful_unsub_total",
    "tv_ws_graceful_shutdown_signalled_total",
    "tv_pool_self_halts_total",
    "tv_pool_degraded_seconds",
    "tv_pool_halts_total",
];

/// RE-BLESSED 2026-08-11 — the retirement note above is inverted into an
/// ACTIVE gate instead of passive prose.
///
/// Each metric is in exactly one of two legal states: **not emitted
/// anywhere** (still retired — fine), or **emitted AND catalogued** (revived
/// properly — fine). The illegal third state is *emitted but uncatalogued*,
/// which is precisely how a resurrected producer would slip in undocumented.
///
/// So the moment an agent wires up e.g. `tv_pool_halts_total`, this test
/// fails and tells them to move the name from `REVIVAL_PENDING_METRICS` into
/// `REQUIRED_METRICS` with a real description. The protection that used to
/// say "these are deleted" now says "these may return, but only with their
/// documentation" — same guard, new direction.
#[test]
fn revival_pending_metrics_are_catalogued_the_moment_they_are_emitted() {
    let haystack = read_all_sources();
    let catalogued: std::collections::HashSet<&str> =
        REQUIRED_METRICS.iter().map(|(n, _)| *n).collect();

    let mut undocumented = Vec::new();
    for name in REVIVAL_PENDING_METRICS {
        let emitted = haystack.contains(&format!("\"{name}\""));
        if emitted && !catalogued.contains(name) {
            undocumented.push(*name);
        }
    }

    assert!(
        undocumented.is_empty(),
        "{} Dhan-lane metric(s) are emitted in production source but absent \
         from REQUIRED_METRICS:\n{}\n\nThese retired with the Dhan live-WS \
         lane and are returning with the 2026-08-09-authorized revival. Move \
         each name out of REVIVAL_PENDING_METRICS and into REQUIRED_METRICS \
         with a real operator-readable description — and add its dashboard \
         panel + alert rule in the same PR (operator-charter-forever.md §C: \
         a counter with no panel and no alert is not monitoring). Do NOT \
         silence this by deleting the name from REVIVAL_PENDING_METRICS.",
        undocumented.len(),
        undocumented
            .iter()
            .map(|n| format!("  - {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Guards the guard: a name may not sit in BOTH lists (that would make the
/// anti-rot check above vacuous for that metric).
#[test]
fn revival_pending_metrics_are_disjoint_from_required() {
    let catalogued: std::collections::HashSet<&str> =
        REQUIRED_METRICS.iter().map(|(n, _)| *n).collect();
    for name in REVIVAL_PENDING_METRICS {
        assert!(
            !catalogued.contains(name),
            "{name} is in BOTH REQUIRED_METRICS and REVIVAL_PENDING_METRICS \
             — once catalogued it must be REMOVED from the pending list, or \
             the emitted-but-uncatalogued check silently stops covering it."
        );
    }
}

/// E1: Sanity check — REQUIRED_METRICS itself must not contain duplicates.
/// A duplicate would mean two different descriptions point at the same
/// metric name, which would silently confuse operators.
#[test]
fn metrics_catalog_no_duplicate_names() {
    let mut seen = std::collections::HashSet::new();
    for (name, _) in REQUIRED_METRICS {
        assert!(
            seen.insert(*name),
            "E1: duplicate metric name in REQUIRED_METRICS: {name}"
        );
    }
}

/// E1: Every metric name must follow the Prometheus convention
/// (lowercase, underscores, `_total` suffix for counters).
#[test]
fn metrics_catalog_names_follow_prometheus_conventions() {
    for (name, _) in REQUIRED_METRICS {
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "E1: metric name must be snake_case ASCII: {name}"
        );
        assert!(
            name.starts_with("tv_"),
            "E1: all dlt metrics must carry the tv_ prefix: {name}"
        );
    }
}
