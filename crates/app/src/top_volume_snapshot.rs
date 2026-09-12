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
//! | a non-finite `gain_pct` | the §28.4 NaN-poisoning class, one table over — a NaN written to `DOUBLE` makes every later comparison on the column false. **Since 2026-09-12 this refusal NULLs the column and KEEPS the row** — see [`SnapshotRefusal::GainUnavailable`] |
//! | a rank that does not fit `i64` | arithmetic that cannot happen at k ≤ 250, refused rather than wrapped, because a silently negative rank reads as a valid row |
//!
//! Each refusal is COUNTED. Three of them DROP the row, and must: they
//! protect a column the row is KEYED or ORDERED by, so a wrong value there is
//! stored as a different instrument or as the reason a contract ranked first.
//! The fourth, `GainUnavailable`, NULLs one leaf column and keeps everything
//! else — because dropping the row there threw away the contract's VOLUME,
//! the one thing this table exists to record, over a display column beside
//! it. Dropping one observability row costs no tick; writing a wrong one
//! costs the trust in the whole table; and dropping the RIGHT row for the
//! wrong reason costs the record itself.

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
    /// The UNDERLYING's percentage change was not knowable — no spot price
    /// yet, no previous close yet, or a value `eligible_gain_pct` refuses as
    /// implausible.
    ///
    /// # This one does NOT drop the row (2026-09-12)
    ///
    /// It is the only refusal whose column is neither in the DEDUP key nor in
    /// the ordering: the views select `gain_pct` with no arithmetic and
    /// nothing else reads it. Until today it `continue`d like the other three,
    /// so an underlying that had not yet printed took every one of its
    /// contracts' volume rows out of the table with it — worst at the open and
    /// after a mid-session redeploy, exactly when the record matters most.
    ///
    /// The name changed with the behaviour: it no longer describes a value
    /// that was non-finite, it describes a column that is NULL.
    GainUnavailable,
    /// The 1-based rank overflowed `i64` — unreachable at any real `k`, and
    /// refused anyway rather than wrapped.
    RankOutOfRange,
    /// `window_lots_milli` did not fit a signed 64-bit column. Unreachable at
    /// any real lot size (it would need ~9.2e15 milli-lots in one window) and
    /// refused rather than wrapped, because a negative rank key stored as the
    /// reason a contract ranked first is worse than an absent row.
    LotsOutOfRange,
}

impl SnapshotRefusal {
    /// Every reason, so the caller can PRE-REGISTER one counter series per
    /// label at zero.
    ///
    /// The Prometheus exporter renders no series it has never seen, and each
    /// LABEL VALUE is its own series — so an unseeded reason is simply absent
    /// from `/metrics` until its first occurrence, which is the one moment an
    /// operator needs it to already be there. Same reasoning as
    /// `pre_register_gainer_filter_counter`.
    pub const ALL: [Self; 4] = [
        Self::IdTooLargeForSignedColumn,
        Self::GainUnavailable,
        Self::RankOutOfRange,
        Self::LotsOutOfRange,
    ];

    /// Position in [`Self::ALL`], so a caller can hold one pre-resolved
    /// counter handle per reason in a fixed array rather than building a
    /// labelled counter per refusal.
    ///
    /// That is not tidiness: `metrics::counter!(NAME, "reason" => label)` with
    /// a non-literal label drops to the ALLOCATING arm, which is the
    /// `record_ws_lag` class this repository measured at ~36M allocations an
    /// hour. Four reasons x four cadences x two families over a population the
    /// 2026-09-12 cut removal made market-bounded is not a path to allocate on.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::IdTooLargeForSignedColumn => 0,
            Self::GainUnavailable => 1,
            Self::RankOutOfRange => 2,
            Self::LotsOutOfRange => 3,
        }
    }

    /// Stable label for the refusal counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdTooLargeForSignedColumn => "id_too_large",
            Self::GainUnavailable => "gain_unavailable",
            Self::RankOutOfRange => "rank_out_of_range",
            Self::LotsOutOfRange => "lots_out_of_range",
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
/// `saturating_sub` rather than `-`: `rem_euclid` is NON-NEGATIVE by
/// definition, so at `i64::MIN` the subtraction underflows. The release
/// profile sets `overflow-checks = true` with `panic = "abort"`, which makes
/// that a process death rather than a wrong timestamp — on a function that is
/// `pub const` and therefore callable from anywhere. Saturating costs one
/// instruction and is exact for every reachable input.
#[must_use]
pub const fn floor_to_second(ts_nanos: i64) -> i64 {
    let rem = ts_nanos.rem_euclid(NANOS_PER_SECOND);
    ts_nanos.saturating_sub(rem)
}

