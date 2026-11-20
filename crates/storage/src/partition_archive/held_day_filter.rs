//! The archiver's held-day filter (daily-universe Quote 63, 2026-11-20).
//!
//! Every archive pass (the daily leg, the hour path and the disk-pressure
//! leg all run [`super::PartitionArchiver::run_archive_pass`]) removes from
//! its worklist each partition of a held day's `ticks`, `tick_quotes`,
//! `feed_aux_packets` or `candles_*` table ([`crate::held_days`]) BEFORE the
//! cross-verification hold, the backlog measure and the per-pass cap: a held
//! partition is never exported, copied or dropped, takes no slot, and is not
//! counted as copy work for the drain hold. Each removal adds one to
//! `tv_partition_archive_skipped_total{reason="held_day"}`.
//!
//! Cost: one held-day verdict (at most two small file reads) per distinct
//! (day) among held-kind partitions in the worklist, then O(worklist)
//! compares, once per pass, off the runtime worker. Cold path.

use chrono::NaiveDate;
use tracing::{info, warn};

use crate::held_days::{HeldDays, HeldVerdict, partition_day, table_is_held_kind};

use super::{ArchiveRunSummary, PartitionArchiver};

/// Registers `tv_partition_archive_skipped_total{reason="held_day"}` with
/// `n` (0 at a pass start, so the series exists before the first hold).
pub(crate) fn count_held_day(n: u64) {
    metrics::counter!(
        "tv_partition_archive_skipped_total",
        "reason" => "held_day"
    )
    .increment(n);
}

/// Splits `worklist` into the partitions kept and the number held, with the
/// verdict of each distinct held-kind day read once through `verdict`.
/// Pure apart from `verdict`, O(worklist × distinct days) with a handful of
/// distinct days.
pub(crate) fn split_held_partitions(
    worklist: Vec<(&'static str, String)>,
    mut verdict: impl FnMut(NaiveDate) -> HeldVerdict,
) -> (
    Vec<(&'static str, String)>,
    u32,
    Vec<(NaiveDate, HeldVerdict)>,
) {
    let mut seen: Vec<(NaiveDate, HeldVerdict)> = Vec::new();
    let mut kept = Vec::with_capacity(worklist.len());
    let mut held: u32 = 0;
    for (table, partition) in worklist {
        let day = if table_is_held_kind(table) {
            partition_day(&partition)
        } else {
            None
        };
        let is_held = day.is_some_and(|day| {
            // O(1) EXEMPT: a handful of distinct days per pass.
            if let Some((_, v)) = seen.iter().find(|(d, _)| *d == day) {
                v.is_held()
            } else {
                let v = verdict(day);
                seen.push((day, v));
                v.is_held()
            }
        });
        if is_held {
            held = held.saturating_add(1);
        } else {
            kept.push((table, partition));
        }
    }
    (kept, held, seen)
}

impl PartitionArchiver {
    /// Removes held-day partitions from `worklist`, counting them in
    /// `summary.held_day` and on the `held_day` skip counter.
    // TEST-EXEMPT: needs an archiver; covered by test_regression_held_day_archive_pass_and_export_day_write_nothing_until_done (stub pass) and test_split_held_partitions_keeps_depth_and_other_days_and_reads_each_day_once (the split)
    pub(crate) fn apply_held_days(
        &self,
        worklist: Vec<(&'static str, String)>,
        summary: &mut ArchiveRunSummary,
    ) -> Vec<(&'static str, String)> {
        let held_days: &HeldDays = &self.held_days;
        let (kept, held, verdicts) = crate::off_worker::off_worker(|| {
            split_held_partitions(worklist, |day| held_days.verdict(day))
        });
        summary.held_day = summary.held_day.saturating_add(held);
        if held > 0 {
            count_held_day(u64::from(held));
        }
        for (day, verdict) in verdicts {
            match verdict {
                HeldVerdict::Held {
                    source,
                    done_damaged,
                } => {
                    if done_damaged {
                        warn!(
                            %day,
                            "partition archive: this held day has a repair `.done` file that \
                             fails its check, so the day stays held (nothing copied or \
                             dropped) until a valid one from the repair tool replaces it"
                        );
                    }
                    info!(
                        %day,
                        source = ?source,
                        "partition archive: day HELD (Quote 63) — its tick, quote, aux and \
                         candle partitions are not copied or dropped until its repair is verified"
                    );
                }
                HeldVerdict::Released { .. } | HeldVerdict::NotHeld => {}
            }
        }
        kept
    }

    /// Whether the drain-hold backlog counts `partition` of `table`: not when
    /// its day is held (nothing tonight can copy it). At most two small file
    /// reads, off the runtime worker.
    // TEST-EXEMPT: one call of HeldDays::holds_partition, covered by test_holds_partition_and_table_is_held_kind_and_partition_day
    pub(crate) fn backlog_counts_held(&self, table: &str, partition: &str) -> bool {
        !crate::off_worker::off_worker(|| self.held_days.holds_partition(table, partition))
    }

    /// Test seam: the held days (and their folders) this archiver reads.
    #[cfg(test)]
    // TEST-EXEMPT: test-only builder; covered by test_regression_held_day_skipped_by_all_three_paths
    pub(crate) fn with_test_held_days(mut self, held: HeldDays) -> Self {
        self.held_days = held;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::held_days::HeldSource;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date")
    }

    #[test]
    fn test_split_held_partitions_keeps_depth_and_other_days_and_reads_each_day_once() {
        let worklist = vec![
            ("ticks", "2026-11-20T09".to_string()),
            ("tick_quotes", "2026-11-20T10".to_string()),
            ("feed_aux_packets", "2026-11-20T11".to_string()),
            ("candles_1m", "2026-11-20".to_string()),
            ("market_depth_book", "2026-11-20T09".to_string()),
            ("cross_verify_1m_audit", "2026-11-20".to_string()),
            ("ticks", "2026-11-19T09".to_string()),
            ("candles_1m", "2026-11-19".to_string()),
        ];
        let mut reads = 0;
        let (kept, held, verdicts) = split_held_partitions(worklist, |day| {
            reads += 1;
            if day == d("2026-11-20") {
                HeldVerdict::Held {
                    source: HeldSource::Config,
                    done_damaged: false,
                }
            } else {
                HeldVerdict::NotHeld
            }
        });
        assert_eq!(held, 4);
        assert_eq!(reads, 2, "each distinct day is read once");
        assert_eq!(verdicts.len(), 2);
        assert_eq!(
            kept,
            vec![
                ("market_depth_book", "2026-11-20T09".to_string()),
                ("cross_verify_1m_audit", "2026-11-20".to_string()),
                ("ticks", "2026-11-19T09".to_string()),
                ("candles_1m", "2026-11-19".to_string()),
            ]
        );
    }

    #[test]
    fn test_split_held_partitions_releases_after_done() {
        let worklist = vec![("tick_quotes", "2026-11-20T10".to_string())];
        let (kept, held, _) = split_held_partitions(worklist.clone(), |_| HeldVerdict::Released {
            done_on: d("2026-11-25"),
        });
        assert_eq!(held, 0);
        assert_eq!(kept, worklist);
    }

    #[test]
    fn test_count_held_day_registers_without_panicking() {
        // No recorder installed: a no-op, but the call must be safe at 0.
        count_held_day(0);
        count_held_day(3);
        assert_eq!(
            split_held_partitions(Vec::new(), |_| HeldVerdict::NotHeld).1,
            0
        );
    }
}
