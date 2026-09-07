//! Turns the in-RAM leaderboard into the rows `top_volume_rank` stores.
//!
//! The ranking lives in RAM and dies with the process — correct for the
//! decision path, which never reads the database. The operator's ask
//! (2026-09-06) was that the ranking also be QUERYABLE afterwards, and
//! `top_volume_rank_persistence` gave that a table. This module is the pure
//! function between them: ranked contracts in, storable rows out.
//!
//! It is deliberately pure. The scheduled caller that fires it every second
//! is a separate unit of work with hot-path consequences; the projection
//! itself has none, and keeping it separable means the four refusals below
//! are testable without a runtime.
//!
//! ## The four things it refuses, and why each is a real loss if it does not
//!
//! | Refusal | What it prevents |
//! |---|---|
//! | a snapshot timestamp not on a whole second | the row's own DEDUP key stops collapsing a re-emit, so one contract holds several rows for one snapshot |
//! | a `security_id` or `underlying_id` above `i64::MAX` | QuestDB `LONG` is signed; a namespace-banded id would WRAP to a negative and be stored as a different instrument |
//! | a non-finite `gain_pct` | the §28.4 NaN-poisoning class, one table over — a NaN written to `DOUBLE` makes every later comparison on the column false |
//! | a rank that does not fit `i64` | arithmetic that cannot happen at k ≤ 250, refused rather than wrapped, because a silently negative rank reads as a valid row |
//!
//! Each refusal is COUNTED and the row dropped. Dropping one observability
//! row costs no tick; writing a wrong one costs the trust in the whole table.

use tickvault_common::types::ExchangeSegment;
use tickvault_storage::top_volume_rank_persistence::{SnapshotCadence, TopVolumeRankRow};

use crate::volume_leaderboard::{OptionFamily, RankedContract};

/// Nanoseconds in one second — the snapshot grid.
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The feed label every row carries today. A constant rather than a
/// parameter because only one feed produces ticks; when a second does, this
/// becomes an argument and the DEDUP key already separates them.
pub const SNAPSHOT_FEED: &str = "dhan";

/// Why a contract was left out of a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRefusal {
    /// The id does not fit a signed 64-bit column.
    IdTooLargeForSignedColumn,
    /// `gain_pct` was NaN or infinite.
    NonFiniteGain,
    /// The 1-based rank overflowed `i64` — unreachable at any real `k`, and
    /// refused anyway rather than wrapped.
    RankOutOfRange,
}

impl SnapshotRefusal {
    /// Stable label for the refusal counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdTooLargeForSignedColumn => "id_too_large",
            Self::NonFiniteGain => "non_finite_gain",
            Self::RankOutOfRange => "rank_out_of_range",
        }
    }
}

/// A built snapshot: the rows to write, plus what was left out and why.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotProjection {
    /// The rows, in rank order.
    pub rows: Vec<TopVolumeRankRow>,
    /// Contracts dropped, with the reason. Never silently discarded.
    pub refusals: Vec<(u64, SnapshotRefusal)>,
}

impl SnapshotProjection {
    /// How many contracts were refused.
    #[must_use]
    pub fn refusal_count(&self) -> usize {
        self.refusals.len()
    }
}

/// Truncates a timestamp DOWN to its whole second.
///
/// The DEDUP key carries `ts`, so two emits of the same snapshot must land on
/// the same nanosecond value or QuestDB keeps both. Truncation is toward
/// negative infinity rather than toward zero: a pre-epoch timestamp is not
/// reachable here, but a `%` that rounds toward zero would put two adjacent
/// pre-epoch instants on the same grid point in one direction and different
/// ones in the other, which is the kind of asymmetry nobody re-derives later.
#[must_use]
pub const fn floor_to_second(ts_nanos: i64) -> i64 {
    let rem = ts_nanos.rem_euclid(NANOS_PER_SECOND);
    ts_nanos - rem
}

