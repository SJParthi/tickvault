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
    ("candle_ddl_boot.rs", 1),
    ("contract_name_repair_boot.rs", 1),
    ("daily_archive_boot.rs", 1),
    ("daily_task_marker.rs", 2),
    ("depth200_ranked_steer.rs", 1),
    ("depth20_track.rs", 1),
    ("depth_first_packet.rs", 1),
    ("depth_rebalance.rs", 7),
    ("depth_subscription_view.rs", 3),
    ("dhan_contract_universe.rs", 1),
    ("dhan_depth_universe.rs", 3),
    ("dhan_feed_stack.rs", 90),
    ("dhan_feed_stack/feed_aux_tests.rs", 1),
    ("dhan_lifecycle.rs", 2),
    ("dhan_live_crossverify.rs", 7),
    ("dhan_live_crossverify_boot.rs", 8),
    ("dhan_live_universe.rs", 18),
    ("dhan_rest_stack.rs", 4),
    ("dhan_universe.rs", 3),
    ("disk_pressure_boot.rs", 7),
    ("ensure_questdb.rs", 1),
    ("feed_scoreboard_boot.rs", 2),
    ("index_constituency_boot.rs", 1),
    ("infra.rs", 3),
    ("log_coalescer.rs", 1),
    ("main.rs", 3),
    ("observability.rs", 3),
    ("oms_wiring.rs", 1),
    ("order_observability.rs", 1),
    ("order_runtime.rs", 3),
    ("subsystem_memory.rs", 1),
    ("tf_consistency_boot.rs", 1),
    ("top_volume_snapshot.rs", 1),
    ("volume_leaderboard.rs", 2),
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
