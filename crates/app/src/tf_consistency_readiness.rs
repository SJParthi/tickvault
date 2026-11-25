//! When the daily timeframe-consistency check may read, and which of its
//! differences the fold produces by design (plan item M3, 2026-10-26).
//!
//! # What the live data showed
//!
//! The check (`tf_consistency_boot`) fired at 15:40:00 IST every day and
//! reported, for example on 2026-10-26, 48,581 mismatches and 27,680 missing
//! higher-timeframe rows. Two separate things were wrong, and neither was a
//! fold bug.
//!
//! 1. **It read before the rows it judged existed.** A bucket seals when the
//!    live catch-up sweep passes its natural end plus
//!    `CATCHUP_LATENESS_MARGIN_SECS` (240 s). At 15:40:00 every bucket ending
//!    after about 15:36 is still open: the last 3/5/10-minute bars (end
//!    15:40 to 15:42), the 15- and 30-minute bars that span the close (end
//!    15:45, sealed at about 15:49) and the hourly bar 15:15 to 16:15. The
//!    seal writer logged 10,854 rows written at 15:43 to 15:44 and 18,632 at
//!    15:48 to 15:49 on 2026-10-26, after the check had read. Every later
//!    run of the same day read `missing_tf_rows = 0` (13, 14, 21, 22 and
//!    23 October). [`TfReadiness`] makes the run wait, bounded, until the
//!    live side has sealed past [`TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST`],
//!    the seal writer drained after that sweep, no seal spill file for the
//!    day is staged, and every candle table it reads applied its WAL through
//!    a snapshot taken after that. Whatever is still unsealed when it reads
//!    (the hourly 15:15 bar, or anything past the floor if the wait ran out)
//!    is skipped and counted (`unsealed_at_read`), never called missing.
//!
//! 2. **Two differences are the fold attributing more to a longer bar.**
//!    The fold only credits a move to a bar when the move provably happened
//!    inside it. A longer bar spans more time, so it can prove more:
//!
//!    * [`AttributedShape::ExtremeStraddle`]: the exchange's day high or low
//!      moved between two of our packets that fall in different minutes of
//!      the same longer bar. The 1-minute bars refuse it (it could belong to
//!      either minute); the longer bar takes it. Live example, 2026-10-23,
//!      `IDX_I` id 3: day low 16232.25 at the packet received 12:51:59.95,
//!      16232.05 at 12:52:00.46. The 3-minute 12:51 bar stores low 16232.05;
//!      its 1-minute bars' lowest low is 16232.25. Open, close, volume and
//!      tick count agree.
//!    * [`AttributedShape::FirstTrade`]: a contract whose first packet of the
//!      day comes after 09:16. The trades before it happened between 09:15
//!      and that packet, inside the 09:15 bar of a longer timeframe but not
//!      inside one 1-minute bar, so the longer bar takes the day's first
//!      volume and open and the 1-minute bar does not. Live example,
//!      2026-10-23, `NSE_FNO` id 35633 (DIVISLAB 27 Oct 9400 CE): 5-minute
//!      09:15 volume 12,900, its 1-minute bars sum to 11,700, the first
//!      1-minute row is 09:16 with volume 0.
//!
//!    Reproduced offline from the 2026-10-23 S3 archive of `candles_1m` and
//!    the 3/5/15/30/60-minute tables: 38,850 mismatching bars; 31,474
//!    (81.0%) are the first shape and 7,376 (19.0%) the second; 0 are
//!    anything else. They are counted (`attributed_wider`) and kept out of
//!    the paging mismatch count. Anything outside the two exact shapes still
//!    pages as before.
//!
//! **Honest limits.** The classification trusts the shape: a fold bug that
//! only ever widened a longer bar's extremes, with open, close and volume
//! exact, would be counted here instead of paging; the count stays visible
//! on the run's log line and its counter. The hourly 15:15 bar is not judged
//! by the in-process run (it seals at about 16:19, the box stops at 16:30).

