//! APP SELF-SOURCE-SCAN BUDGET (2026-10-12, deep audit PROOF-13).
//!
//! Many app tests read the app's own source with `include_str!("x.rs")` and
//! assert that some text is present. Those guards are legitimate tripwires
//! (lock files pin phrases, wiring must exist), but a text guard proves that
//! code EXISTS, not that it WORKS, and nothing stopped new text guards from
//! standing in for behaviour tests: `dhan_feed_stack.rs` alone carries about
//! ninety of them.
//!
//! `app_self_source_scans_may_only_shrink` counts, per file under
//! `crates/app/src`, the `include_str!(...)` calls whose argument names a
//! `.rs` file, and fails when a file's count passes its budget (or a file not
//! listed gains one). A new self-scan must remove one elsewhere, or raise the
//! budget in a reviewed diff whose comment names the behaviour test that
//! pairs with it. Behaviour tests are preferred.
//!
//! HONEST LIMITS: converting the existing scans into behaviour tests is not
//! done here. Scans that read source through `std::fs::read_to_string` (most
//! files under `crates/app/tests/`) are not counted; this budget covers the
//! inline test modules in `src`.

use std::path::{Path, PathBuf};

/// `(file under crates/app/src, include_str! of a .rs file)`, measured
/// 2026-10-12. Sorted. Counts may only go DOWN; lower a row when a scan is
/// replaced by a behaviour test.
const BUDGET: &[(&str, usize)] = &[
    ("api_token_rotation.rs", 1),
    ("bin/smoke_test.rs", 1),
    ("boot_helpers.rs", 1),
    // 2026-10-26 (audit J3): +5 wiring scans, one per audit consumer, pinning
    // that each starts the audit DDL re-run when its own ensure is refused (a
    // refusal at the one moment a consumer starts cannot be staged in a unit
    // test). Paired behaviour tests: `audit_tables_ensure_reads_not_keyed_when_unreachable`,
    // `test_spawn_audit_ensure_until_keyed_once_starts_once_per_process`.
    ("candle_ddl_boot.rs", 6),
    ("contract_name_repair_boot.rs", 1),
    ("daily_archive_boot.rs", 1),
    ("daily_task_marker.rs", 2),
    ("depth200_ranked_steer.rs", 1),
    ("depth20_track.rs", 1),
    ("depth_first_packet.rs", 1),
    ("depth_rebalance.rs", 7),
    ("depth_subscription_view.rs", 3),
    // 2026-10-21 (owner: depth switched off): 2 wiring scans (the attach starts
    // both depth halves done, main stores the switch); paired behaviour test
    // `test_set_depth_sockets_enabled_round_trips`.
    ("depth_switch.rs", 2),
    // 1 -> 2 (2026-10-28, daily expiry check): pins that the attach builds its
    // candle names from every option row, not the capped ranking table. Paired
    // behaviour test: `test_option_labels_from_keys_exactly_as_a_candle_row_probes`.
    ("dhan_contract_universe.rs", 2),
    ("dhan_depth_universe.rs", 3),
    // 2026-10-14 (ITEM 57): 90 -> 88; `contract_attach_tests` (3 scans) moved
    // to its own file, one scan added by `test_live_tick_flush_leaves_depth_to_its_own_cadence`,
    // which pairs it with a behaviour half on the same ingest.
    // 2026-10-22 (plan item 62): 88 -> 81; seven top_volume source scans went
    // with the code they scanned.
    // 2026-10-26 (audit K3): +1 wiring scan, the tick-gap verdict counters are
    // counted at ingest, seeded at 0 and never EMF-shipped. Paired behaviour
    // test: `tick_verdict_counters_move_on_a_backward_ltt_and_a_volume_jump`.
    ("dhan_feed_stack.rs", 82),
    ("dhan_feed_stack/contract_attach_tests.rs", 3),
    ("dhan_feed_stack/feed_aux_tests.rs", 1),
    // 2026-10-26 (audit PR M / M4): 1 wiring scan, every dial batch republishes
    // the reserved-connections gauge (the widen dials long after bring-up and
    // cannot be staged in a unit test). Paired behaviour test:
    // `test_regression_reserved_connections_gauge_follows_the_widen`.
    ("dhan_feed_stack/reserved_connections_tests.rs", 1),
    // 2026-10-28 (DT-small seed queue): 1 wiring scan, the drain's idle arm is
    // gated on `idle_work_pending()` alone, never the seed latch (the select!
    // arm guard cannot be driven from a unit test). Paired behaviour half in
    // the same test, `test_idle_arm_keeps_seeding_queued_batches_after_the_seed_channel_closes`:
    // idle steps alone seed every queued batch after the sender is dropped.
    ("dhan_feed_stack/seed_step_tests.rs", 1),
    ("dhan_live_crossverify.rs", 7),
    ("dhan_live_crossverify_boot.rs", 8),
    ("dhan_live_universe.rs", 18),
    ("dhan_rest_stack.rs", 4),
    // 3 -> 4 (2026-10-28, Quote 44 review): `build_once` cannot run without
    // the network, so the held-counter emit after the sweep is pinned by
    // source. Paired behaviour test for the count itself:
    // `test_sweep_stale_artifacts_in_keeps_an_old_artifact_with_no_verified_copy`.
    ("dhan_universe.rs", 4),
    ("disk_pressure_boot.rs", 7),
    ("ensure_questdb.rs", 1),
    // 2026-10-21 (plan ITEM 60): 3 wiring scans (lane budget, main restart arm,
    // hold supervisor); paired behaviour tests `evening_step_*`,
    // `test_catchup_budget_for_only_a_fresh_evening_pending_flag`,
    // `test_request_self_restart_resolves_a_later_self_restart_requested`.
    ("evening_drain.rs", 3),
    // 2026-10-28 (daily expiry check): 5 wiring scans (main spawns the 15:50
    // check, it runs on its own task never the frame drain, the attach refuses
    // before it names or dials, every error carries its code). Paired behaviour
    // tests: `test_check_selected_expiries_refuses_off_expiry_expired_unknown_and_the_stub`,
    // `test_classify_candle_contracts_all_current_is_clean`,
    // `test_record_subscribe_expiry_scope_and_subscribe_refused_for_day`.
    // 5 -> 7 (2026-10-28, expiry check review): +1 the attach feeds every
    // per-id table from `preferred_option_rows` (the attach needs the network);
    // paired behaviour tests `test_preferred_option_rows_keeps_one_row_per_key_as_by_key_does`,
    // `test_preferred_option_rows_maps_a_reused_id_to_the_admissible_underlying`.
    // +1 the real 15:50 loop keeps the simulated loop's step order (the loop
    // sleeps until 15:50 and cannot run in a test); paired behaviour test
    // `test_secs_until_after_checked_day_composed_in_the_daily_loop_checks_every_trading_day_once`.
    ("expiry_scope_check_tests.rs", 7),
    ("feed_scoreboard_boot.rs", 2),
    ("index_constituency_boot.rs", 1),
    ("infra.rs", 3),
    // 2026-10-28 (Quote 44 review, finding 3): 1 doc scan, the module header
    // must name the real proof-to-sweep bound (`SNAPSHOT_PASS_TIMEOUT_SECS`,
    // 1,800 s), not "seconds". Paired behaviour half in the same test,
    // `test_run_bounded_snapshot_pass_bounds_the_gap_between_proof_and_sweep`:
    // a pass past the bound returns no summary and sweeps nothing.
    ("instrument_snapshot_upload.rs", 1),
    ("log_coalescer.rs", 1),
    ("main.rs", 3),
    ("observability.rs", 3),
    ("oms_wiring.rs", 1),
    ("order_observability.rs", 1),
    ("order_runtime.rs", 3),
    ("subsystem_memory.rs", 1),
    ("tf_consistency_boot.rs", 1),
    // 2026-10-26 (audit K4): 1 wiring scan, main spawns the daily tick
    // conservation check and reads the last WAL receipt after claiming the WAL
    // and before its own writer starts. Paired behaviour tests: the
    // `test_classify_*` verdict tests and `test_process_down_gap_row_*`.
    ("tick_conservation.rs", 1),
    ("ws_audit_consumer.rs", 2),
    ("ws_connection_rollup.rs", 1),
];