/// Truncates a timestamp DOWN to its cadence grid cell, on the 09:00 anchor.
///
/// The companion of [`nanos_to_next_grid_boundary`]: that one says when the
/// next boundary IS, this one says which boundary a given instant belongs to.
/// Together they make a snapshot's timestamp a property of the window rather
/// than of when the scheduler happened to run the arm — which is what lets a
/// `top_volume` row and a `candles_<tf>` row share a `ts` and be joined.
///
/// `period_secs == 1` reduces to [`floor_to_second`], and a zero period is
/// handled rather than divided by.
#[must_use]
pub const fn floor_to_grid(ts_nanos: i64, period_secs: u64) -> i64 {
    if period_secs <= 1 {
        return floor_to_second(ts_nanos);
    }
    // `period_secs` is `SnapshotCadence::interval_secs`, whose largest value is
    // 60. `i64::try_from` is not `const`.
    // APPROVED: 60 cannot wrap i64; try_from is not const.
    #[allow(clippy::cast_possible_wrap)]
    let period = period_secs as i64;
    const SECS_PER_DAY: i64 = 86_400;
    let secs = ts_nanos.div_euclid(NANOS_PER_SECOND);
    let day_start = secs.div_euclid(SECS_PER_DAY) * SECS_PER_DAY;
    let anchor = day_start + SNAPSHOT_GRID_ANCHOR_SECS_OF_DAY_IST;
    // `rem_euclid` so an instant BEFORE the 09:00 anchor lands on the same grid
    // running backwards, rather than on a negative remainder that would floor
    // it UPWARD into a cell that has not happened.
    let rem = (secs - anchor).rem_euclid(period);
    // Saturating for the same reason `floor_to_second` saturates: release is
    // `overflow-checks = true` with `panic = "abort"`, and this is `pub const`.
    let cell_secs = secs.saturating_sub(rem);
    match cell_secs.checked_mul(NANOS_PER_SECOND) {
        Some(nanos) => nanos,
        // Unreachable through `now_ist_nanos()`, and degrading to the plain
        // second beats aborting the drain.
        None => floor_to_second(ts_nanos),
    }
}

/// The IST second-of-day the candle grid is anchored on: 09:00.
///
/// MIRRORS `tickvault_trading::candles::tf_index::CANDLE_SESSION_OPEN_SECS_OF_DAY_IST`,
/// which is `pub(crate)` there and so cannot be imported. Pinned to the same
/// literal by `the_snapshot_grid_anchor_matches_the_candle_grid_anchor`, for
/// the reason the whole alignment exists: a snapshot that lands on a different
/// grid from the candles cannot be joined to them, and the failure is silent —
/// the query simply returns fewer rows.
pub const SNAPSHOT_GRID_ANCHOR_SECS_OF_DAY_IST: i64 = 32_400;