use tickvault_storage::seal_spill::SpillStaged;
use tickvault_storage::wal_suspension_watcher::WalTableRow;

use crate::tf_consistency_boot::{CandleRow, Recomputed, to_paise};

/// The session's first bucket start, 09:15:00 IST, as seconds of the day.
const SESSION_OPEN_SECS_OF_DAY: i64 = 33_300;

/// The latest natural bucket end the run waits for: 15:45:00 IST, where the
/// 15- and 30-minute bars that span the 15:40 close end. Every other target
/// except the hourly bar ends at or before it (the 3-minute 15:39 bar ends
/// 15:42). Pinned against the grid by
/// `test_sealed_target_is_the_latest_natural_end_below_the_hourly_bar`.
pub const TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST: i64 = 15 * 3600 + 45 * 60;

/// The wait gives up at 15:55:00 IST: the target plus the 240 s catch-up
/// margin, one 5 s sweep, and about six minutes for the seal writer and the
/// QuestDB WAL apply. Well before the 16:30 stop.
pub const TF_VERIFY_READY_DEADLINE_SECS_OF_DAY_IST: i64 = 15 * 3600 + 55 * 60;

const _: () = assert!(
    TF_VERIFY_READY_DEADLINE_SECS_OF_DAY_IST
        > TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST
            + crate::dhan_feed_stack::CATCHUP_LATENESS_MARGIN_SECS as i64
            + crate::dhan_feed_stack::CATCHUP_SEAL_INTERVAL_SECS as i64,
    "the wait must outlast the sweep that seals the target"
);

/// Seconds between readiness samples.
pub const TF_VERIFY_READY_POLL_SECS: u64 = 5;

/// Seconds per day.
const SECS_PER_DAY: i64 = 86_400;

/// Why the read was not final when it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TfNotReady {
    /// No catch-up sweep of the target day had sealed past the target.
    NotSealed,
    /// The seal writer had not drained after the sweep.
    SealsPending,
    /// A seal spill file for the day was still staged (or the folder could
    /// not be listed).
    SealSpillStaged,
    /// A candle table had not applied its WAL through the snapshot.
    NotApplied,
}

impl TfNotReady {
    /// Stable log label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotSealed => "not_sealed",
            Self::SealsPending => "seals_pending",
            Self::SealSpillStaged => "seal_spill_staged",
            Self::NotApplied => "not_applied",
        }
    }
}

/// What the readiness wait found: the seconds of the day through which every
/// bucket was sealed (buckets ending after it are skipped), and whether the
/// read was final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TfReadiness {
    /// `None` when no live lane published progress for the day in this
    /// process (a past day, or no lane): every bucket is judged, as before.
    pub sealed_through_secs_of_day: Option<i64>,
    /// `Ok` when all barriers held.
    pub verdict: Result<(), TfNotReady>,
}

/// The catch-up floor as seconds of the target day, or `None` when the floor
/// is not inside that day. `floor_fold_secs` is the IST wall clock read as
/// epoch seconds (`CATCHUP_PROGRESS`). Pure, O(1).
#[must_use]
pub fn sealed_through_secs_of_day(floor_fold_secs: u32, day_start_fold_secs: i64) -> Option<i64> {
    let sod = i64::from(floor_fold_secs).saturating_sub(day_start_fold_secs);
    (0..SECS_PER_DAY).contains(&sod).then_some(sod)
}

/// The first three barriers for one sample. `drained` must be strictly after
/// the sweep that reached the floor (a drain in the same second may predate
/// its seals); a spill folder that could not be listed reads as staged
/// (fail closed). Pure, O(1).
pub fn classify_seal_barriers(
    sealed_through_secs_of_day: i64,
    sweep_done_unix_secs: i64,
    drained_unix_secs: Option<i64>,
    spill: &std::io::Result<SpillStaged>,
) -> Result<(), TfNotReady> {
    if sealed_through_secs_of_day < TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST {
        return Err(TfNotReady::NotSealed);
    }
    match drained_unix_secs {
        Some(d) if d > sweep_done_unix_secs => {}
        _ => return Err(TfNotReady::SealsPending),
    }
    match spill {
        Ok(s) if s.staged == 0 => Ok(()),
        _ => Err(TfNotReady::SealSpillStaged),
    }
}

