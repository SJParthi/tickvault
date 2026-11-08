//! The hour path (plan item PR AA, step 3; owner Quote 51, 2026-11-07).
//!
//! # What it decides
//!
//! Which tables a pass may archive hour by hour instead of day by day, and
//! which spill folder can still post rows into each of them.
//!
//! - `market_depth`: on the hour path in every pass, as before
//!   ([`super::ARRIVAL_STAMPED_HOUR_TABLES`]).
//! - `market_depth_book`, `tick_quotes`, `feed_aux_packets`: only while the
//!   pass carries disk pressure ([`PRESSURE_HOUR_TABLES`]), switched off as a
//!   group by `[partition_retention] pressure_hour_tables_enabled = false`.
//!   Their `ts` is the frame's ARRIVAL instant in IST, like `market_depth`:
//!   the depth writer stamps `received_at + IST offset` on both depth tables,
//!   `tick_quotes` stamps the WAL receipt plus the IST offset, and
//!   `feed_aux_packets` stamps the receipt (or the frame sequence, which is
//!   the wall clock at receipt) plus the IST offset.
//! - `ticks`: only while the pass carries pressure AND
//!   `ticks_hour_path_enabled = true` (ships `false`). Its `ts` is the
//!   exchange last-trade time, so a live tick can land in an earlier hour of
//!   the same day; the owner decides before it is switched on.
//!
//! Nothing else ever reaches the hour path: [`HOUR_PATH_TABLES`] is a closed
//! list, checked at compile time against candles, SEBI and audit tables, and
//! [`hour_window_for`] answers `None` for every other name.
//!
//! Every hour-path partition is judged by its END (at least the window, 4
//! hours or more, after the hour closed; `build_closed_hour_list_sql`) and
//! goes through the WAL snapshot gate ([`uses_snapshot_gate`]), which holds
//! every writer that can post into an old hour for the whole copy and drop.
//!
//! # Spill files
//!
//! A table is held off the hour path while a file in ITS OWN spill folder
//! still holds rows a replay could post back ([`super::spill_dirs_for_table`]):
//! the depth folder for the two depth tables, the tick folder for the three
//! tick-writer tables. Only `market_depth` may be forced onto the hour path
//! under pressure with a pending spill file (today's behaviour since
//! 2026-09-01), behind `market_depth_force_with_spill_pending`; the other
//! tables wait for their folder to drain. Whether QuestDB re-creates a
//! dropped WAL partition on a later insert is checked by the `#[ignore]`d
//! probe `tests/questdb_dropped_partition_probe.rs`.
//!
//! # Complexity
//!
//! Pure and O(1): a few comparisons against lists of at most five names. The
//! compile-time check runs once, at build time.

use tickvault_common::config::PartitionRetentionConfig;

use super::{ARRIVAL_STAMPED_HOUR_TABLES, DEPTH_TABLES, MIN_HOT_HOURS};
use crate::partition_manager::{HOUR_PARTITIONED_TABLES, RETENTION_EXEMPT_TABLES};

/// Tables on the hour path only while a pass carries disk pressure. Their
/// `ts` is the arrival instant in IST (see the module doc).
pub(crate) const PRESSURE_HOUR_TABLES: &[&str] =
    &["market_depth_book", "tick_quotes", "feed_aux_packets"];

/// `ticks`: on the hour path only under pressure and behind
/// `ticks_hour_path_enabled` (ships `false`).
pub(crate) const TICKS_TABLE: &str = "ticks";

/// Every table the hour path can ever name: the five tables Quote 51 lists.
/// A closed list; [`hour_path_list_is_safe`] checks it at compile time.
pub(crate) const HOUR_PATH_TABLES: &[&str] = &[
    "market_depth",
    "market_depth_book",
    "tick_quotes",
    "feed_aux_packets",
    "ticks",
];

/// Tables whose rows the TICK writer writes, so whose pending rows sit in
/// the tick spill folder.
pub(crate) const TICK_SPILL_TABLES: &[&str] = &["ticks", "tick_quotes", "feed_aux_packets"];