/// Whether a snapshot stamped at this instant may be CAPTURED at all.
///
/// Pure, O(1), and the gate the caller must consult BEFORE ranking — not
/// after, because ranking 25,000 contracts to then throw the result away is
/// work the drain does not need to do.
///
/// # Why this exists, when zero volume already yields an empty ranking
///
/// It looks redundant and it is not. Before 09:15 every cumulative volume is
/// zero, so an empty ranking falls out on its own — **while the leaderboard
/// is fresh**. The leaderboard lives in RAM and `reset_daily` is the only
/// thing that clears it, so a process spanning midnight begins the pre-open
/// holding YESTERDAY's non-zero volumes. A value-only guard would publish 250
/// stale rows every second from 09:00, each one a confident ranking of a
/// market that has not opened.
///
/// A value gate is not a session gate. This is the session gate; wiring
/// `reset_daily` is the other half and neither substitutes for the other.
///
/// The window is `[09:15:00, 15:40:00)` — the operator's *"starting 9.15 am
/// till 3.39 pm"*, with the end exclusive so the last captured snapshot is
/// 15:39:59.
#[must_use]
pub const fn within_capture_window(secs_of_day_ist: u32) -> bool {
    secs_of_day_ist >= tickvault_common::constants::TOP_VOLUME_CAPTURE_START_SECS_OF_DAY_IST
        && secs_of_day_ist < tickvault_common::constants::TOP_VOLUME_CAPTURE_END_SECS_OF_DAY_IST
}

/// Seconds-of-day IST for an IST-nanosecond instant.
///
/// The stored timestamps on this path are ALREADY IST (the WebSocket
/// timestamp rule: `ts` is IST epoch, never UTC plus an offset), so this is a
/// modulo and deliberately NOT a timezone conversion. Adding one here would
/// be the +5:30-twice bug the data-integrity rule calls the single most
/// critical one in the repository.
#[must_use]
pub const fn secs_of_day_ist(ts_ist_nanos: i64) -> u32 {
    const SECS_PER_DAY: i64 = 86_400;
    let secs = ts_ist_nanos.div_euclid(NANOS_PER_SECOND);
    // `rem_euclid` so a pre-epoch instant lands in [0, 86_400) rather than
    // going negative and failing the cast — unreachable in production, and
    // the kind of asymmetry nobody re-derives later.
    let of_day = secs.rem_euclid(SECS_PER_DAY);
    // Bounded by the modulo above, so the cast cannot truncate.
    of_day as u32
}

