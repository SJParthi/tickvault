//! PR AA review finding 11 (2026-11-09): the hour-path-table partitions any
//! archive leg has sent a DROP for, per IST day, each with its `diskSize`.
//!
//! The daily leg sizes its free-space target from the bytes one session
//! leaves on disk ([`super::PartitionArchiver::session_bytes_on`]), and it
//! read only what was STILL listed: an hour the disk-pressure leg had already
//! copied and dropped that day was missing, so a pressure day read low and
//! pulled the target's p95 down on exactly the heavy days that set it. Both
//! legs drop through one function, which records each partition here BEFORE
//! its DROP is sent; the reading adds back every recorded partition the
//! listing no longer shows.
//!
//! Review of finding 11 (2026-11-09), why the record is per PARTITION and
//! taken before the DROP, whatever the DROP answers:
//! - A DROP whose answer was lost (client timeout, connection reset) may
//!   have run on QuestDB. Recording "not dropped" read the day short with no
//!   error. Now a recorded partition the listing still shows is counted from
//!   the listing, and one it does not show is counted from the record, so
//!   the DROP's answer does not matter.
//! - A DROP QuestDB accepted but has not applied yet (WAL table) is still
//!   listed. Adding the tally to the listing counted it twice. A listed
//!   partition is now counted once, from the listing.
//! - A partition of unknown size no longer marks the whole day unknown: it
//!   refuses the reading only once it is gone from the listing.
//! - The same partition dropped twice (a refused or unapplied drop retried
//!   by a later pass) is one record, never two.
//!
//! Fail-closed: a partition gone from the listing whose size was never
//! known, or a day with more records than [`HOUR_PATH_DROP_NAMES_PER_DAY`],
//! makes the reading an error (the caller retries on its next tick). Never a
//! partial sum, never a double count.
//!
//! RAM only. HONEST LIMIT: a restart forgets the drops before it, so a day
//! first measured after a restart that day can still read low (the 30%-of-disk
//! floor of the target still bounds it).
//!
//! Cost: one uncontended lock, a scan of [`HOUR_PATH_DROP_DAYS`] slots and a
//! scan of at most [`HOUR_PATH_DROP_NAMES_PER_DAY`] names per drop; one copy
//! of the day's records per reading. Cold path, never the frame drain.

use std::sync::{Mutex, PoisonError};

/// Day slots kept: today, yesterday and slack across midnight. A drop for a
/// fifth day replaces the oldest day.
pub(crate) const HOUR_PATH_DROP_DAYS: usize = 4;

/// Distinct partitions recorded per day. The five hour-path tables hold at
/// most 24 hour partitions and one day partition each per day (125); past
/// this bound the day reads unknown.
pub(crate) const HOUR_PATH_DROP_NAMES_PER_DAY: usize = 160;

/// One partition a DROP was sent for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DroppedPartition {
    pub(crate) table: &'static str,
    pub(crate) partition: String,
    /// `diskSize` just before the DROP; `None` when it could not be listed.
    pub(crate) bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DroppedDay {
    /// `NaiveDate::num_days_from_ce`; meaningful only when `used`.
    day_ce: i32,
    used: bool,
    partitions: Vec<DroppedPartition>,
    /// More distinct partitions than [`HOUR_PATH_DROP_NAMES_PER_DAY`].
    overflowed: bool,
}

/// What [`HourPathDropTally::read`] reports for one day.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DayDrops {
    pub(crate) partitions: Vec<DroppedPartition>,
    pub(crate) overflowed: bool,
}

/// The per-day drop record. The process has one ([`process_drop_tally`]);
/// tests build their own so they never share it.
#[derive(Debug)]
pub(crate) struct HourPathDropTally {
    days: Mutex<[DroppedDay; HOUR_PATH_DROP_DAYS]>,
}

impl HourPathDropTally {
    /// An empty tally. `const` so the process instance needs no lazy setup.
    pub(crate) const fn new() -> Self {
        const EMPTY: DroppedDay = DroppedDay {
            day_ce: 0,
            used: false,
            partitions: Vec::new(),
            overflowed: false,
        };
        Self {
            days: Mutex::new([EMPTY; HOUR_PATH_DROP_DAYS]),
        }
    }

