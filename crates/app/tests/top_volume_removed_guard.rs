//! Pins the 2026-10-21 (EVENING) operator removal (plan item 62): nothing
//! writes `candles_3s`, `candles_5s` or any `top_volume_*` table, and the
//! ranking that only fed `top_volume` is gone.
//!
//! Operator, verbatim (Quote 34, `daily-universe-scope-expansion-2026-05-27.md`):
//! *"see clealry note to delete and permanently remove and drop candles 3
//! seconds and candles 5 seconds dude and even entire top volumes entire
//! tables also dude okay?"*. The scope lock's section "2026-10-21 (EVENING) —
//! CANDLES_3S, CANDLES_5S AND TOP_VOLUME ARE REMOVED" REJECTs re-adding a 3 s
//! or 5 s timeframe, or any `top_volume_*` table or writer, without a fresh
//! dated quote. This file is that REJECT row's teeth.
//!
//! It replaces `top_volume_stock_only_guard.rs` (the 2026-09-18 FOURTH
//! "stock options only" pin), whose subject, the volume board, no longer
//! exists.
//!
//! # What stays, on purpose
//!
//! The six tables are DROPPED only after every partition on the box is copied
//! to S3 and verified (Quote 34). Until then the retention sweep must keep
//! archiving them, so `candles_3s` / `candles_5s` sit on an archive-only list
//! and the five `top_volume*` names stay on the HOUR sweep list. The last test
//! pins that half, so a tidy-up cannot quietly stop their S3 copy.

use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/") // APPROVED: test
        .to_path_buf()
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(crates_dir().join(rel)).unwrap_or_default()
}

/// Every production `.rs` file under `crates/<crate>/src`, with test modules
/// cut off at the first `#[cfg(test)]` so prose in a test cannot satisfy or
/// defeat a scan.
fn production_sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    for krate in ["app", "storage", "trading", "common", "core", "api"] {
        walk(&crates_dir().join(krate).join("src"), &mut files);
    }
    files
        .into_iter()
        .map(|f| {
            let src = std::fs::read_to_string(&f).unwrap_or_default();
            let prod = src
                .split_once("\n#[cfg(test)]")
                .map_or(src.as_str(), |(before, _)| before)
                .to_string();
            (f, prod)
        })
        .collect()
}

/// Lines of `src` with every `//` comment removed.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|line| line.find("//").map_or(line, |i| &line[..i]))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_top_volume_modules_are_deleted() {
    for rel in [
        "storage/src/top_volume_rank_persistence.rs",
        "app/src/top_volume_sweep.rs",
        "app/src/top_volume_snapshot.rs",
        "app/src/volume_leaderboard.rs",
        "app/src/dhan_feed_stack/top_volume_threads.rs",
    ] {
        assert!(
            !crates_dir().join(rel).exists(),
            "{rel} was removed with top_volume (Quote 34); re-adding it needs a \
             fresh dated operator quote recorded in the scope lock first"
        );
    }
}

#[test]
fn no_production_code_writes_or_ensures_a_top_volume_table() {
    let sources = production_sources();
    assert!(sources.len() > 100, "the source walk found almost nothing");
    for (file, prod) in &sources {
        let code = code_only(prod);
        for banned in [
            "top_volume_rank_persistence::",
            "TopVolumeRankWriter",
            "ensure_top_volume_tables",
            "spawn_supervised_top_volume_spill_replay",
            "with_top_volume_writer(",
            "VolumeLeaderboard",
        ] {
            assert!(
                !code.contains(banned),
                "{} names `{banned}`: a top_volume writer, ensure or ranking is back. \
                 Quote 34 removed them; a fresh dated quote must come first.",
                file.display()
            );
        }
    }
}

#[test]
fn the_fold_has_no_three_or_five_second_frame() {
    let tf = read("trading/src/candles/tf_index.rs");
    let tf_prod = tf
        .split_once("\n#[cfg(test)]")
        .map_or(tf.as_str(), |(before, _)| before);
    let code = code_only(tf_prod);
    assert!(
        code.contains("pub const TF_COUNT: usize = 8;"),
        "TF_COUNT must stay 8 (1s 1m 3m 5m 10m 15m 30m 60m)"
    );
    for banned in ["S3 =", "S5 =", "\"candles_3s\"", "\"candles_5s\""] {
        assert!(
            !code.contains(banned),
            "tf_index.rs carries `{banned}`: a 3 s or 5 s frame is back"
        );
    }
}

#[test]
fn the_retired_tables_stay_on_the_archive_sweep_until_copied() {
    let shadow = code_only(&read("storage/src/shadow_persistence.rs"));
    assert!(
        shadow.contains(
            "pub const ARCHIVE_ONLY_CANDLE_TABLES: [&str; 2] = [\"candles_3s\", \"candles_5s\"];"
        ),
        "candles_3s / candles_5s must stay archive-only (Quote 34: no drop before \
         the verified S3 copy)"
    );
    let manager = code_only(&read("storage/src/partition_manager.rs"));
    for name in [
        "\"top_volume\"",
        "\"top_volume_1s\"",
        "\"top_volume_3s\"",
        "\"top_volume_5s\"",
        "\"top_volume_1m\"",
    ] {
        assert!(
            manager.contains(name),
            "{name} left the HOUR sweep list: its partitions on the box would stop \
             being archived to S3 before the Quote 35 wipe"
        );
    }
    let archive = code_only(&read("storage/src/partition_archive.rs"));
    assert!(
        archive.contains("ARCHIVE_ONLY_CANDLE_TABLES"),
        "the archive pass must still visit the archive-only candle tables"
    );
}

#[test]
fn the_scans_bite() {
    // A banned name inside a comment is ignored, in code it is caught.
    assert!(!code_only("// TopVolumeRankWriter").contains("TopVolumeRankWriter"));
    assert!(code_only("let w = TopVolumeRankWriter::new();").contains("TopVolumeRankWriter"));
}
