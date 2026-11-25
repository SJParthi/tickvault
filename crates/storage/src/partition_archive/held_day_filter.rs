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
//! A released day waits too while the Brutex export has not sealed it
//! ([`crate::held_days::HeldDays::archive_holds`]), so its repaired hours are
//! exported in DEDUP-key order before the archiver copies them (hostile
//! review 2026-11-20).
//!
//! A `.hold` written while a pass runs (the feed-time page's auto-hold) is
//! re-read just before each held-kind partition is copied
//! ([`super::PartitionArchiver::held_since_pass_start`]), so the running
//! pass stops at the next partition, not only the next pass.
//!
//! Cost: one archive verdict (at most three small file reads) per distinct
//! (day) among held-kind partitions in the worklist, then O(worklist)
//! compares, once per pass; plus one verdict per held-kind partition just
//! before its copy. All off the runtime worker. Cold path.

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

/// One distinct day's decision: its verdict, and whether the archiver keeps
/// it back ([`HeldDays::archive_verdict`]).
pub(crate) type DayDecision = (NaiveDate, HeldVerdict, bool);

/// Splits `worklist` into the partitions kept and the number held, with the
/// archive decision of each distinct held-kind day read once through
/// `decide`. Pure apart from `decide`, O(worklist × distinct days) with a
/// handful of distinct days.
pub(crate) fn split_held_partitions(
    worklist: Vec<(&'static str, String)>,
    mut decide: impl FnMut(NaiveDate) -> (HeldVerdict, bool),
) -> (Vec<(&'static str, String)>, u32, Vec<DayDecision>) {
    let mut seen: Vec<DayDecision> = Vec::new();
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
            if let Some((_, _, keep)) = seen.iter().find(|(d, _, _)| *d == day) {
                *keep
            } else {
                let (verdict, keep) = decide(day);
                seen.push((day, verdict, keep));
                keep
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
        let (kept, held, decisions) = crate::off_worker::off_worker(|| {
            split_held_partitions(worklist, |day| held_days.archive_verdict(day))
        });
        summary.held_day = summary.held_day.saturating_add(held);
        if held > 0 {
            count_held_day(u64::from(held));
        }
        for (day, verdict, keep) in decisions {
            match verdict {
                HeldVerdict::Held {
                    source,
                    done_damaged,
                } => {
                    if done_damaged {
                        warn!(
                            %day,
                            "partition archive: this held day has a repair `.done` file that \
                             fails its check or is dated after today, so the day stays held \
                             (nothing copied or dropped) until a valid one from the repair \
                             tool replaces it"
                        );
                    }
                    info!(
                        %day,
                        source = ?source,
                        "partition archive: day HELD (Quote 63) — its tick, quote, aux and \
                         candle partitions are not copied or dropped until its repair is verified"
                    );
                }
                HeldVerdict::Released { .. } | HeldVerdict::NotHeld if keep => {
                    info!(
                        %day,
                        "partition archive: repaired day waits for its Brutex export (Quote \
                         63) — its tick, quote, aux and candle partitions are copied after the \
                         export seals it, so they reach S3 in order"
                    );
                }
                HeldVerdict::Released { .. } | HeldVerdict::NotHeld => {}
            }
        }
        kept
    }

    /// Whether `partition` of `table` became held after this pass listed it
    /// (a `.hold` written mid-pass): re-read just before its copy; when so,
    /// counted in `summary.held_day` and on the `held_day` skip counter. A
    /// table that is never held answers `false` with no file read.
    // TEST-EXEMPT: one call of HeldDays::holds_partition plus the count; covered by test_regression_held_day_archive_pass_and_export_day_write_nothing_until_done and test_holds_partition_and_table_is_held_kind_and_partition_day
    pub(crate) fn held_since_pass_start(
        &self,
        table: &str,
        partition: &str,
        summary: &mut ArchiveRunSummary,
    ) -> bool {
        if !table_is_held_kind(table) {
            return false;
        }
        let held =
            crate::off_worker::off_worker(|| self.held_days.holds_partition(table, partition));
        if held {
            summary.held_day = summary.held_day.saturating_add(1);
            count_held_day(1);
            info!(
                table,
                partition,
                "partition archive: day became HELD during this pass (Quote 63); this \
                 partition is not copied or dropped"
            );
        }
        held
    }

    /// Whether the drain-hold backlog counts `partition` of `table`: not when
    /// the archiver keeps its day back (nothing tonight can copy it). At most
    /// three small file reads, off the runtime worker.
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
        let (kept, held, decisions) = split_held_partitions(worklist, |day| {
            reads += 1;
            if day == d("2026-11-20") {
                (
                    HeldVerdict::Held {
                        source: HeldSource::Config,
                        done_damaged: false,
                    },
                    true,
                )
            } else {
                (HeldVerdict::NotHeld, false)
            }
        });
        assert_eq!(held, 4);
        assert_eq!(reads, 2, "each distinct day is read once");
        assert_eq!(decisions.len(), 2);
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
        let released = HeldVerdict::Released {
            done_on: d("2026-11-25"),
        };
        let (kept, held, _) = split_held_partitions(worklist.clone(), |_| (released, false));
        assert_eq!(held, 0);
        assert_eq!(kept, worklist);
        // Released but waiting for its export: still kept back.
        let (kept, held, _) = split_held_partitions(worklist, |_| (released, true));
        assert_eq!(held, 1);
        assert!(kept.is_empty());
    }

    /// The per-partition re-read sits in the pass loop before any copy
    /// (behaviour: `test_held_since_pass_start_counts_a_hold_written_mid_pass`
    /// in the stub tests).
    #[test]
    fn test_held_since_pass_start_runs_before_every_copy_in_the_pass_loop() {
        let src = include_str!("../partition_archive.rs");
        let call = src
            .find("self.held_since_pass_start(table, &partition, &mut summary)")
            .expect("the pass loop re-reads the hold");
        for copy in [
            "self.process_one(table, &partition, &mut summary)",
            "archive_with_snapshot_gate(self, table, &partition, gate, &mut summary)",
            "self.archive_one_quiesced(table, &partition, &mut summary)",
        ] {
            let at = src.find(copy).expect(copy);
            assert!(call < at, "the hold re-read must come before `{copy}`");
        }
        assert!(!table_is_held_kind("market_depth_book"));
    }

    #[test]
    fn test_count_held_day_registers_without_panicking() {
        // No recorder installed: a no-op, but the call must be safe at 0.
        count_held_day(0);
        count_held_day(3);
        assert_eq!(
            split_held_partitions(Vec::new(), |_| (HeldVerdict::NotHeld, false)).1,
            0
        );
    }
}