    /// The slot for `day_ce`, claiming a free one or the oldest day's.
    /// O([`HOUR_PATH_DROP_DAYS`]).
    fn slot(days: &mut [DroppedDay; HOUR_PATH_DROP_DAYS], day_ce: i32) -> &mut DroppedDay {
        let index = days
            .iter()
            .position(|d| d.used && d.day_ce == day_ce)
            .or_else(|| days.iter().position(|d| !d.used))
            .unwrap_or_else(|| {
                days.iter()
                    .enumerate()
                    .min_by_key(|(_, d)| d.day_ce)
                    .map_or(0, |(i, _)| i)
            });
        let slot = &mut days[index];
        if !(slot.used && slot.day_ce == day_ce) {
            *slot = DroppedDay {
                day_ce,
                used: true,
                ..DroppedDay::default()
            };
        }
        slot
    }

    /// Records that a DROP of `table`'s `partition` (of IST `day`) is about
    /// to be sent, with its `diskSize` just before (`None` when unknown).
    /// Call it BEFORE the DROP, whatever the DROP then answers. A partition
    /// already recorded keeps one record: its size is the newest attempt's
    /// when known and not smaller, and unknown when the newest is unknown.
    /// O([`HOUR_PATH_DROP_DAYS`] + [`HOUR_PATH_DROP_NAMES_PER_DAY`]).
    pub(crate) fn record(
        &self,
        day: chrono::NaiveDate,
        table: &'static str,
        partition: &str,
        bytes: Option<u64>,
    ) {
        use chrono::Datelike;
        let mut days = self.days.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = Self::slot(&mut days, day.num_days_from_ce());
        if let Some(known) = slot
            .partitions
            .iter_mut()
            .find(|p| p.table == table && p.partition == partition)
        {
            known.bytes = match (known.bytes, bytes) {
                (Some(old), Some(new)) => Some(old.max(new)),
                (_, newest) => newest,
            };
            return;
        }
        if slot.partitions.len() >= HOUR_PATH_DROP_NAMES_PER_DAY {
            slot.overflowed = true;
            return;
        }
        slot.partitions.push(DroppedPartition {
            table,
            partition: partition.to_string(),
            bytes,
        });
    }

    /// The record for `day` (empty when nothing was dropped that day).
    pub(crate) fn read(&self, day: chrono::NaiveDate) -> DayDrops {
        use chrono::Datelike;
        let day_ce = day.num_days_from_ce();
        let days = self.days.lock().unwrap_or_else(PoisonError::into_inner);
        days.iter()
            .find(|d| d.used && d.day_ce == day_ce)
            .map_or_else(DayDrops::default, |d| DayDrops {
                partitions: d.partitions.clone(),
                overflowed: d.overflowed,
            })
    }
}

static PROCESS_DROP_TALLY: HourPathDropTally = HourPathDropTally::new();

/// The process tally both archive legs record into.
pub(crate) fn process_drop_tally() -> &'static HourPathDropTally {
    &PROCESS_DROP_TALLY
}