/// `sequencerTxn` of each named table that QuestDB reports as a WAL table.
/// A table absent from the listing has nothing pending and gets no entry.
/// O(tables × rows), cold.
#[must_use]
pub fn snapshot_sequencer_txns(tables: &[&str], rows: &[WalTableRow]) -> Vec<(String, i64)> {
    tables
        .iter()
        .filter_map(|t| {
            rows.iter()
                .find(|r| r.name == *t)
                .map(|r| ((*t).to_string(), r.sequencer_txn.unwrap_or(0)))
        })
        .collect()
}

/// `true` when every snapshotted table's `writerTxn` reached its snapshot and
/// none is suspended. A table missing from `rows` is not applied (its listing
/// vanished). O(snapshot × rows), cold.
#[must_use]
pub fn tables_applied(snapshot: &[(String, i64)], rows: &[WalTableRow]) -> bool {
    snapshot.iter().all(|(name, seq)| {
        rows.iter()
            .find(|r| &r.name == name)
            .is_some_and(|r| !r.suspended && r.writer_txn.is_some_and(|w| w >= *seq))
    })
}

/// Which by-design difference a mismatching longer bar is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributedShape {
    /// A day high or low the exchange set between two packets in different
    /// minutes of this bar.
    ExtremeStraddle,
    /// The day's first volume and open, in the 09:15 bar, for a contract
    /// whose first packet came after the bar opened.
    FirstTrade,
}

impl AttributedShape {
    /// Stable counter label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExtremeStraddle => "extreme_straddle",
            Self::FirstTrade => "first_trade",
        }
    }
}

/// Whether a stored longer bar that differs from its 1-minute recompute is
/// one of the two by-design shapes. Call only when the bar differs.
///
/// Both shapes need: close equal, stored high at or above the recompute and
/// stored low at or below it (a longer bar can only know MORE). Then:
///
/// * `ExtremeStraddle`: open and volume equal too, and an extreme is wider.
/// * `FirstTrade`: the bar is the session's first (09:15), the contract's
///   first 1-minute row of the day is after that bar opened, and the stored
///   volume is at least the recompute's.
///
/// Anything else returns `None` and stays a paging mismatch. Pure, O(1).
#[must_use]
pub fn attributed_wider_shape(
    stored: &CandleRow,
    rec: &Recomputed,
    window_start_secs_of_day: i64,
    first_1m_secs_of_day: Option<i64>,
) -> Option<AttributedShape> {
    if to_paise(stored.close) != to_paise(rec.close)
        || to_paise(stored.high) < to_paise(rec.high)
        || to_paise(stored.low) > to_paise(rec.low)
    {
        return None;
    }
    let stored_volume = stored.volume.checked_abs()?;
    let wider =
        to_paise(stored.high) > to_paise(rec.high) || to_paise(stored.low) < to_paise(rec.low);
    if wider && to_paise(stored.open) == to_paise(rec.open) && stored_volume == rec.volume {
        return Some(AttributedShape::ExtremeStraddle);
    }
    let first_trade_bar = window_start_secs_of_day == SESSION_OPEN_SECS_OF_DAY
        && first_1m_secs_of_day.is_some_and(|f| f > window_start_secs_of_day);
    if first_trade_bar && stored_volume >= rec.volume {
        return Some(AttributedShape::FirstTrade);
    }
    None
}

/// The candle tables one run reads: `candles_1m` and every target.
#[must_use]
pub fn read_tables() -> Vec<&'static str> {
    std::iter::once(tickvault_trading::candles::TfIndex::M1.table_name())
        .chain(
            crate::tf_consistency_boot::tf_verify_targets()
                .into_iter()
                .map(|tf| tf.table_name()),
        )
        .collect()
}