/// Projects one family's ranked slice into storable rows.
///
/// `gain_pct_of` and `is_subscribed` are supplied by the caller because the
/// leaderboard holds neither: gains come from the tick that carried the
/// contract's LTP and previous close, and subscription state belongs to the
/// depth pool. Passing them in keeps this function pure and keeps the two
/// lookups the caller's O(1) hash probes rather than a scan here.
///
/// O(k) in the ranked slice, which is 250 at the authorized budget. Per
/// contract it is O(1): two closure calls and a widening.
#[must_use]
pub fn project_snapshot<G, S>(
    snapshot_ts_ist_nanos: i64,
    cadence: SnapshotCadence,
    family: OptionFamily,
    ranked: &[RankedContract],
    gain_pct_of: G,
    is_subscribed: S,
) -> SnapshotProjection
where
    G: Fn(u64, ExchangeSegment) -> f64,
    S: Fn(u64, ExchangeSegment) -> bool,
{
    let ts = floor_to_second(snapshot_ts_ist_nanos);
    let mut rows = Vec::with_capacity(ranked.len());
    let mut refusals = Vec::new();

    for (idx, contract) in ranked.iter().enumerate() {
        let Ok(security_id) = i64::try_from(contract.security_id) else {
            refusals.push((
                contract.security_id,
                SnapshotRefusal::IdTooLargeForSignedColumn,
            ));
            continue;
        };
        let Ok(underlying_id) = i64::try_from(contract.underlying_id) else {
            refusals.push((
                contract.security_id,
                SnapshotRefusal::IdTooLargeForSignedColumn,
            ));
            continue;
        };
        let Ok(rank_zero_based) = i64::try_from(idx) else {
            refusals.push((contract.security_id, SnapshotRefusal::RankOutOfRange));
            continue;
        };
        let Some(rank) = rank_zero_based.checked_add(1) else {
            refusals.push((contract.security_id, SnapshotRefusal::RankOutOfRange));
            continue;
        };

        let gain_pct = gain_pct_of(contract.security_id, contract.segment);
        if !gain_pct.is_finite() {
            refusals.push((contract.security_id, SnapshotRefusal::NonFiniteGain));
            continue;
        }

        rows.push(TopVolumeRankRow {
            snapshot_ts_ist_nanos: ts,
            cadence,
            family: family.as_str(),
            feed: SNAPSHOT_FEED,
            segment: contract.segment.as_str().to_string(),
            rank,
            security_id,
            underlying_id,
            // u32 -> i64 is lossless; no saturation is possible and none is
            // written, so a future widening of the field cannot hide here.
            volume: i64::from(contract.volume),
            gain_pct,
            subscribed: is_subscribed(contract.security_id, contract.segment),
        });
    }

    SnapshotProjection { rows, refusals }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(security_id: u64, underlying_id: u64, volume: u32) -> RankedContract {
        RankedContract {
            security_id,
            segment: ExchangeSegment::NseFno,
            underlying_id,
            volume,
            window_lots_milli: 0,
        }
    }

    #[test]
    fn floor_to_second_leaves_a_whole_second_untouched() {
        assert_eq!(floor_to_second(5 * NANOS_PER_SECOND), 5 * NANOS_PER_SECOND);
    }

    #[test]
    fn floor_to_second_truncates_a_sub_second_remainder_down() {
        // The whole point: two emits inside one second must land on the SAME
        // value or the DEDUP key stops collapsing them.
        let a = floor_to_second(5 * NANOS_PER_SECOND + 1);
        let b = floor_to_second(5 * NANOS_PER_SECOND + 999_999_999);
        assert_eq!(a, 5 * NANOS_PER_SECOND);
        assert_eq!(a, b);
    }

    #[test]
    fn floor_to_second_rounds_toward_negative_infinity_not_toward_zero() {
        // rem_euclid, not %. With `%` this would be 0 and two distinct
        // pre-epoch instants would share a grid point with a post-epoch one.
        assert_eq!(floor_to_second(-1), -NANOS_PER_SECOND);
    }

    #[test]
    fn project_snapshot_numbers_ranks_from_one_in_slice_order() {
        let ranked = [
            contract(10, 1, 500),
            contract(11, 1, 400),
            contract(12, 2, 300),
        ];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| 1.5,
            |_, _| false,
        );
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.rows[0].rank, 1);
        assert_eq!(p.rows[1].rank, 2);
        assert_eq!(p.rows[2].rank, 3);
        assert_eq!(p.rows[0].security_id, 10);
        assert_eq!(p.rows[2].security_id, 12);
    }

    #[test]
    fn project_snapshot_puts_every_row_of_one_snapshot_on_the_same_second() {
        let ranked = [contract(10, 1, 500), contract(11, 1, 400)];
        let p = project_snapshot(
            7 * NANOS_PER_SECOND + 123_456_789,
            SnapshotCadence::FiveSecond,
            OptionFamily::Index,
            &ranked,
            |_, _| 0.0,
            |_, _| true,
        );
        assert!(
            p.rows
                .iter()
                .all(|r| r.snapshot_ts_ist_nanos == 7 * NANOS_PER_SECOND)
        );
    }

    #[test]
    fn project_snapshot_carries_the_subscribed_flag_per_contract() {
        // The column that makes the table an audit rather than trivia: it
        // must reflect THIS contract, not a blanket value.
        let ranked = [contract(10, 1, 500), contract(11, 1, 400)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| 2.0,
            |sid, _| sid == 10,
        );
        assert!(p.rows[0].subscribed);
        assert!(!p.rows[1].subscribed);
    }

    #[test]
    fn project_snapshot_stamps_the_family_and_feed_labels() {
        let ranked = [contract(10, 1, 500)];
        let stock = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| 0.0,
            |_, _| false,
        );
        let index = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Index,
            &ranked,
            |_, _| 0.0,
            |_, _| false,
        );
        assert_eq!(stock.rows[0].family, "stock");
        assert_eq!(index.rows[0].family, "index");
        assert_eq!(stock.rows[0].feed, "dhan");
    }

    #[test]
    fn project_snapshot_refuses_an_id_that_would_wrap_the_signed_column() {
        // QuestDB LONG is signed. A namespace-banded id (Groww sets bit 62,
        // GDF bit 61) above i64::MAX would wrap NEGATIVE and be stored as a
        // different instrument entirely — a wrong row, not a missing one.
        let ranked = [contract(u64::MAX, 1, 500), contract(10, 1, 400)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| 0.0,
            |_, _| false,
        );
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].security_id, 10);
        assert_eq!(
            p.refusals,
            vec![(u64::MAX, SnapshotRefusal::IdTooLargeForSignedColumn)]
        );
    }

    #[test]
    fn project_snapshot_refuses_an_underlying_id_that_would_wrap_too() {
        // The contract id can be fine while the UNDERLYING id is not; both
        // are stored, so both must be checked.
        let ranked = [contract(10, u64::MAX, 500)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| 0.0,
            |_, _| false,
        );
        assert!(p.rows.is_empty());
        assert_eq!(p.refusal_count(), 1);
    }

    #[test]
    fn project_snapshot_refuses_a_nan_or_infinite_gain() {
        // The §28.4 poisoning class one table over: a NaN in a DOUBLE makes
        // every later comparison on the column silently false.
        let ranked = [
            contract(10, 1, 500),
            contract(11, 1, 400),
            contract(12, 1, 300),
        ];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |sid, _| match sid {
                10 => f64::NAN,
                11 => f64::INFINITY,
                _ => 3.25,
            },
            |_, _| false,
        );
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].security_id, 12);
        assert!((p.rows[0].gain_pct - 3.25).abs() < f64::EPSILON);
        assert_eq!(p.refusal_count(), 2);
        assert!(
            p.refusals
                .iter()
                .all(|(_, r)| *r == SnapshotRefusal::NonFiniteGain)
        );
    }

    #[test]
    fn project_snapshot_keeps_the_true_rank_and_leaves_a_visible_hole() {
        // The rank is the contract's real position by volume, so a refused
        // contract leaves a GAP and the survivor keeps rank 3. Renumbering it
        // to 2 would be tidier and would be a lie: it would claim a contract
        // was second-heaviest when it was third. The gap is the signal that a
        // row was refused, and `refusals` says which and why.
        //
        // (An earlier version of this test was NAMED "keeps ranks dense" while
        // asserting exactly this hole — the name said the opposite of the
        // assertion. Kept as a note because a test whose name contradicts its
        // body teaches the next reader something false, which is the same
        // failure this repository keeps recording in its rule files.)
        let ranked = [
            contract(10, 1, 500),
            contract(u64::MAX, 1, 400),
            contract(12, 1, 300),
        ];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| 0.0,
            |_, _| false,
        );
        let ranks: Vec<i64> = p.rows.iter().map(|r| r.rank).collect();
        assert_eq!(ranks, vec![1, 3]);
    }

    #[test]
    fn project_snapshot_widens_volume_without_loss() {
        let ranked = [contract(10, 1, u32::MAX)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| 0.0,
            |_, _| false,
        );
        assert_eq!(p.rows[0].volume, i64::from(u32::MAX));
    }

    #[test]
    fn project_snapshot_on_an_empty_ranking_writes_nothing_and_refuses_nothing() {
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &[],
            |_, _| 0.0,
            |_, _| false,
        );
        assert!(p.rows.is_empty());
        assert_eq!(p.refusal_count(), 0);
    }

    #[test]
    fn refusal_count_reports_every_dropped_contract() {
        // Two different refusal reasons in one snapshot: the count is the
        // caller's single number for "how much of this snapshot is missing",
        // so it must not report only the first kind it met.
        let ranked = [
            contract(u64::MAX, 1, 500),
            contract(10, 1, 400),
            contract(11, 1, 300),
        ];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |sid, _| if sid == 10 { f64::NAN } else { 1.0 },
            |_, _| false,
        );
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.refusal_count(), 2);
        assert_eq!(p.refusal_count(), p.refusals.len());
    }

    #[test]
    fn as_str_labels_are_distinct_for_every_refusal_reason() {
        let labels = [
            SnapshotRefusal::IdTooLargeForSignedColumn.as_str(),
            SnapshotRefusal::NonFiniteGain.as_str(),
            SnapshotRefusal::RankOutOfRange.as_str(),
        ];
        let mut sorted = labels;
        sorted.sort_unstable();
        sorted.iter().zip(sorted.iter().skip(1)).for_each(|(a, b)| {
            assert_ne!(a, b, "refusal labels must be distinct: {labels:?}");
        });
    }

    // -----------------------------------------------------------------------
    // The session-clock capture gate (2026-09-07)
    // -----------------------------------------------------------------------

    /// Seconds-of-day helper for readable fixtures.
    const fn ist(h: u32, m: u32, s: u32) -> u32 {
        h * 3600 + m * 60 + s
    }

    #[test]
    fn the_capture_window_opens_at_0915_and_not_before() {
        assert!(
            !within_capture_window(ist(9, 14, 59)),
            "09:14:59 is pre-open — one second before the window"
        );
        assert!(
            within_capture_window(ist(9, 15, 0)),
            "09:15:00 is INCLUSIVE: it is the operator's stated start"
        );
    }

    #[test]
    fn the_capture_window_closes_after_the_1539_minute() {
        assert!(
            within_capture_window(ist(15, 39, 59)),
            "15:39:59 must still capture — the operator's \"till 3.39 pm\" reads \
             as THROUGH the 15:39 minute, so the end is exclusive at 15:40:00"
        );
        assert!(
            !within_capture_window(ist(15, 40, 0)),
            "15:40:00 is EXCLUSIVE"
        );
    }

    #[test]
    fn the_pre_open_auction_window_never_captures() {
        // 09:00-09:15 ticks ARE legitimately persisted (tick persistence opens
        // at 09:00), and ranking them would rank an auction. This is the whole
        // reason the capture window starts LATER than the persist window.
        for secs in [ist(9, 0, 0), ist(9, 7, 0), ist(9, 12, 30)] {
            assert!(
                !within_capture_window(secs),
                "pre-open second {secs} must never produce a snapshot"
            );
        }
    }

    #[test]
    fn the_overnight_hours_never_capture() {
        // The case a VALUE-only guard cannot cover: a process spanning midnight
        // holds yesterday's non-zero volumes, so "volume is zero" is false and
        // an unguarded snapshot would publish a confident ranking of a market
        // that has not opened.
        for secs in [0, ist(3, 0, 0), ist(8, 59, 59), ist(20, 0, 0), 86_399] {
            assert!(
                !within_capture_window(secs),
                "out-of-session second {secs} must never produce a snapshot"
            );
        }
    }

    #[test]
    fn capture_never_outlives_the_ticks_that_feed_it() {
        // A ranking of volume can only be as current as the ticks behind it.
        // Capturing past the tick-persist window would publish a FROZEN
        // ranking that looks live, which is worse than publishing nothing.
        assert_eq!(
            tickvault_common::constants::TOP_VOLUME_CAPTURE_END_SECS_OF_DAY_IST,
            tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST,
            "the capture window must END with the tick window, not after it"
        );
    }

    #[test]
    fn secs_of_day_reads_ist_directly_and_never_adds_an_offset() {
        // THE most critical data-integrity rule in this repository: stored
        // timestamps on this path are ALREADY IST. Adding +5:30 here would be
        // the offset-applied-twice bug, and it would move every snapshot into
        // a window it does not belong to.
        let nine_fifteen = i64::from(ist(9, 15, 0)) * NANOS_PER_SECOND;
        assert_eq!(
            secs_of_day_ist(nine_fifteen),
            ist(9, 15, 0),
            "an IST instant must read back as the SAME seconds-of-day"
        );
    }

    #[test]
    fn secs_of_day_wraps_at_the_day_boundary_rather_than_growing() {
        let day = 86_400_i64 * NANOS_PER_SECOND;
        let second_day_noon = day + i64::from(ist(12, 0, 0)) * NANOS_PER_SECOND;
        assert_eq!(secs_of_day_ist(second_day_noon), ist(12, 0, 0));
        assert_eq!(secs_of_day_ist(day), 0, "midnight of any day is second 0");
    }

    #[test]
    fn secs_of_day_stays_in_range_for_a_pre_epoch_instant() {
        // Unreachable in production, and the kind of asymmetry nobody
        // re-derives later: a `%` that rounds toward zero would go NEGATIVE
        // and the cast would wrap into a bogus in-window second.
        let before_epoch = -NANOS_PER_SECOND;
        let of_day = secs_of_day_ist(before_epoch);
        assert!(of_day < 86_400, "must stay inside a day, got {of_day}");
        assert_eq!(of_day, 86_399, "one second before the epoch is 23:59:59");
    }
}
