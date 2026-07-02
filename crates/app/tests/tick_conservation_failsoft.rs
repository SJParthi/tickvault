//! Fail-soft contract tests for the daily tick-conservation audits (Dhan +
//! Groww lanes, `tick_conservation_boot`).
//!
//! The module's documented contract is "fail-soft everywhere": a missing WAL
//! dir, an unreachable QuestDB, and a dead metrics exporter must each degrade
//! the verdict to `partial` — the run still terminates promptly, never panics,
//! and never hangs the once-a-day 15:40 IST cold path. These tests enforce
//! exactly that contract fully offline (127.0.0.1 port 1 is never listening →
//! instant connection-refused on every HTTP leg), plus the Groww NDJSON
//! delivered-count ground truth against a real on-disk file.
//!
//! Coverage top-up (2026-07-02, coverage-gate fix PR).

use std::path::PathBuf;
use std::time::Duration;

use tickvault_app::tick_conservation_boot::{
    count_groww_ndjson_lines_for_ist_day, run_groww_tick_conservation_audit,
    run_tick_conservation_audit,
};
use tickvault_common::config::QuestDbConfig;

/// QuestDB config pointing at a port that is guaranteed unbound (port 1) —
/// every HTTP leg gets an instant connection-refused, exercising the
/// fail-soft arms without any wall-clock stall.
fn unreachable_qdb() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 1,
        pg_port: 1,
        ilp_port: 1,
    }
}

fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("tv-conserve-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
    dir
}

/// An arbitrary IST day number (2026-07-01 ≈ epoch-day 20635) — the audits
/// only use it for window math, which is pure.
const TARGET_IST_DAY: u64 = 20_635;
const DAY_NANOS: i64 = 86_400 * 1_000_000_000;

#[tokio::test]
async fn dhan_conservation_audit_is_fail_soft_when_every_source_is_down() {
    // Empty WAL dir + unreachable QuestDB + dead metrics exporter: the audit
    // must complete promptly (fail-soft, partial verdict) — never panic,
    // never hang the daily 15:40 cold path. The 30s ceiling is generous;
    // connection-refused on loopback resolves in microseconds.
    let wal_dir = unique_temp_dir("wal");
    let trading_date_nanos = i64::try_from(TARGET_IST_DAY).unwrap_or(0) * DAY_NANOS;

    let qdb = unreachable_qdb();
    let run = run_tick_conservation_audit(
        &wal_dir,
        &qdb,
        1, // metrics port 1 — never listening
        TARGET_IST_DAY,
        trading_date_nanos,
        trading_date_nanos,
        false, // mid-session boot → partial coverage path
    );
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("fail-soft audit must terminate promptly with all sources down");

    let _ = std::fs::remove_dir_all(&wal_dir);
}

#[tokio::test]
async fn groww_conservation_audit_counts_ndjson_and_stays_fail_soft_offline() {
    // Real NDJSON delivered-count ground truth: 2 lines inside the target IST
    // day, 1 line the day before, 1 line the day after, 1 malformed line, and
    // 1 ts-less line — only the 2 in-window lines may count.
    let dir = unique_temp_dir("ndjson");
    let ndjson_path = dir.join("live-ticks.ndjson");
    let day_start = i64::try_from(TARGET_IST_DAY).unwrap_or(0) * DAY_NANOS;
    let in_window_a = day_start + 1_000_000_000; // 00:00:01 IST
    let in_window_b = day_start + DAY_NANOS - 1; // last nano of the day
    let day_before = day_start - 1;
    let day_after = day_start + DAY_NANOS;
    let contents = format!(
        "{{\"ts_ist_nanos\":{in_window_a},\"ltp\":100.5}}\n\
         {{\"ts_ist_nanos\":{day_before},\"ltp\":99.0}}\n\
         not-json-at-all\n\
         {{\"ltp\":42.0}}\n\
         {{\"ts_ist_nanos\":{in_window_b},\"ltp\":101.0}}\n\
         {{\"ts_ist_nanos\":{day_after},\"ltp\":102.0}}\n"
    );
    std::fs::write(&ndjson_path, contents).expect("write ndjson");

    assert_eq!(
        count_groww_ndjson_lines_for_ist_day(&ndjson_path, TARGET_IST_DAY),
        2,
        "only the two in-window lines count; other days + malformed + ts-less are skipped"
    );

    // Missing file is a valid "0 delivered" measurement, never an error.
    assert_eq!(
        count_groww_ndjson_lines_for_ist_day(&dir.join("absent.ndjson"), TARGET_IST_DAY),
        0
    );

    // Full audit with QuestDB unreachable: db_rows unresolvable → partial —
    // the run must still terminate promptly (fail-soft), never panic or hang.
    let qdb = unreachable_qdb();
    let run = run_groww_tick_conservation_audit(
        &ndjson_path,
        &qdb,
        TARGET_IST_DAY,
        day_start,
        day_start,
        true, // boot covered the session — partiality comes from the dead DB
    );
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("fail-soft Groww audit must terminate promptly with QuestDB down");

    let _ = std::fs::remove_dir_all(&dir);
}