/// Waits, bounded by [`TF_VERIFY_READY_DEADLINE_SECS_OF_DAY_IST`], until the
/// candles the run reads are final for `date`, sampling every
/// [`TF_VERIFY_READY_POLL_SECS`]. Returns at once with no floor when `date`
/// is not today or no live lane published progress for it.
///
/// O(spill files + tables) per sample, cold; one task, once a day.
// TEST-EXEMPT: tokio + QuestDB wiring over the unit-tested pure decisions (sealed_through_secs_of_day / classify_seal_barriers / snapshot_sequencer_txns / tables_applied); called once from run_tf_consistency.
pub async fn wait_for_sealed_candles(
    client: &reqwest::Client,
    exec_url: &str,
    date: chrono::NaiveDate,
    day_start_ist_nanos: i64,
) -> TfReadiness {
    use tracing::{info, warn};
    let day_start_fold_secs = day_start_ist_nanos.div_euclid(1_000_000_000);
    let today = chrono::Utc::now()
        .checked_add_signed(chrono::TimeDelta::seconds(
            tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64,
        ))
        .map(|t| t.date_naive());
    let no_floor = TfReadiness {
        sealed_through_secs_of_day: None,
        verdict: Ok(()),
    };
    if today != Some(date) {
        return no_floor;
    }
    let deadline_unix = day_start_fold_secs
        .saturating_add(TF_VERIFY_READY_DEADLINE_SECS_OF_DAY_IST)
        .saturating_sub(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64);
    let tables = read_tables();
    let mut snapshot: Option<Vec<(String, i64)>> = None;
    loop {
        let Some((floor, done)) = crate::dhan_feed_stack::CATCHUP_PROGRESS.load() else {
            info!(
                "tf_consistency: no live catch-up progress in this process — judging every bucket"
            );
            return no_floor;
        };
        let Some(sealed) = sealed_through_secs_of_day(floor, day_start_fold_secs) else {
            info!(
                floor,
                "tf_consistency: catch-up progress is not for the target day — judging every bucket"
            );
            return no_floor;
        };
        let done = i64::from(done);
        let spill = tokio::task::spawn_blocking(move || {
            tickvault_storage::seal_spill::staged_production_spill_records_for_day(date)
        })
        .await
        .unwrap_or_else(|join| Err(std::io::Error::other(join)));
        let mut verdict = classify_seal_barriers(
            sealed,
            done,
            tickvault_storage::seal_writer_loop::last_seal_drained_unix_secs(),
            &spill,
        );
        if verdict.is_err() {
            snapshot = None;
        } else {
            match tickvault_storage::wal_suspension_watcher::fetch_wal_tables(client, exec_url)
                .await
            {
                Ok(rows) => {
                    // One snapshot, taken the first time the floor is past
                    // the target and the drain and spill barriers hold: no
                    // target bucket ends between the target and the hourly
                    // bar's 16:15, so a later sweep seals nothing the run
                    // judges. Dropped whenever a barrier fails again.
                    let snap =
                        snapshot.get_or_insert_with(|| snapshot_sequencer_txns(&tables, &rows));
                    if !tables_applied(snap, &rows) {
                        verdict = Err(TfNotReady::NotApplied);
                    }
                }
                Err(_) => verdict = Err(TfNotReady::NotApplied),
            }
        }
        let now = chrono::Utc::now().timestamp();
        if verdict.is_ok() || now >= deadline_unix {
            if let Err(reason) = verdict {
                warn!(
                    reason = reason.as_str(),
                    sealed_through_secs_of_day = sealed,
                    "tf_consistency: candles not final by the wait deadline — \
                     buckets past the sealed floor are skipped, the rest judged"
                );
            }
            return TfReadiness {
                sealed_through_secs_of_day: Some(sealed),
                verdict,
            };
        }
        tokio::time::sleep(std::time::Duration::from_secs(TF_VERIFY_READY_POLL_SECS)).await;
    }
}

#[cfg(test)]
#[path = "tf_consistency_readiness_tests.rs"]
mod tests;