/// The IST day a partition name belongs to, by the same rule as
/// `sum_partition_bytes_on_day`: `YYYY-MM-DD` alone or followed by `T…`.
/// `None` for any other name. Pure, O(1).
pub(crate) fn partition_day(name: &str) -> Option<chrono::NaiveDate> {
    let date = name.get(..10)?;
    let rest = name.get(10..)?;
    if !(rest.is_empty() || rest.starts_with('T')) {
        return None;
    }
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// The bytes IST `day` holds on the hour-path tables: the day's partitions
/// in `listings` (one `(table, rows)` per table, rows as `(name, diskSize)`),
/// plus every partition in `drops` (read AFTER the listings) that its
/// table's listing no longer shows. A recorded partition still listed is
/// counted once, from the listing (its DROP was refused, unanswered, or not
/// applied yet). A table missing from `listings` lists nothing.
///
/// Why one read of the tally after the listings is enough: a DROP is
/// recorded before it is sent, so a partition gone from a listing was
/// recorded before that listing ran; a partition dropped after its table was
/// listed was counted by the listing and is skipped here. Pure, O(drops ×
/// rows of its table).
///
/// # Errors
/// The reason, as text for the caller's log line: a recorded partition gone
/// from the listing whose size was never known, or more recorded partitions
/// than the tally keeps.
pub(crate) fn session_bytes_with_drops(
    listings: &[(&str, &[(String, u64)])],
    day: chrono::NaiveDate,
    drops: &DayDrops,
) -> Result<u64, &'static str> {
    if drops.overflowed {
        return Err("more hour-path partitions of the day were dropped than the tally keeps");
    }
    let mut total = listings.iter().fold(0_u64, |sum, (_, rows)| {
        sum.saturating_add(crate::partition_manager::sum_partition_bytes_on_day(
            rows, day,
        ))
    });
    for dropped in &drops.partitions {
        // O(1) EXEMPT: begin — one table's partition list per recorded drop
        // (at most a few hundred names), cold, once per session reading.
        let listed = listings
            .iter()
            .filter(|(table, _)| *table == dropped.table)
            .any(|(_, rows)| rows.iter().any(|(name, _)| *name == dropped.partition));
        // O(1) EXEMPT: end
        if listed {
            continue;
        }
        match dropped.bytes {
            Some(bytes) => total = total.saturating_add(bytes),
            None => {
                return Err("an hour-path partition of the day was dropped with no known size");
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const DEPTH: &str = "market_depth";
    const QUOTES: &str = "tick_quotes";

    fn day(d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 11, d).expect("date")
    }

    fn rows(list: &[(&str, u64)]) -> Vec<(String, u64)> {
        list.iter().map(|(n, b)| ((*n).to_string(), *b)).collect()
    }

    #[test]
    fn test_hour_path_dropped_tally_keeps_four_days() {
        let tally = HourPathDropTally::new();
        for d in 1..=4 {
            tally.record(
                day(d),
                DEPTH,
                &format!("2026-11-0{d}T09"),
                Some(u64::from(d)),
            );
        }
        assert_eq!(tally.read(day(1)).partitions.len(), 1);
        assert_eq!(tally.read(day(4)).partitions[0].bytes, Some(4));
        // A fifth day replaces the oldest.
        tally.record(day(5), DEPTH, "2026-11-05T09", Some(5));
        assert_eq!(tally.read(day(1)), DayDrops::default());
        assert_eq!(tally.read(day(2)).partitions[0].bytes, Some(2));
        assert_eq!(tally.read(day(5)).partitions[0].bytes, Some(5));
        assert_eq!(tally.read(day(9)), DayDrops::default());
    }

    #[test]
    fn test_record_keeps_one_entry_per_partition() {
        let tally = HourPathDropTally::new();
        tally.record(day(7), DEPTH, "2026-11-07T09", Some(600));
        tally.record(day(7), DEPTH, "2026-11-07T09", Some(700));
        tally.record(day(7), DEPTH, "2026-11-07T09", Some(650));
        // Same name on another table is another partition.
        tally.record(day(7), QUOTES, "2026-11-07T09", Some(1));
        let read = tally.read(day(7));
        assert_eq!(read.partitions.len(), 2);
        assert_eq!(read.partitions[0].bytes, Some(700));
        // The newest attempt unknown makes the record unknown; a later known
        // size makes it known again.
        tally.record(day(7), DEPTH, "2026-11-07T09", None);
        assert_eq!(tally.read(day(7)).partitions[0].bytes, None);
        tally.record(day(7), DEPTH, "2026-11-07T09", Some(5));
        assert_eq!(tally.read(day(7)).partitions[0].bytes, Some(5));
    }

    #[test]
    fn test_record_past_the_name_bound_overflows_the_day() {
        let tally = HourPathDropTally::new();
        for i in 0..HOUR_PATH_DROP_NAMES_PER_DAY {
            tally.record(day(7), DEPTH, &format!("p{i}"), Some(1));
        }
        assert!(!tally.read(day(7)).overflowed);
        // A known name still updates in place.
        tally.record(day(7), DEPTH, "p0", Some(2));
        assert!(!tally.read(day(7)).overflowed);
        tally.record(day(7), DEPTH, "one-too-many", Some(1));
        let read = tally.read(day(7));
        assert!(read.overflowed);
        assert_eq!(read.partitions.len(), HOUR_PATH_DROP_NAMES_PER_DAY);
        assert!(session_bytes_with_drops(&[], day(7), &read).is_err());
    }

    #[test]
    fn test_partition_day_reads_day_and_hour_names() {
        assert_eq!(partition_day("2026-11-07"), Some(day(7)));
        assert_eq!(partition_day("2026-11-07T09"), Some(day(7)));
        assert_eq!(partition_day("2026-11-07.2"), None);
        assert_eq!(partition_day("2026-11"), None);
        assert_eq!(partition_day("garbage-xx"), None);
        assert_eq!(partition_day(""), None);
    }

    // Regression: 2026-11-09 — review of PR AA finding 11. A recorded
    // partition the listing still shows (its DROP was accepted but not yet
    // applied, or refused, or unanswered) is counted once, from the listing;
    // one the listing no longer shows is counted from the record.
    #[test]
    fn test_session_bytes_with_drops_counts_a_still_listed_drop_once() {
        let tally = HourPathDropTally::new();
        tally.record(day(7), DEPTH, "2026-11-07T14", Some(1_000));
        tally.record(day(7), DEPTH, "2026-11-07T09", Some(400));
        let depth = rows(&[("2026-11-07T14", 1_000), ("2026-11-06T15", 9)]);
        let quotes = rows(&[("2026-11-07T10", 7)]);
        let listings: [(&str, &[(String, u64)]); 2] = [(DEPTH, &depth), (QUOTES, &quotes)];
        let read = tally.read(day(7));
        // T14 listed (1,000 once) + T10 (7) + T09 dropped (400).
        assert_eq!(
            session_bytes_with_drops(&listings, day(7), &read),
            Ok(1_407)
        );
    }

    // Regression: 2026-11-09 — review of PR AA finding 11. A partition of
    // unknown size refuses the reading only once it is gone from the
    // listing; it no longer marks the whole day unknown.
    #[test]
    fn test_session_bytes_with_drops_refuses_only_a_gone_unknown_partition() {
        let tally = HourPathDropTally::new();
        tally.record(day(7), DEPTH, "2026-11-07T14", None);
        let still = rows(&[("2026-11-07T14", 50)]);
        let read = tally.read(day(7));
        assert_eq!(
            session_bytes_with_drops(&[(DEPTH, &still)], day(7), &read),
            Ok(50)
        );
        assert!(session_bytes_with_drops(&[(DEPTH, &[])], day(7), &read).is_err());
        // The same name listed on ANOTHER table does not stand in for it.
        assert!(session_bytes_with_drops(&[(QUOTES, &still)], day(7), &read).is_err());
        assert_eq!(
            session_bytes_with_drops(&[], day(7), &DayDrops::default()),
            Ok(0)
        );
    }

    #[test]
    fn test_session_bytes_with_drops_saturates() {
        let tally = HourPathDropTally::new();
        tally.record(day(7), DEPTH, "2026-11-07T09", Some(u64::MAX));
        let listed = rows(&[("2026-11-07T10", 5)]);
        let read = tally.read(day(7));
        assert_eq!(
            session_bytes_with_drops(&[(DEPTH, &listed)], day(7), &read),
            Ok(u64::MAX)
        );
    }

    #[test]
    fn test_process_drop_tally_is_one_instance() {
        assert!(std::ptr::eq(process_drop_tally(), process_drop_tally()));
    }

    proptest! {
        /// Whatever is dropped, refused, retried, left unapplied or answered
        /// with an error, the listing left plus the tally is the bytes the
        /// day held; it errs only when a partition of unknown size is gone.
        #[test]
        fn proptest_listing_plus_tally_is_the_day(
            sizes in proptest::collection::vec(0_u64..1_000_000, 0..30),
            // 0 = not dropped, 1 = dropped and applied, 2 = DROP sent but
            // still listed (refused, unanswered or not applied yet).
            fate in proptest::collection::vec(0_u8..3, 30),
            size_known in proptest::collection::vec(any::<bool>(), 30),
            retried in proptest::collection::vec(any::<bool>(), 30),
        ) {
            let tally = HourPathDropTally::new();
            let before: u64 = sizes.iter().sum();
            let mut listed: Vec<(String, u64)> = Vec::new();
            let mut gone_unknown = false;
            for (i, size) in sizes.iter().enumerate() {
                let name = format!("2026-11-07T{:02}", i % 24);
                // Distinct names: the hour plus a table pick per index.
                let table = if i < 24 { DEPTH } else { QUOTES };
                let known = size_known[i].then_some(*size);
                if fate[i] != 0 {
                    tally.record(day(7), table, &name, known);
                    if retried[i] {
                        tally.record(day(7), table, &name, known);
                    }
                }
                if fate[i] == 1 {
                    gone_unknown |= known.is_none();
                } else if table == DEPTH {
                    // A `tick_quotes` partition is listed under its own
                    // table below.
                    listed.push((name, *size));
                }
            }
            let quotes: Vec<(String, u64)> = sizes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i >= 24 && fate[*i] != 1)
                .map(|(i, s)| (format!("2026-11-07T{:02}", i % 24), *s))
                .collect();
            let listings: [(&str, &[(String, u64)]); 2] = [(DEPTH, &listed), (QUOTES, &quotes)];
            let got = session_bytes_with_drops(&listings, day(7), &tally.read(day(7)));
            if gone_unknown {
                prop_assert!(got.is_err());
            } else {
                prop_assert_eq!(got, Ok(before));
            }
        }
    }
}