/// Nanoseconds from `now` to the NEXT boundary of a `period_secs` grid.
///
/// # Why a grid at all
///
/// Until 2026-09-12 the four cadence timers were built with
/// `tokio::time::interval(period)`, whose first tick resolves IMMEDIATELY —
/// so the grid was anchored on whatever instant the drain happened to start,
/// and every later fire inherited that offset plus accumulated scheduling
/// drift. A 5-second sweep firing at 09:20:07.3 stamped 09:20:07, which is on
/// no 5-second grid point at all. Rows a query cannot line up on a boundary
/// cannot be joined to the candle of the same window, and nothing about the
/// table says so.
///
/// # Why the anchor is 09:00 and not the epoch
///
/// The candle grid rebases every day at 09:00 IST
/// (`TfIndex::bucket_start`: `session_open + ((t - session_open) / secs) * secs`),
/// NOT at midnight. For the four cadences shipped today — 1 s, 3 s, 5 s, 1 m —
/// both 86,400 and 32,400 are divisible by every period, so an epoch modulo
/// would give the identical answer and the distinction looks academic. It is
/// not: a fifth cadence with `32_400 % period != 0` (7 s, 45 s, 90 s) would
/// silently land on a grid the candles never touch. Anchoring on the session
/// open makes that unrepresentable instead of merely unlikely.
///
/// # The return is always > 0
///
/// A boundary EXACTLY at `now` returns a full period, never zero. `interval_at`
/// with a start instant already in the past fires immediately, and an immediate
/// fire measures a zero-length window — one row claiming a window that did not
/// happen.
#[must_use]
pub const fn nanos_to_next_grid_boundary(now_ist_nanos: i64, period_secs: u64) -> u64 {
    // A zero period is not reachable (`TOP_VOLUME_SNAPSHOT_INTERVALS` is built
    // from `interval_secs`, and `tokio::time::interval` panics on zero anyway),
    // and is handled rather than divided by.
    if period_secs == 0 {
        return 0;
    }
    // `period_secs` is `SnapshotCadence::interval_secs`, whose largest value is
    // 60 -- eighteen orders of magnitude below `i64::MAX`, so the wrap this
    // lint guards cannot occur. `i64::try_from` is not `const`, and this must
    // stay `const` so the grid arithmetic is testable without a runtime.
    // APPROVED: 60 cannot wrap i64; try_from is not const.
    #[allow(clippy::cast_possible_wrap)]
    let period = period_secs as i64;
    const SECS_PER_DAY: i64 = 86_400;
    let secs = now_ist_nanos.div_euclid(NANOS_PER_SECOND);
    // `div_euclid` so a pre-epoch instant floors DOWN rather than toward zero.
    let day_start = secs.div_euclid(SECS_PER_DAY) * SECS_PER_DAY;
    let anchor = day_start + SNAPSHOT_GRID_ANCHOR_SECS_OF_DAY_IST;
    // `rem_euclid` so an instant BEFORE 09:00 — the whole pre-open, when the
    // drain actually starts — lands on the same grid running backwards rather
    // than on a negative remainder that would push the boundary into the past.
    let rem = (secs - anchor).rem_euclid(period);
    let boundary_secs = secs + (period - rem);
    // CHECKED, and it is the only multiply in this function that can leave the
    // i64 range. `boundary_secs` is up to one period past `now / 1e9`, so at a
    // clock near `i64::MAX` the product exceeds `i64::MAX` and the release
    // profile — `overflow-checks = true`, `panic = "abort"` — turns that into a
    // PROCESS DEATH on the frame drain rather than a wrong delay.
    //
    // Not reachable today: `now_ist_nanos()` is `timestamp_nanos_opt()`, whose
    // `Some` range ends 2262-04-11. It is checked anyway because this is
    // `pub const`, because every other step here already documents its
    // `div_euclid`/`rem_euclid`/sign-loss reasoning, and because the fallback
    // is free: a zero delay makes `interval_at` fire immediately, which is the
    // UNALIGNED behaviour this function replaced — degraded, never fatal.
    let boundary_nanos = match boundary_secs.checked_mul(NANOS_PER_SECOND) {
        Some(nanos) => nanos,
        None => return 0,
    };
    // Strictly positive: `period - rem` is at least 1 whole second and the
    // sub-second part of `now` is under one second.
    let delta = boundary_nanos - now_ist_nanos;
    // `delta` is proven STRICTLY POSITIVE two lines up -- `period - rem` is at
    // least one whole second and the sub-second part of `now` is under one
    // second -- and bounded above by one period (60 s), so there is no sign to
    // lose. `u64::try_from` is not `const`.
    // APPROVED: proven strictly positive above; try_from is not const.
    #[allow(clippy::cast_sign_loss)]
    let out = delta as u64;
    out
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
/// `gain_pct_of` is called with the **UNDERLYING's** id, not the contract's.
/// That is the whole point of the 2026-09-09 fix: it used to be called with
/// `(contract.security_id, contract.segment)`, and nothing has ever written a
/// previous close for an `NSE_FNO` contract, so the probe returned `None` on
/// every row, became `NaN`, and each row was refused as `NonFiniteGain` —
/// **the table was empty for the whole session while every counter read
/// healthy.** Handing the closure the underlying id makes the wrong lookup
/// unrepresentable rather than merely corrected.
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
    G: Fn(u64) -> Option<f64>,
    S: Fn(u64, ExchangeSegment) -> bool,
{
    // FLOORED TO THE CADENCE GRID, not merely to the second.
    //
    // # The stall that put a row where no candle can see it (2026-09-12)
    //
    // The stamp is `now` at the instant the timer arm RUNS, and a timer is not
    // a promise about when that is. `MissedTickBehavior::Skip` re-snaps the
    // NEXT deadline to the grid, but it resolves the currently-pending tick
    // IMMEDIATELY when the stall ends — so after any pause longer than one
    // period (a QuestDB stall, an IO hiccup) one fire executes at an arbitrary
    // instant. `floor_to_second` then stamped it on an arbitrary second, and
    // for the 3s, 5s and 1m cadences an arbitrary second is a row no candle
    // row shares a timestamp with. That is the silent failure the grid
    // alignment exists to prevent, surviving inside the fix for it.
    //
    // Flooring to the cadence grid makes the stamp a property of the WINDOW
    // rather than of the scheduler. Two consecutive fires cannot collide into
    // one grid cell — `Skip` guarantees the next deadline is a fresh boundary,
    // and a late fire floors to the cell it belongs to — so the DEDUP key
    // stays one row per contract per snapshot.
    //
    // For the 1-second cadence this is exactly `floor_to_second`, so nothing
    // about that board changes.
    let ts = floor_to_grid(snapshot_ts_ist_nanos, cadence.interval_secs());
    let mut rows = Vec::with_capacity(ranked.len());
    // PRE-SIZED like `rows`, and for a reason the empty `Vec::new()` it
    // replaces got backwards: the refusal that dominates this buffer is
    // `GainUnavailable`, and that one is the EXPECTED state near the open —
    // before a spot price and a previous close exist for an underlying, EVERY
    // row refuses. `Vec::new()` grows geometrically, so the common pre-open
    // case paid ~15 reallocations and memcpys per family per sweep, on the
    // frame drain. One allocation of a buffer that is usually empty by 09:20
    // is the cheaper side of that trade, and it makes the projection's
    // allocation count a constant 2 rather than a function of the market.
    let mut refusals = Vec::with_capacity(ranked.len());

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

        let Ok(window_lots_milli) = i64::try_from(contract.window_lots_milli) else {
            refusals.push((contract.security_id, SnapshotRefusal::LotsOutOfRange));
            continue;
        };

        // The UNDERLYING's move, keyed on the underlying — see the fn doc.
        //
        // The `is_finite` filter SURVIVES the closure becoming an `Option`,
        // and that is deliberate rather than belt-and-braces: the signature
        // permits `Some(NaN)`, and a future caller that computes a gain
        // in-line rather than through `eligible_gain_pct` would hand one over.
        // A NaN must never reach `column_f64`, so it is mapped to `None` here
        // and counted under the same reason — the column is NULL either way,
        // and the row LIVES either way.
        let gain_pct = gain_pct_of(contract.underlying_id).filter(|g| g.is_finite());
        if gain_pct.is_none() {
            refusals.push((contract.security_id, SnapshotRefusal::GainUnavailable));
        }

        rows.push(TopVolumeRankRow {
            snapshot_ts_ist_nanos: ts,
            cadence,
            family: family.as_str(),
            feed: SNAPSHOT_FEED,
            segment: contract.segment.as_str(),
            rank,
            security_id,
            underlying_id,
            // u32 -> i64 is lossless; no saturation is possible and none is
            // written, so a future widening of the field cannot hide here.
            volume: i64::from(contract.volume),
            // The two inputs of the rank division, both `u32` at the source,
            // so `i64::from` is lossless exactly as `volume` above is. They
            // need no refusal arm of their own: neither can overflow, and a
            // `lot_size` of 0 is already impossible here — `rank` only reaches
            // this projection for contracts whose `window_lots_milli` divided,
            // and `window_lots_milli` returns `None` on a zero lot.
            delta_units: i64::from(contract.delta_units),
            lot_size: i64::from(contract.lot_size),
            window_lots_milli,
            gain_pct,
            subscribed: is_subscribed(contract.security_id, contract.segment),
        });
    }

    SnapshotProjection { rows, refusals }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Like [`contract`], naming the underlying explicitly for the tests that
    /// exercise the gain closure — which is keyed on the UNDERLYING.
    fn contract_under(security_id: u64, underlying_id: u64, volume: u32) -> RankedContract {
        contract(security_id, underlying_id, volume)
    }

    fn contract(security_id: u64, underlying_id: u64, volume: u32) -> RankedContract {
        RankedContract {
            security_id,
            segment: ExchangeSegment::NseFno,
            underlying_id,
            volume,
            // A rank key distinct from `volume`, so a test that asserts the
            // stored key cannot pass by accidentally reading the volume.
            window_lots_milli: u64::from(volume) * 3,
            // Self-consistent with the key above: `delta * 1000 / 1000` is
            // `delta`, so `delta_units = volume * 3` reproduces exactly the
            // `window_lots_milli` beside it. Both are distinct from `volume`
            // itself, so a test asserting either cannot pass by reading the
            // wrong field.
            //
            // `saturating_mul` because one test deliberately feeds `u32::MAX`
            // to prove the volume widening is lossless; above `u32::MAX / 3`
            // this fixture saturates and the reproduce-the-key property no
            // longer holds — which is a property of the FIXTURE's arithmetic,
            // not of the projection, and no test asserts it up there.
            delta_units: volume.saturating_mul(3),
            lot_size: 1_000,
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
            |_| Some(1.5),
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
        let fired_at = 7 * NANOS_PER_SECOND + 123_456_789;
        let p = project_snapshot(
            fired_at,
            SnapshotCadence::FiveSecond,
            OptionFamily::Index,
            &ranked,
            |_| Some(0.0),
            |_, _| true,
        );
        // ONE stamp for the whole snapshot — the property this test is named
        // for, and the reason the DEDUP key can collapse a re-emit.
        let stamp = p.rows[0].snapshot_ts_ist_nanos;
        assert!(p.rows.iter().all(|r| r.snapshot_ts_ist_nanos == stamp));

        // UPDATED 2026-09-12: that stamp is the CADENCE GRID cell, not merely
        // the whole second the arm ran in. It used to assert `7s` here, which
        // was the sub-second truncation of the fire instant; a 5-second board
        // now floors to its own grid so a late fire cannot land on a second no
        // candle row shares. The sub-second part is still gone, which is what
        // the DEDUP key needs.
        assert_eq!(stamp, floor_to_grid(fired_at, 5));
        assert_eq!(
            stamp % NANOS_PER_SECOND,
            0,
            "a snapshot stamp must never carry a sub-second remainder"
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
            |_| Some(2.0),
            |sid, _| sid == 10,
        );
        assert!(p.rows[0].subscribed);
        assert!(!p.rows[1].subscribed);
    }

    /// The projection carries the rank division's two INPUTS, not just its
    /// result — and carries them per contract, never a blanket value.
    #[test]
    fn project_snapshot_carries_the_rank_inputs_per_contract() {
        let ranked = [contract(10, 1, 500), contract(11, 1, 400)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_| Some(2.0),
            |_, _| true,
        );
        assert_eq!(p.rows.len(), 2);

        for (row, src) in p.rows.iter().zip(ranked.iter()) {
            assert_eq!(row.delta_units, i64::from(src.delta_units));
            assert_eq!(row.lot_size, i64::from(src.lot_size));
            // The stored trio must close on its own arithmetic, or the column
            // that exists to make the ordering checkable cannot check it.
            assert_eq!(
                row.delta_units * 1_000 / row.lot_size,
                row.window_lots_milli
            );
        }

        // Per contract, not blanket: the two rows differ.
        assert_ne!(p.rows[0].delta_units, p.rows[1].delta_units);
        // And neither is the cumulative volume, which is a separate column.
        assert_ne!(p.rows[0].delta_units, p.rows[0].volume);
    }

    #[test]
    fn project_snapshot_stamps_the_family_and_feed_labels() {
        let ranked = [contract(10, 1, 500)];
        let stock = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_| Some(0.0),
            |_, _| false,
        );
        let index = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Index,
            &ranked,
            |_| Some(0.0),
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
            |_| Some(0.0),
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
            |_| Some(0.0),
            |_, _| false,
        );
        assert!(p.rows.is_empty());
        assert_eq!(p.refusal_count(), 1);
    }

    #[test]
    fn project_snapshot_refuses_a_nan_or_infinite_gain() {
        // The §28.4 poisoning class one table over: a NaN in a DOUBLE makes
        // every later comparison on the column silently false.
        // Distinct UNDERLYINGS, because the gain closure is keyed on the
        // underlying id since 2026-09-09 — three strikes of one stock share
        // one gain by construction and could not be told apart here.
        let ranked = [
            contract_under(10, 91, 500),
            contract_under(11, 92, 400),
            contract_under(12, 93, 300),
        ];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |underlying_id| match underlying_id {
                // A caller that computes a gain in-line rather than through
                // `eligible_gain_pct` CAN hand over a `Some(NaN)`. The
                // signature permits it, so the projection must still refuse
                // it -- these two arms are that case, not dead weight.
                91 => Some(f64::NAN),
                92 => Some(f64::INFINITY),
                _ => Some(3.25),
            },
            |_, _| false,
        );
        // ALL THREE rows survive. Until 2026-09-12 this asserted ONE, because
        // a non-finite gain deleted the whole row -- taking the contract's
        // volume, the column the table exists for, out with a display column
        // beside it.
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.rows[0].security_id, 10);
        assert_eq!(p.rows[1].security_id, 11);
        assert_eq!(p.rows[2].security_id, 12);
        assert_eq!(p.rows[0].gain_pct, None, "NaN is NULLed, not stored");
        assert_eq!(p.rows[1].gain_pct, None, "Inf is NULLed, not stored");
        assert!(
            p.rows[2]
                .gain_pct
                .is_some_and(|g| (g - 3.25).abs() < f64::EPSILON)
        );
        // Still COUNTED -- the column being NULL is a fact an operator can
        // read, and the counter is what says how often it happens.
        assert_eq!(p.refusal_count(), 2);
        assert!(
            p.refusals
                .iter()
                .all(|(_, r)| *r == SnapshotRefusal::GainUnavailable)
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
            |_| Some(0.0),
            |_, _| false,
        );
        let ranks: Vec<i64> = p.rows.iter().map(|r| r.rank).collect();
        assert_eq!(ranks, vec![1, 3]);
    }

    #[test]
    fn project_snapshot_stores_the_rank_key_not_only_the_cumulative_volume() {
        // The sort key has been lots-in-window since 2026-09-07, and until
        // 2026-09-09 the table stored ONLY cumulative `volume` — so a reader
        // could see rank 1 holding less volume than rank 40 with nothing in
        // the row to explain the order. This asserts the number the ordering
        // was actually computed from is on the row.
        let ranked = [contract(10, 91, 500), contract(11, 92, 400)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_| Some(1.0),
            |_, _| false,
        );
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[0].window_lots_milli, 1_500);
        assert_eq!(p.rows[1].window_lots_milli, 1_200);
        // And it is genuinely a different column from `volume`.
        assert_ne!(p.rows[0].window_lots_milli, p.rows[0].volume);
    }

    #[test]
    fn project_snapshot_refuses_a_rank_key_that_would_wrap_the_signed_column() {
        let mut c = contract(10, 91, 500);
        c.window_lots_milli = u64::MAX;
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &[c],
            |_| Some(1.0),
            |_, _| false,
        );
        assert!(p.rows.is_empty());
        assert_eq!(p.refusals, vec![(10, SnapshotRefusal::LotsOutOfRange)]);
    }

    #[test]
    fn project_snapshot_asks_the_gain_closure_for_the_underlying_not_the_contract() {
        // The 2026-09-09 defect in one assertion: the closure used to be
        // handed the CONTRACT id and segment, and no previous close is ever
        // stored for an NSE_FNO contract, so every row was refused as
        // a non-finite gain and the table stayed empty all session.
        let ranked = [contract(4_431, 2_885, 500)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |id| {
                assert_eq!(id, 2_885, "the gain closure must receive the UNDERLYING id");
                Some(7.5)
            },
            |_, _| false,
        );
        assert_eq!(p.rows.len(), 1);
        assert!(
            p.rows[0]
                .gain_pct
                .is_some_and(|g| (g - 7.5).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn project_snapshot_widens_volume_without_loss() {
        let ranked = [contract(10, 1, u32::MAX)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_| Some(0.0),
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
            |_| Some(0.0),
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
            contract_under(u64::MAX, 91, 500),
            contract_under(10, 92, 400),
            contract_under(11, 93, 300),
        ];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |underlying_id| {
                if underlying_id == 92 { None } else { Some(1.0) }
            },
            |_, _| false,
        );
        // TWO rows, not one: the oversized id DROPS its row (that column is in
        // the DEDUP key), the unknown gain KEEPS its row with a NULL column.
        // Both are counted, and the count is still the caller's single number
        // for "how much of this snapshot needed a refusal".
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[0].security_id, 10);
        assert_eq!(p.rows[0].gain_pct, None);
        assert_eq!(p.rows[1].security_id, 11);
        assert_eq!(p.refusal_count(), 2);
        assert_eq!(p.refusal_count(), p.refusals.len());
    }

    /// The bite for the 2026-09-12 fix: an underlying whose gain is simply
    /// not known yet must NOT cost the contract its volume row.
    ///
    /// `None` rather than `Some(NaN)` on purpose -- this is the shape the
    /// PRODUCTION caller produces (`underlying_gain_pct` returns `Option`, and
    /// the `.unwrap_or(f64::NAN)` that used to flatten it is gone). The NaN
    /// path is a separate test because it is a separate hazard.
    #[test]
    fn a_row_with_an_unknown_gain_still_carries_its_volume() {
        let ranked = [contract(4_431, 2_885, 987_654)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            // Neither store has heard from this underlying yet -- the ordinary
            // state at the open, and after any mid-session restart.
            |_| None,
            |_, _| true,
        );
        assert_eq!(
            p.rows.len(),
            1,
            "an unknown gain must NULL one column, never delete the row"
        );
        assert_eq!(p.rows[0].security_id, 4_431);
        assert_eq!(p.rows[0].volume, 987_654);
        assert_eq!(p.rows[0].rank, 1);
        assert!(p.rows[0].subscribed);
        assert_eq!(p.rows[0].gain_pct, None);
        // Counted, because a NULL nobody counts is a NULL nobody notices.
        assert_eq!(
            p.refusals,
            vec![(4_431, SnapshotRefusal::GainUnavailable)],
            "the NULL must still be attributable to a reason"
        );
    }

    /// The three refusals that guard a KEY or ORDERING column must still
    /// delete the row. Only the gain changed on 2026-09-12.
    #[test]
    fn an_id_or_rank_or_lots_refusal_still_deletes_the_row() {
        let oversized = [contract(u64::MAX, 2_885, 500)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &oversized,
            |_| Some(1.0),
            |_, _| false,
        );
        assert!(
            p.rows.is_empty(),
            "an id that would wrap the signed column is stored as a DIFFERENT \
             instrument -- that row must never be written"
        );
        assert_eq!(p.refusal_count(), 1);
    }

    /// `index()` must agree with `ALL`, or a caller's pre-resolved counter
    /// array attributes a refusal to the wrong reason -- silently, since both
    /// are valid slots.
    #[test]
    fn every_refusal_index_is_its_own_position_in_all() {
        for (slot, reason) in SnapshotRefusal::ALL.iter().enumerate() {
            assert_eq!(
                reason.index(),
                slot,
                "{reason:?} reports slot {} but ALL lists it at {slot}",
                reason.index()
            );
        }
        // And ALL is complete: an array sized from its length must have a slot
        // for every reason the projection can produce.
        assert_eq!(SnapshotRefusal::ALL.len(), 4);
    }

    /// Our grid anchor must be the CANDLE grid anchor, or the two tables sit
    /// on grids that never touch and the join silently returns fewer rows.
    #[test]
    fn the_snapshot_grid_anchor_matches_the_candle_grid_anchor() {
        // `tf_index::CANDLE_SESSION_OPEN_SECS_OF_DAY_IST` is `pub(crate)` in
        // the trading crate, so this pins the same literal that file's own
        // test pins. 09:00 IST, NOT 09:15 and NOT midnight.
        assert_eq!(SNAPSHOT_GRID_ANCHOR_SECS_OF_DAY_IST, 32_400);
    }

    /// Every fire must land on a multiple of its period from 09:00.
    ///
    /// This is the bite for the 2026-09-12 alignment. Before it the timers
    /// were `tokio::time::interval(period)`, whose first tick resolves
    /// immediately, so the grid was anchored on the drain's start instant and
    /// a 5-second sweep could stamp ...:02, ...:07, ...:12 forever.
    #[test]
    fn the_next_boundary_is_always_on_the_session_anchored_grid() {
        const DAY: i64 = 86_400;
        // A Friday, 09:20:07.312 IST -- deliberately off every grid.
        let day_start = 1_757_000_000_i64.div_euclid(DAY) * DAY;
        let now = (day_start + 9 * 3_600 + 20 * 60 + 7) * NANOS_PER_SECOND + 312_000_000;

        for period in [1_u64, 3, 5, 60] {
            let delta = nanos_to_next_grid_boundary(now, period);
            assert!(
                delta > 0,
                "period {period}: a boundary must be in the FUTURE"
            );
            let boundary = now + i64::try_from(delta).expect("bounded by one period");
            assert_eq!(
                boundary % NANOS_PER_SECOND,
                0,
                "period {period}: a boundary must be a whole second"
            );
            let secs_since_anchor =
                boundary / NANOS_PER_SECOND - (day_start + SNAPSHOT_GRID_ANCHOR_SECS_OF_DAY_IST);
            assert_eq!(
                secs_since_anchor.rem_euclid(i64::try_from(period).expect("small")),
                0,
                "period {period}: boundary is not a multiple of the period from 09:00"
            );
            assert!(
                delta <= period * 1_000_000_000,
                "period {period}: the wait must never exceed one period"
            );
        }
    }

    /// A boundary EXACTLY at `now` must return a FULL period, never zero.
    ///
    /// `interval_at` with a start instant already in the past fires
    /// immediately, and an immediate fire measures a zero-length window -- one
    /// row claiming a window that did not happen.
    #[test]
    fn a_boundary_exactly_now_waits_a_whole_period_rather_than_firing_twice() {
        const DAY: i64 = 86_400;
        let day_start = 1_757_000_000_i64.div_euclid(DAY) * DAY;
        // Exactly 09:20:00.000 -- on the grid for all four periods.
        let on_grid = (day_start + 9 * 3_600 + 20 * 60) * NANOS_PER_SECOND;
        for period in [1_u64, 3, 5, 60] {
            assert_eq!(
                nanos_to_next_grid_boundary(on_grid, period),
                period * 1_000_000_000,
                "period {period}: an on-grid instant must wait a FULL period"
            );
        }
    }

    /// The drain starts in the PRE-OPEN, before the 09:00 anchor. A naive
    /// remainder would go negative there and put the boundary in the past.
    #[test]
    fn a_pre_open_start_still_lands_on_the_same_grid() {
        const DAY: i64 = 86_400;
        let day_start = 1_757_000_000_i64.div_euclid(DAY) * DAY;
        // 08:31:02.4 IST -- the ordinary drain start, 29 minutes before the
        // anchor.
        let now = (day_start + 8 * 3_600 + 31 * 60 + 2) * NANOS_PER_SECOND + 400_000_000;
        for period in [1_u64, 3, 5, 60] {
            let delta = nanos_to_next_grid_boundary(now, period);
            assert!(delta > 0, "period {period}");
            let boundary_secs = (now + i64::try_from(delta).expect("bounded")) / NANOS_PER_SECOND;
            let since_anchor = boundary_secs - (day_start + SNAPSHOT_GRID_ANCHOR_SECS_OF_DAY_IST);
            assert_eq!(
                since_anchor.rem_euclid(i64::try_from(period).expect("small")),
                0,
                "period {period}: a pre-open start must use the same grid, \
                 running backwards from 09:00"
            );
        }
    }

    /// A zero period is unreachable in production and must not divide by zero.
    #[test]
    fn a_zero_period_returns_zero_rather_than_dividing() {
        assert_eq!(nanos_to_next_grid_boundary(NANOS_PER_SECOND, 0), 0);
    }

    /// A LATE fire still stamps the row on the grid a candle can be joined to.
    ///
    /// # The permutation this closes
    ///
    /// `MissedTickBehavior::Skip` re-snaps the NEXT deadline to the grid, but
    /// it resolves the currently-pending tick immediately when a stall ends. So
    /// one fire per stall per cadence executes at an arbitrary instant. Stamped
    /// with `floor_to_second` that row landed on an arbitrary second — for the
    /// 3s/5s/1m boards, a second no candle row shares, making the row invisible
    /// to the join the whole grid alignment exists to serve.
    #[test]
    fn a_late_fire_still_lands_on_the_cadence_grid() {
        // 09:00:00 IST on an arbitrary day, as the anchor sees it.
        let anchor = SNAPSHOT_GRID_ANCHOR_SECS_OF_DAY_IST * NANOS_PER_SECOND;
        for period in [3_u64, 5, 60] {
            let p = i64::try_from(period).expect("small");
            // A fire that should have happened at the boundary one period in,
            // but actually ran 2.5 s late because the drain was stalled.
            let boundary = anchor + p * NANOS_PER_SECOND;
            let late = boundary + 2_500_000_000;
            assert_eq!(
                floor_to_grid(late, period),
                boundary,
                "a fire {period}s-cadence late must be stamped on the boundary \
                 it belongs to, not on the second it happened to run in"
            );
            // And the NEXT fire lands on the following cell, so two fires can
            // never collapse onto one DEDUP key.
            let next = boundary + p * NANOS_PER_SECOND;
            assert_ne!(
                floor_to_grid(next, period),
                floor_to_grid(late, period),
                "consecutive fires must stay in distinct grid cells"
            );
        }
        // The 1-second board is unchanged by construction.
        let t = anchor + 7 * NANOS_PER_SECOND + 123_456_789;
        assert_eq!(floor_to_grid(t, 1), floor_to_second(t));
    }

    /// An extreme clock DEGRADES; it does not abort the process.
    ///
    /// The release profile is `overflow-checks = true` with `panic = "abort"`,
    /// so an unchecked multiply at `i64::MAX` would kill the frame drain rather
    /// than return a wrong number. That input is not reachable through
    /// `now_ist_nanos()` — `timestamp_nanos_opt()` stops at 2262-04-11 — but
    /// both of these are `pub const`, and the whole point of the checks is that
    /// nobody has to re-derive reachability before calling them.
    ///
    /// In debug (where these tests run) an overflow panics, so a regression
    /// here fails loudly rather than silently wrapping.
    #[test]
    fn the_extremes_of_the_clock_degrade_instead_of_aborting() {
        for period in [1_u64, 3, 5, 60] {
            // Far future: the boundary in nanos leaves the i64 range, so the
            // function falls back to "fire now" rather than multiplying.
            assert_eq!(
                nanos_to_next_grid_boundary(i64::MAX, period),
                0,
                "i64::MAX must fall back, not overflow (period {period})"
            );
            // Far past: no overflow, and the answer must still be a real delay
            // on the grid rather than zero or a wrap.
            let past = nanos_to_next_grid_boundary(i64::MIN, period);
            assert!(
                past > 0 && past <= period * NANOS_PER_SECOND as u64,
                "i64::MIN must yield a bounded positive delay, got {past} (period {period})"
            );
        }

        // `floor_to_second` saturates rather than underflowing: `rem_euclid` is
        // non-negative, so `i64::MIN - rem` would go below the range.
        assert_eq!(floor_to_second(i64::MIN), i64::MIN);
        // And it is still exact everywhere it is actually used.
        assert_eq!(floor_to_second(1_500_000_000), 1_000_000_000);
    }

    #[test]
    fn as_str_labels_are_distinct_for_every_refusal_reason() {
        let labels = [
            SnapshotRefusal::IdTooLargeForSignedColumn.as_str(),
            SnapshotRefusal::GainUnavailable.as_str(),
            SnapshotRefusal::RankOutOfRange.as_str(),
            SnapshotRefusal::LotsOutOfRange.as_str(),
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