const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn starts_with(haystack: &[u8], prefix: &[u8]) -> bool {
    if prefix.len() > haystack.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if haystack[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    let mut start = 0;
    while start + needle.len() <= haystack.len() {
        let mut i = 0;
        let mut same = true;
        while i < needle.len() {
            if haystack[start + i] != needle[i] {
                same = false;
                break;
            }
            i += 1;
        }
        if same {
            return true;
        }
        start += 1;
    }
    false
}

const fn listed(list: &[&str], name: &str) -> bool {
    let mut i = 0;
    while i < list.len() {
        if bytes_eq(list[i].as_bytes(), name.as_bytes()) {
            return true;
        }
        i += 1;
    }
    false
}

/// Whether the hour-path lists are safe: every hour-path table is an
/// HOUR-partitioned market-data table, none is a candle, SEBI or audit
/// table or on the retention-exempt list, and every table the selection can
/// name is on [`HOUR_PATH_TABLES`]. Checked at compile time below.
const fn hour_path_list_is_safe() -> bool {
    let mut i = 0;
    while i < HOUR_PATH_TABLES.len() {
        let name = HOUR_PATH_TABLES[i].as_bytes();
        if !listed(HOUR_PARTITIONED_TABLES, HOUR_PATH_TABLES[i])
            || listed(RETENTION_EXEMPT_TABLES, HOUR_PATH_TABLES[i])
            || starts_with(name, b"candles")
            || contains(name, b"audit")
            || contains(name, b"lifecycle")
            || contains(name, b"constituency")
        {
            return false;
        }
        i += 1;
    }
    let mut j = 0;
    while j < ARRIVAL_STAMPED_HOUR_TABLES.len() {
        if !listed(HOUR_PATH_TABLES, ARRIVAL_STAMPED_HOUR_TABLES[j]) {
            return false;
        }
        j += 1;
    }
    let mut k = 0;
    while k < PRESSURE_HOUR_TABLES.len() {
        if !listed(HOUR_PATH_TABLES, PRESSURE_HOUR_TABLES[k]) {
            return false;
        }
        k += 1;
    }
    listed(HOUR_PATH_TABLES, TICKS_TABLE)
}

const _: () = assert!(
    hour_path_list_is_safe(),
    "the hour path may name only the five Quote 51 market-data tables, never a candle, SEBI or \
     audit table"
);

/// The hour floor for a configured window: `0` is OFF (`None`), anything
/// else is clamped up to [`MIN_HOT_HOURS`]. Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_hour_floor_end_based_at_least_4h
pub(crate) fn hot_hour_floor(configured_hours: u32) -> Option<u32> {
    (configured_hours != 0).then(|| configured_hours.max(MIN_HOT_HOURS))
}

/// The hour window `table` gets in this pass, or `None` for its day window.
///
/// `market_depth` in every pass; the [`PRESSURE_HOUR_TABLES`] only while the
/// pass carries pressure and `pressure_hour_tables_enabled`; `ticks` only
/// under pressure and `ticks_hour_path_enabled`. Every other table, and a
/// configured `depth_hot_hours = 0`, gets `None`. Spill files are judged
/// separately ([`super::hour_window_decision`]). Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_hour_window_never_candles_sebi_audit
pub(crate) fn hour_window_for(table: &str, cfg: &PartitionRetentionConfig) -> Option<u32> {
    if !HOUR_PATH_TABLES.contains(&table) {
        return None;
    }
    let hours = hot_hour_floor(cfg.depth_hot_hours)?;
    if ARRIVAL_STAMPED_HOUR_TABLES.contains(&table) {
        return Some(hours);
    }
    if !cfg.under_disk_pressure {
        return None;
    }
    let switched_on = if table == TICKS_TABLE {
        cfg.ticks_hour_path_enabled
    } else {
        cfg.pressure_hour_tables_enabled && PRESSURE_HOUR_TABLES.contains(&table)
    };
    switched_on.then_some(hours)
}

/// Whether `table` may be forced onto the hour path under pressure while a
/// file in its spill folder is still pending: `market_depth` only, behind
/// `market_depth_force_with_spill_pending` (today's behaviour). Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_new_tables_not_forced_with_own_spill_pending
pub(crate) fn may_force_with_spill_pending(table: &str, cfg: &PartitionRetentionConfig) -> bool {
    table == "market_depth" && cfg.market_depth_force_with_spill_pending
}

/// Whether a partition of `table` goes through the WAL snapshot gate: the
/// depth tables always (step 2), and every table on the hour path in this
/// pass. Every other table keeps the exact `writerTxn == sequencerTxn` gate.
/// Pure, O(tables on the hour path), at most five.
#[must_use]
// TEST-EXEMPT: covered by test_snapshot_gate_follows_the_hour_path
pub(crate) fn uses_snapshot_gate(table: &str, hour_path_this_run: &[&str]) -> bool {
    DEPTH_TABLES.contains(&table) || hour_path_this_run.contains(&table)
}

#[cfg(test)]
mod tests {
    use super::super::{HourWindowDecision, hour_window_decision, spill_dirs_for_table};
    use super::*;
    use crate::partition_manager::build_closed_hour_list_sql;

    fn cfg(
        pressure: bool,
        hours: u32,
        group: bool,
        ticks: bool,
        force_depth: bool,
    ) -> PartitionRetentionConfig {
        PartitionRetentionConfig {
            under_disk_pressure: pressure,
            depth_hot_hours: hours,
            pressure_hour_tables_enabled: group,
            ticks_hour_path_enabled: ticks,
            market_depth_force_with_spill_pending: force_depth,
            ..PartitionRetentionConfig::default()
        }
    }

    /// Every flag combination and a spread of windows.
    fn every_cfg() -> Vec<PartitionRetentionConfig> {
        let mut all = Vec::new();
        for pressure in [false, true] {
            for hours in [0, 1, 4, 6, 24, u32::MAX] {
                for group in [false, true] {
                    for ticks in [false, true] {
                        for force in [false, true] {
                            all.push(cfg(pressure, hours, group, ticks, force));
                        }
                    }
                }
            }
        }
        all
    }

    #[test]
    fn test_hour_window_never_candles_sebi_audit() {
        // Every table the archiver sweeps, under every configuration: only
        // the five Quote 51 tables can ever get an hour window.
        for table in super::super::swept_tables() {
            let named = HOUR_PATH_TABLES.contains(&table);
            for c in every_cfg() {
                if !named {
                    assert_eq!(
                        hour_window_for(table, &c),
                        None,
                        "{table} reached the hour path"
                    );
                }
            }
            if table.starts_with("candles")
                || table.contains("audit")
                || table.contains("lifecycle")
                || table.contains("constituency")
                || RETENTION_EXEMPT_TABLES.contains(&table)
            {
                assert!(!named, "{table} is on the hour-path list");
            }
        }
        let mut names = HOUR_PATH_TABLES.to_vec();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "feed_aux_packets",
                "market_depth",
                "market_depth_book",
                "tick_quotes",
                "ticks"
            ],
            "the hour path names exactly the five tables of Quote 51"
        );
        assert!(hour_path_list_is_safe());
    }

    #[test]
    fn test_hour_path_list_check_bites_on_a_bad_name() {
        // The compile-time helpers really refuse what the check relies on.
        assert!(starts_with(b"candles_1m", b"candles"));
        assert!(contains(b"order_audit", b"audit"));
        assert!(contains(b"instrument_lifecycle_audit", b"lifecycle"));
        assert!(!contains(b"tick_quotes", b"audit"));
        assert!(listed(HOUR_PARTITIONED_TABLES, "tick_quotes"));
        assert!(!listed(HOUR_PARTITIONED_TABLES, "candles_1m"));
        assert!(listed(RETENTION_EXEMPT_TABLES, "instrument_lifecycle"));
    }

    #[test]
    fn test_hour_floor_end_based_at_least_4h() {
        // The SQL is the real rule: parse its window back out and check, for
        // every hour of two days and every minute "now", that a selected
        // partition ENDED at least four hours before now.
        for configured in [1, 3, 4, 5, 6] {
            let hours = hot_hour_floor(configured).expect("non-zero is on");
            assert!(hours >= MIN_HOT_HOURS && MIN_HOT_HOURS >= 4);
            let sql = build_closed_hour_list_sql("tick_quotes", hours);
            let marker = "dateadd('h', -";
            let at = sql.find(marker).expect("hour cutoff") + marker.len();
            let cutoff: i64 = sql[at..]
                .split(',')
                .next()
                .and_then(|n| n.trim().parse().ok())
                .expect("cutoff number");
            assert_eq!(cutoff, i64::from(hours) + 1, "{sql}");
            for now_min in (0..48 * 60).step_by(7) {
                for start_hour in 0..48_i64 {
                    let start = start_hour * 60;
                    // A partition's minTimestamp is anywhere inside its hour.
                    for min_ts in [start, start + 30, start + 59] {
                        let selected = min_ts < now_min - cutoff * 60;
                        if selected {
                            let end = start + 60;
                            assert!(
                                end <= now_min - i64::from(MIN_HOT_HOURS) * 60,
                                "hour starting {start} selected at {now_min} \
                                 ended less than 4 h ago (configured {configured})"
                            );
                        }
                    }
                }
            }
        }
        // The window is measured from the hour's end: with the 4-hour floor
        // (cutoff 5 h on minTimestamp) the 11:00 hour, which ends at 12:00,
        // is not selected at 15:59 and is at 16:01. The old start-based rule
        // (cutoff 4 h) selected it already at 15:01.
        let selected = |min_ts: i64, now: i64, cutoff_h: i64| min_ts < now - cutoff_h * 60;
        let eleven = 11 * 60;
        assert!(!selected(eleven, 15 * 60 + 59, 5));
        assert!(selected(eleven, 16 * 60 + 1, 5));
        assert!(
            selected(eleven, 15 * 60 + 1, 4),
            "the old rule, for contrast"
        );
    }

    #[test]
    fn test_ticks_off_by_default() {
        assert!(!PartitionRetentionConfig::default().ticks_hour_path_enabled);
        // The live hour window (`config/base.toml` sets 4; the serde default
        // is 0, which is off for every table and would prove nothing here).
        let default = PartitionRetentionConfig {
            depth_hot_hours: 4,
            ..PartitionRetentionConfig::default()
        };
        assert_eq!(hour_window_for("market_depth", &default), Some(4));
        let pressure = PartitionRetentionConfig {
            under_disk_pressure: true,
            ..default.clone()
        };
        assert_eq!(hour_window_for("tick_quotes", &pressure), Some(4));
        assert_eq!(hour_window_for(TICKS_TABLE, &default), None);
        assert_eq!(
            hour_window_for(TICKS_TABLE, &pressure),
            None,
            "ticks must stay on its day window by default, pressure or not"
        );
        // Only the explicit switch, and only under pressure, puts it on.
        let on = PartitionRetentionConfig {
            ticks_hour_path_enabled: true,
            ..pressure.clone()
        };
        assert_eq!(hour_window_for(TICKS_TABLE, &on), Some(4));
        let on_no_pressure = PartitionRetentionConfig {
            ticks_hour_path_enabled: true,
            ..default
        };
        assert_eq!(hour_window_for(TICKS_TABLE, &on_no_pressure), None);
        assert!(!PRESSURE_HOUR_TABLES.contains(&TICKS_TABLE));
        assert!(!ARRIVAL_STAMPED_HOUR_TABLES.contains(&TICKS_TABLE));
    }

    #[test]
    fn test_pressure_tables_only_under_pressure_and_switch() {
        for table in PRESSURE_HOUR_TABLES {
            assert_eq!(
                hour_window_for(table, &cfg(false, 4, true, false, true)),
                None,
                "{table} without pressure keeps its day window"
            );
            assert_eq!(
                hour_window_for(table, &cfg(true, 4, true, false, true)),
                Some(4)
            );
            assert_eq!(
                hour_window_for(table, &cfg(true, 4, false, false, true)),
                None,
                "{table}: the rollback switch keeps it on days"
            );
            assert_eq!(
                hour_window_for(table, &cfg(true, 0, true, false, true)),
                None,
                "{table}: 0 is off"
            );
            assert_eq!(
                hour_window_for(table, &cfg(true, 2, true, false, true)),
                Some(MIN_HOT_HOURS),
                "{table}: clamped up to the floor"
            );
        }
        // market_depth keeps its every-pass hour path.
        assert_eq!(
            hour_window_for("market_depth", &cfg(false, 4, false, false, true)),
            Some(4)
        );
        assert_eq!(
            hour_window_for("market_depth", &cfg(false, 0, true, true, true)),
            None
        );
    }

    #[test]
    fn test_new_tables_not_forced_with_own_spill_pending() {
        for table in PRESSURE_HOUR_TABLES.iter().chain([&TICKS_TABLE]) {
            let c = cfg(true, 4, true, true, true);
            let hours = hour_window_for(table, &c);
            assert_eq!(hours, Some(4), "{table}");
            assert!(!may_force_with_spill_pending(table, &c), "{table}");
            assert_eq!(
                hour_window_decision(hours, true, true, may_force_with_spill_pending(table, &c)),
                HourWindowDecision::DeferredToDays,
                "{table} must wait for its own spill folder to drain"
            );
            assert_eq!(
                hour_window_decision(hours, false, true, false),
                HourWindowDecision::Hours(4),
                "{table} with its folder drained takes the hour path"
            );
        }
        // market_depth: forced only behind its switch.
        let on = cfg(true, 4, true, false, true);
        let off = cfg(true, 4, true, false, false);
        assert!(may_force_with_spill_pending("market_depth", &on));
        assert!(!may_force_with_spill_pending("market_depth", &off));
        assert_eq!(
            hour_window_decision(Some(4), true, true, true),
            HourWindowDecision::ForcedHours(4)
        );
        assert_eq!(
            hour_window_decision(Some(4), true, true, false),
            HourWindowDecision::DeferredToDays
        );
    }

    #[test]
    fn test_own_folder_only() {
        let depth = [crate::depth_persistence::DEPTH_SPILL_DIR];
        let tick = [crate::tick_persistence::TICK_SPILL_DIR];
        for table in DEPTH_TABLES {
            assert_eq!(spill_dirs_for_table(table), depth, "{table}");
        }
        for table in TICK_SPILL_TABLES {
            assert_eq!(spill_dirs_for_table(table), tick, "{table}");
        }
        // Every hour-path table has exactly one folder of its own.
        for table in HOUR_PATH_TABLES {
            assert_eq!(spill_dirs_for_table(table).len(), 1, "{table}");
        }
        // A table on neither list keeps both folders (it never takes the hour
        // path, so this is only the conservative answer).
        assert_eq!(
            spill_dirs_for_table("candles_1m"),
            [
                crate::depth_persistence::DEPTH_SPILL_DIR,
                crate::tick_persistence::TICK_SPILL_DIR,
            ]
        );
    }

    #[test]
    fn test_snapshot_gate_follows_the_hour_path() {
        assert!(uses_snapshot_gate("market_depth", &[]));
        assert!(uses_snapshot_gate("market_depth_book", &[]));
        assert!(!uses_snapshot_gate("tick_quotes", &[]));
        assert!(uses_snapshot_gate("tick_quotes", &["tick_quotes"]));
        assert!(!uses_snapshot_gate("feed_aux_packets", &["tick_quotes"]));
        assert!(!uses_snapshot_gate("candles_1m", &["tick_quotes"]));
    }
}