fn app_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// Number of `include_str!(...)` calls in `src` whose (balanced) argument
/// contains a `.rs"` string, i.e. that load a Rust source file.
fn self_scan_count(src: &str) -> usize {
    const M: &str = "include_str!(";
    let mut n = 0;
    let mut from = 0;
    while let Some(rel) = src[from..].find(M) {
        let start = from + rel + M.len();
        let mut depth = 1usize;
        let mut end = start;
        for (i, c) in src[start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > start && src[start..end].contains(".rs\"") {
            n += 1;
        }
        from = start;
    }
    n
}

#[test]
fn app_self_source_scans_may_only_shrink() {
    let names: Vec<&str> = BUDGET.iter().map(|r| r.0).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted, "BUDGET must be sorted and unique");

    let root = app_src();
    let mut files = Vec::new();
    collect_rs(&root, &mut files);
    let mut measured: Vec<(String, usize)> = files
        .iter()
        .filter_map(|f| {
            let rel = f
                .strip_prefix(&root)
                .unwrap_or(f)
                .display()
                .to_string()
                .replace('\\', "/");
            let n = self_scan_count(&std::fs::read_to_string(f).unwrap_or_default());
            (n > 0).then_some((rel, n))
        })
        .collect();
    measured.sort();
    let total: usize = measured.iter().map(|r| r.1).sum();
    let table: String = measured
        .iter()
        .map(|(f, n)| format!("    (\"{f}\", {n}),\n"))
        .collect();
    assert!(
        total > 50,
        "only {total} self-scans found; the scan is broken\nMeasured now:\n{table}"
    );

    let over: Vec<String> = measured
        .iter()
        .filter(|(f, n)| *n > BUDGET.iter().find(|(b, _)| b == f).map_or(0, |(_, m)| *m))
        .map(|(f, n)| format!("{f}: {n}"))
        .collect();
    assert!(
        over.is_empty(),
        "self-source scans grew past their budget: {over:?}. Prefer a behaviour test; \
         if a text guard is truly needed, remove one elsewhere or raise the budget with \
         a comment naming its paired behaviour test.\nMeasured now:\n{table}"
    );
    let stale: Vec<String> = BUDGET
        .iter()
        .filter(|(b, m)| measured.iter().find(|(f, _)| f == b).map_or(0, |(_, n)| *n) < *m)
        .map(|(b, m)| format!("{b}: budget {m}"))
        .collect();
    assert!(
        stale.is_empty(),
        "budget rows above the measured count (lower them in this PR): {stale:?}\n\
         Measured now:\n{table}"
    );
}

#[test]
fn self_scan_counter_bites() {
    let src = concat!(
        "let a = include_str!(\"x.rs\");\n",
        "let b = include_str!(\n    \"../y.rs\"\n);\n",
        "let c = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/z.rs\"));\n",
        "let d = include_str!(\"data.json\");\n",
    );
    assert_eq!(self_scan_count(src), 3);
}
