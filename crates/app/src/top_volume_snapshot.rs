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
//! ## The three things it refuses, and why each is a real loss if it does not
//!
//! | Refusal | What it prevents |
//! |---|---|
//! | a snapshot timestamp not on a whole second | the row's own DEDUP key stops collapsing a re-emit, so one contract holds several rows for one snapshot |
//! | a `security_id` or `underlying_id` above `i64::MAX` | QuestDB `LONG` is signed; a namespace-banded id would WRAP to a negative and be stored as a different instrument |
//! | volume arithmetic that will not fit a signed 64-bit column | a WRAPPED lot count or percentage is stored as the reason a contract ranked first |
//! | no human label for the contract | the row is WRITTEN with `unmapped` and counted — the label is a display column, and dropping the volume row over it would repeat the `gain_pct` mistake recorded below |
//!
//! Each refusal is COUNTED. Three of them DROP the row, and must: they
//! protect a column the row is KEYED or ORDERED by, so a wrong value there is
//! stored as a different instrument or as the reason a contract ranked first.
//! The fourth, `LabelUnavailable`, NULLs one leaf column and keeps everything
//! else — because dropping the row there threw away the contract's VOLUME,
//! the one thing this table exists to record, over a display column beside
//! it. Dropping one observability row costs no tick; writing a wrong one
//! costs the trust in the whole table; and dropping the RIGHT row for the
//! wrong reason costs the record itself.
//!
//! ⚠ The heading says THREE and the table lists FOUR rows, because the first
//! is a refusal of the SNAPSHOT (checked once, before any row is built) and
//! the other three are refusals of a ROW. That is the shape the counter has:
//! [`SnapshotRefusal`] carries the three per-row reasons only.
//!
//! A FOURTH per-row reason, `GainUnavailable`, was deleted on 2026-09-19 with
//! the `gain_pct` column it guarded (operator: *"See as of now I believe we
//! don't need this underlying percentage change right dude"*). It is not
//! retained as an unreachable variant: a refusal reason with no emit site
//! pre-registers a counter series that can only ever read zero, and a
//! permanently-zero counter reads as coverage rather than as absence.

use tickvault_common::types::ExchangeSegment;
use tickvault_storage::top_volume_rank_persistence::{SnapshotCadence, TopVolumeRankRow};

use crate::contract_underlying_map::UNLABELLED_CONTRACT;
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
    /// No human label was resolvable for this contract.
    ///
    /// # This one does NOT drop the row either (2026-09-13)
    ///
    /// Like the deleted `GainUnavailable` before it, the label is a leaf DISPLAY
    /// column: it is not in the DEDUP key and nothing orders by it. The row is
    /// WRITTEN with `contract_underlying_map::UNLABELLED_CONTRACT` and still
    /// carries `security_id` + `segment`, so it is identifiable by join.
    ///
    /// It should be ZERO on a healthy session: the label snapshot and the
    /// owner snapshot are built from the same artifact rows in the same pass,
    /// and only a contract the owner map admitted can reach the leaderboard.
    /// A non-zero reading means the two snapshots have DRIFTED — the label
    /// publish failed, or a second producer published one without the other.
    ///
    /// It replaced `RankOutOfRange`, which died with the `rank` column on the
    /// same day. `ALL` therefore stays four long and every fixed-size counter
    /// array sized from it is unchanged.
    LabelUnavailable,
    /// `window_lots_milli` did not fit a signed 64-bit column, or its
    /// percentage form overflowed. Refused rather than wrapped, because a
    /// negative rank key stored as the reason a contract ranked first is worse
    /// than an absent row.
    ///
    /// # ⚠ BOTH ARMS ARE UNREACHABLE at today's bounds (recorded 2026-09-13)
    ///
    /// The prior text said "unreachable … it would need ~9.2e15 milli-lots",
    /// which conflated the units (9.2e15 *lots* is 9.2e18 *milli*-lots) and
    /// understated the real headroom by 1000x. The arithmetic, stated so the
    /// next reader does not re-derive it:
    ///
    /// | quantity | value |
    /// |---|---|
    /// | `delta_units` ceiling (it is a `u32`) | 4,294,967,295 |
    /// | `window_lots_milli` = `delta_units * 1000 / lot_size`, worst at `lot_size = 1` | ~**4.29e12** |
    /// | `i64::try_from` (the KEY arm) fails above `i64::MAX` | ~**9.22e18** — ~2.1 MILLION x above |
    /// | `checked_mul(100)` (the PCT arm) fails above `i64::MAX / 100` | ~**9.22e16** — ~21,000 x above |
    ///
    /// So neither arm can fire, and **this counter reading zero proves
    /// nothing** — it is the class this repository has recorded three times
    /// (a saturated redial ceiling, an unreachable `unsubscribe_timeout`, a
    /// self-satisfying source scan) where an unreachable counter gets cited as
    /// coverage. The guards STAY: an unreachable guard on arithmetic is
    /// correct defence, and it is what makes the widening of `delta_units` to
    /// `u64` a safe future change rather than a silent wrap.
    ///
    /// Its two siblings differ, and the difference is the point:
    /// [`Self::LabelUnavailable`] fires on real snapshot drift, and
    /// [`Self::IdTooLargeForSignedColumn`] is unreachable only because every
    /// namespace band in use today sits below `2^63` — an id space this
    /// module does not own, so that one is unreachable by CONVENTION where
    /// these two are unreachable by ARITHMETIC.
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
    pub const ALL: [Self; 3] = [
        Self::IdTooLargeForSignedColumn,
        Self::LabelUnavailable,
        Self::LotsOutOfRange,
    ];

    /// Position in [`Self::ALL`], so a caller can hold one pre-resolved
    /// counter handle per reason in a fixed array rather than building a
    /// labelled counter per refusal.
    ///
    /// That is not tidiness: `metrics::counter!(NAME, "reason" => label)` with
    /// a non-literal label drops to the ALLOCATING arm, which is the
    /// `record_ws_lag` class this repository measured at ~36M allocations an
    /// hour. Three reasons x four cadences over a population the 2026-09-12 cut
    /// removal made market-bounded is not a path to allocate on.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::IdTooLargeForSignedColumn => 0,
            Self::LabelUnavailable => 1,
            Self::LotsOutOfRange => 2,
        }
    }

    /// Stable label for the refusal counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdTooLargeForSignedColumn => "id_too_large",
            Self::LabelUnavailable => "label_unavailable",
            Self::LotsOutOfRange => "lots_out_of_range",
        }
    }
}

/// A built snapshot: the rows to write, plus what was left out and why.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotProjection<'a> {
    /// The rows, in rank order.
    ///
    /// The lifetime is the LABEL SNAPSHOT's, not decoration: `contract` is a
    /// `&'a str` borrowed out of the `Arc<HashMap<..>>` the caller loaded once
    /// for this sweep, which is what makes the per-row label cost one pointer
    /// copy and zero allocation. The rows are appended inside the same sweep
    /// that built them, so an owned `String` (an allocation per row, up to
    /// ~80,000 a second on the frame drain) buys nothing.
    pub rows: Vec<TopVolumeRankRow<'a>>,
    /// Contracts dropped, with the reason. Never silently discarded.
    pub refusals: Vec<(u64, SnapshotRefusal)>,
}

impl SnapshotProjection<'_> {
    /// How many contracts were refused.
    #[must_use]
    pub fn refusal_count(&self) -> usize {
        self.refusals.len()
    }
}

/// Entries [`project_snapshot`] reserves in its refusal buffer up front.
///
/// A SMALL FIXED capacity, not `ranked.len()`, and the number is derived
/// rather than round. One entry is `(u64, SnapshotRefusal)` = **16 B** (an 8 B
/// id plus a one-byte fieldless enum, padded by the `u64`'s alignment), so 64
/// entries is exactly **1 KiB** — one allocation, page-sized, per family per
/// cadence.
///
/// It is sized for the case that actually happens. On a healthy sweep the
/// refusal count is **zero**: `IdTooLargeForSignedColumn` and
/// `LotsOutOfRange` are both unreachable at real bounds (see
/// [`SnapshotRefusal::LotsOutOfRange`]), and `LabelUnavailable` means the two
/// artifact-built snapshots have DRIFTED, which is a handful of contracts or
/// none. 64 covers the drift case with no reallocation at all.
///
/// It is deliberately NOT sized for a degraded window in which most rows
/// refuse. Covering that costs ~323 KB of reserved-and-untouched memory on
/// EVERY sweep, all session, on the frame drain — see the note at the
/// allocation site. Growing into it instead costs ~10 reallocations inside one
/// transient window, and `Vec`'s doubling makes the total memcpy amortized
/// O(1).
///
/// ⚠ This read "the degraded window (post-open, before the first underlying is
/// priced, when every row refuses `GainUnavailable`)" until 2026-09-19. That
/// window no longer produces refusals at all: `gain_pct` and its reason were
/// deleted with the operator's removal of the underlying percentage change,
/// so an unpriced underlying now costs this table nothing. The SIZING is
/// unchanged and is still right — a small fixed capacity that grows
/// geometrically is correct whether the degraded case is common or absent —
/// but the case it was sized against is gone, which makes 64 more generous
/// than it needs to be rather than less.
const SNAPSHOT_REFUSAL_PREALLOC: usize = 64;

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
/// `top_volume` row land on the same grid as a `candles_<tf>` row.
///
/// ⚠ **The grid alone does NOT make the two joinable, and this doc said it
/// did until 2026-09-18.** This function floors to a grid CELL BOUNDARY; a
/// sweep firing at a boundary therefore floors to the window's CLOSE, while a
/// candle is stamped at its window's OPEN — one period apart on every row.
/// The caller subtracts one period for exactly that reason; see the dated
/// comment at the `ts` binding in `project_snapshot`.
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

/// The stored volume-percentage change, in MILLI-PERCENT, from the rank key.
///
/// # It is a monotone transform of `window_lots_milli`, not new information
///
/// `net_volume_chg_pct = window_lots_milli / 10 - 100`, so in thousandths of a
/// percent that is exactly `window_lots_milli * 100 - 100_000`. The two carry
/// the SAME ordering, row for row, including every tie — which is why the
/// comparator in `volume_leaderboard::rank` still sorts the integer key and
/// this column is presentation only. It is STORED anyway, on the operator's
/// 2026-09-13 instruction ("put the volume percentage chnage column as well
/// also alwasy to see the rpecise percnetgae chnage"): a reader asking the
/// table what changed should not have to know the transform. That is a
/// legitimate reason to store a derived value and it must not be read as the
/// column being independent — if the two ever disagree, this one is wrong.
///
/// MILLI-percent, and integer, for the reason the whole ranking path is
/// integer: a float in a column derived from an integer key can round two
/// distinct keys onto one value, and this repository bans a float sort key
/// outright. `4_150_000` here is `+4150.000%`; `0` is exactly one lot; a
/// contract that traded LESS than one lot reads NEGATIVE, which is the reason
/// the change form was chosen over a ratio.
///
/// `None` only on an overflow that needs `window_lots_milli` above
/// `i64::MAX / 100` ≈ **9.2e16** (⚠ CORRECTED 2026-09-13 — this read `~9.2e13`,
/// low by 1000x). The real ceiling is `u32::MAX * 1000` ≈ 4.29e12, so the arm
/// is ~21,000x out of reach and CANNOT FIRE; it is refused rather than wrapped
/// because a wrapped percentage stored as the reason a contract ranked first
/// is worse than an absent row, and it is kept so widening `delta_units` stays
/// a safe change. See [`SnapshotRefusal::LotsOutOfRange`] for the full table
/// and for why an unreachable counter must never be cited as coverage.
#[must_use]
pub const fn net_volume_chg_milli_pct(window_lots_milli: i64) -> Option<i64> {
    match window_lots_milli.checked_mul(100) {
        Some(scaled) => scaled.checked_sub(100_000),
        None => None,
    }
}

/// The scale the leaderboard's lot arithmetic carries, and the divisor that
/// takes it back to the whole numbers the operator reads.
///
/// `VolumeLeaderboard` computes `(delta_units * 1000) / lot_size` so that a
/// fractional lot survives the integer division and the ranking can separate
/// two contracts that differ by less than one lot. That scale is an internal
/// detail of the RANKING; the operator's own directive of 2026-09-19 is that
/// the STORED figures are whole ("total lots traded shoudl be the long right
/// why decimal here bro why the fuck bro"), so both stored columns divide it
/// out here rather than pushing a thousandths unit into the table.
///
/// Named rather than spelled `1_000` at the two division sites, because the
/// two must move together if the leaderboard's scale ever changes: dividing
/// the lots by one factor and the percentage by another would put a row's own
/// two numbers into disagreement, which is the one failure a reader cannot
/// detect from the row.
const MILLI_PER_WHOLE: i64 = 1_000;

/// One reading of the candle fold's OPEN bucket for a contract, at the
/// snapshot instant.
///
/// Added 2026-09-18 for the operator's *"i clelary told you to precisely have
/// the same volume even in top volume also as simialr to candles tables
/// volume"*. The leaderboard cannot answer that on its own —
/// `RankedContract` carries traded UNITS and a cumulative counter and **no
/// price at all**, so it can neither sign a volume nor state a price move.
/// The fold holds both, and this is what the projection asks it for.
///
/// A struct rather than a tuple because three unlabelled numbers at a call
/// site is how a skew gets stored in a volume column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandleBarReading {
    /// The bar's SIGNED gross volume — byte-identical to what
    /// `candles_<tf>.volume` carries for this contract and bucket.
    pub signed_volume: i64,
    /// Close vs YESTERDAY's close, in percent — the field that fills BOTH
    /// `candles_<tf>.close_pct_from_prev_day` AND `candles_<tf>.change_pct`
    /// (the extraction site writes one `LiveCandleState` field into both
    /// columns). Copied, never re-derived.
    pub close_pct_from_prev_day: f64,
    /// Close vs TODAY's 09:15 session open, in percent — byte-identical to
    /// `candles_<tf>.open_pct`. NOT the opening gap: that is `open_gap_pct`,
    /// a different column measuring the session open against yesterday's
    /// close, and it is deliberately not carried here.
    pub open_pct: f64,
}

impl CandleBarReading {
    /// Reads one bar the fold returned for a row's window.
    ///
    /// The sweep fires AT the window close, so that bar is usually still the
    /// OPEN bucket — no tick stamped past the boundary has landed yet. The two
    /// percentages are written only when a bucket seals, so on an open bucket
    /// they still read 0.0, and 0.0 passes the finite filter and would be
    /// stored as a real "unchanged on the day". Stamping this COPY runs the
    /// exact function the seal runs, on the same close / previous-day close /
    /// session open, so the value is the one the candle row will carry for the
    /// ticks folded so far. Idempotent on a sealed bar.
    ///
    /// # Complexity
    /// O(1): three percentage computations on a `Copy` value, no allocation.
    #[must_use]
    pub fn from_bar(
        mut bar: tickvault_trading::candles::live_candle_state::LiveCandleState,
    ) -> Self {
        bar.stamp_seal_percentages();
        Self {
            signed_volume: bar.signed_volume(),
            close_pct_from_prev_day: bar.close_pct_from_prev_day,
            open_pct: bar.open_pct,
        }
    }
}

/// Projects one family's ranked slice into storable rows.
///
/// `is_subscribed` is supplied by the caller because the leaderboard does not
/// hold it: subscription state belongs to the depth pool. Passing it in keeps
/// this function pure and keeps the lookup the caller's O(1) hash probe rather
/// than a scan here. `label_of` and `candle_of` are handed over for the same
/// reason.
///
/// # The UNDERLYING's percentage move is GONE (operator, 2026-09-19)
///
/// A `gain_pct_of` closure used to be a fifth parameter, filling a `gain_pct`
/// column with the move of the contract's UNDERLYING. The operator removed it:
/// *"See as of now I believe we don't need this underlying percentage change
/// right dude"*. Its refusal arm went with it rather than being left as a
/// reason that can never fire — a counter with no emit site reads as coverage
/// it does not provide. The percentages this row now carries are the
/// CONTRACT's own, copied from the candle fold; the underlying's move is still
/// available from the underlying's own rows in `candles_<tf>`.
///
/// O(k) in the ranked slice. Per contract it is O(1): three closure calls and
/// a handful of widenings.
///
/// ⚠ CORRECTED 2026-09-13: this read "which is 250 at the authorized budget".
/// The operator's 2026-09-12 directive REMOVED the top-250 persistence cut —
/// every traded option contract is projected — so `k` is market-bounded, capped
/// only by `TOP_VOLUME_PERSIST_PER_FAMILY` (25,000). The claim understated the
/// slice by up to 100x, in the reassuring direction.
#[must_use]
pub fn project_snapshot<'a, S, L, C>(
    snapshot_ts_ist_nanos: i64,
    cadence: SnapshotCadence,
    family: OptionFamily,
    ranked: &[RankedContract],
    is_subscribed: S,
    label_of: L,
    candle_of: C,
) -> SnapshotProjection<'a>
where
    S: Fn(u64, ExchangeSegment) -> bool,
    L: Fn(u64, ExchangeSegment) -> Option<&'a str>,
    C: Fn(u64, ExchangeSegment, u32) -> Option<CandleBarReading>,
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
    // ⚠ CORRECTED 2026-09-18 — the grid alignment above was necessary and was
    // NOT sufficient, and the module said otherwise in two places.
    //
    // The sweep fires AT a grid boundary, so flooring `now` to the grid yields
    // the boundary that has just been crossed — which is the window's CLOSE.
    // A `candles_<tf>` bar is stamped at its window's OPEN. So a `top_volume`
    // row and a candle row carrying the same `ts` described windows ONE PERIOD
    // APART, on every row of every cadence, while this file's own docs claimed
    // the alignment "lets a `top_volume` row and a `candles_<tf>` row share a
    // `ts` and be joined". They could not be joined, and the failure was
    // silent: the query simply returned the wrong window's figures.
    //
    // The measurement is unchanged — the leaderboard's delta covers the
    // interval since the previous fire, i.e. the window that just closed — so
    // naming that window by its OPEN is what the operator's "both of them
    // should be precisely matchable" (2026-09-18) actually requires.
    let boundary = floor_to_grid(snapshot_ts_ist_nanos, cadence.interval_secs());
    let period_nanos = i64::try_from(cadence.interval_secs())
        .unwrap_or(1)
        .saturating_mul(NANOS_PER_SECOND);
    let ts = boundary.saturating_sub(period_nanos);
    // The row stamp in IST epoch SECONDS — the OPEN of the window this
    // snapshot describes, and the exact key the candle fold buckets on
    // (`LiveCandleState::bucket_start_ist_secs` is seconds). It is handed to
    // the candle probe so the probe can answer for THIS window or refuse.
    //
    // `None` only when the stamp will not fit the fold's `u32` second —
    // impossible on any epoch this code can see (`u32` seconds run past year
    // 2106) and refused rather than coerced, because coercing to `0` would
    // collide with the fold's own never-opened sentinel and store an empty
    // bar as if it were a measurement.
    let ts_secs = ts.div_euclid(NANOS_PER_SECOND);
    let window_open_secs = u32::try_from(ts_secs).ok();
    // `rows` IS pre-sized: it genuinely fills. Every contract on the board
    // produces a row — the common-case refusal (`LabelUnavailable`) does
    // NOT drop the row, it only NULLs a leaf column — so `ranked.len()` is
    // the exact final length in
    // every reachable case and one allocation is the whole cost.
    let mut rows = Vec::with_capacity(ranked.len());
    // `refusals` is NOT, and the comment that used to defend pre-sizing it
    // argued about the allocation COUNT while being silent about the BYTES.
    //
    // # ⚠ CORRECTED 2026-09-13 — `Vec::with_capacity(ranked.len())` was a
    // # multi-hundred-KB allocation per sweep, on the drain, to hold nothing
    //
    // The prior text read "it makes the projection's allocation count a
    // constant 2 rather than a function of the market". True of the count and
    // false about what it costs. One entry is `(u64, SnapshotRefusal)` — 8 B
    // of id plus a one-byte fieldless enum, padded to **16 B** by the u64's
    // alignment. Since the operator's 2026-09-12 directive removed the
    // top-250-per-family persistence cut, `ranked.len()` is market-bounded
    // (~20,220 traded option contracts measured, ceiling
    // `TOP_VOLUME_PERSIST_PER_FAMILY` = 25,000), so that line reserved
    //
    //     20,220 x 16 B  =  ~323 KB   per family per cadence
    //
    // and the cadences are FOUR since the same directive (1s/3s/5s/1m). In the
    // second where all four arms land together across both families that is
    // **8 x ~323 KB = ~2.6 MB allocated and dropped per second** on the frame
    // drain, for a buffer whose steady-state length is ZERO.
    //
    // The count claim did not even hold on its own terms when it was written:
    // `LabelUnavailable` and the then-existing `GainUnavailable` could BOTH
    // fire for one contract, so the true worst case was `2 x ranked.len()`
    // entries and `ranked.len()` of capacity would have reallocated anyway.
    //
    // ⚠ 2026-09-19: the degraded window that argument turned on is GONE.
    // `GainUnavailable` fired on every row between the open and the first
    // priced underlying (measured at up to ~30 minutes on 2026-09-08), and it
    // was deleted with the `gain_pct` column. The remaining two reasons are
    // unreachable arithmetic and real snapshot drift, so the steady-state
    // length is now zero in every session, not merely most of one. The fixed
    // capacity STAYS: geometric growth costs nothing when it never grows, and
    // ~1 KiB once per sweep is the honest price of keeping a drift buffer that
    // does not have to be sized against the market.
    let mut refusals = Vec::with_capacity(SNAPSHOT_REFUSAL_PREALLOC);

    for contract in ranked {
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
        let Ok(window_lots_milli) = i64::try_from(contract.window_lots_milli) else {
            refusals.push((contract.security_id, SnapshotRefusal::LotsOutOfRange));
            continue;
        };
        // The percentage the operator reads, as a WHOLE NUMBER (operator,
        // 2026-09-19 Quote B: "total lots traded shoudl be the long right why
        // decimal here bro"). Derived in `i64` throughout so no float ever
        // touches this path.
        //
        // # Why it is divided from the milli value, not from `total_lots_traded`
        //
        // `total_lots_traded` truncates to whole lots, so deriving the
        // percentage from it would throw away the fractional lot the operator
        // asked to be precise about: 40.9 lots is +3,990%, and
        // `total_lots_traded * 100 - 100` would report +3,900%. Both figures
        // are the honest whole-number rendering of their OWN quantity, and
        // each is derived from the same divisor, so the two never disagree
        // about rank — truncation toward zero is monotone, so the descending
        // order is identical either way.
        //
        // ⚠ CORRECTED 2026-09-13 — the implication was stated BACKWARDS. This
        // read "it shares the lots refusal arm because it is a total function
        // of it: if the key did not fit, neither does the percentage." The
        // percentage limit is `i64::MAX / 100`, which is **100x TIGHTER** than
        // the key limit of `i64::MAX` — so the true implication runs the other
        // way (pct fits => key fits), and on paper a key-fits/pct-fails band
        // exists at 9.22e16 < v <= 9.22e18. Sharing the arm is still right,
        // because both mean "the volume arithmetic did not fit a column"; what
        // was wrong is the reason given for it.
        //
        // In practice NEITHER arm is reachable: the milli figure is capped by
        // `u32::MAX * 1000` ~ 4.29e12, ~21,000x below even the tighter pct
        // limit. The band above is arithmetic, not a live case — see
        // `SnapshotRefusal::LotsOutOfRange`.
        let Some(volume_percentage_change) =
            net_volume_chg_milli_pct(window_lots_milli).map(|milli| milli / MILLI_PER_WHOLE)
        else {
            refusals.push((contract.security_id, SnapshotRefusal::LotsOutOfRange));
            continue;
        };
        // Whole lots, per the same quote. `floor(floor(1000d/L)/1000)` is
        // exactly `floor(d/L)` for positive integers, so this is the lot count
        // itself and not a rounding of a rounding.
        let total_lots_traded = window_lots_milli / MILLI_PER_WHOLE;

        // ONE hash probe into the snapshot the caller loaded for this sweep,
        // then a pointer copy. No allocation, no refcount bump, no formatting
        // on the frame drain — the label was rendered once at attach.
        let contract_label = label_of(contract.security_id, contract.segment);
        if contract_label.is_none() {
            refusals.push((contract.security_id, SnapshotRefusal::LabelUnavailable));
        }

        // ---- the candle fold's OWN reading of this contract (2026-09-18) ----
        //
        // ONE O(1) probe — one hash lookup and one index into a pre-sized slot
        // array. It answers `None` when the contract has no aggregator slot at
        // all (never ticked this session, or the slot budget was exhausted).
        //
        // The probe is asked for THIS ROW'S OWN WINDOW, never for "whatever is
        // open now": it is handed `window_open_secs` and answers `None` unless
        // it holds a bar that starts exactly there. So every candle-sourced
        // column either describes this row's window or is NULL with the rest
        // of them — a cross-verification can never be handed a neighbouring
        // window's volume wearing this row's timestamp.
        //
        // `window_open_secs` is `None` only when the row's own `ts` will not
        // fit the fold's `u32` second — impossible on any epoch this code can
        // see (`u32` seconds run past year 2106) and refused rather than
        // coerced, because coercing to `0` would match the fold's own
        // never-opened sentinel and store an empty bar as a measurement.
        let candle = window_open_secs
            .and_then(|window| candle_of(contract.security_id, contract.segment, window));
        // FINITE-gated on every percentage the bar carries. The fold already
        // refuses a non-finite quotient, so this is the second gate on values
        // that must never reach `column_f64` — cheap, and it means a future
        // caller computing a percentage in-line cannot slip a NaN through.
        // COPIED from the bar, never re-derived (operator, 2026-09-19 Quote A:
        // "no extra claucltion or derivation"). `percentage_change` is the
        // candles table's `close_pct_from_prev_day` — the SAME field that
        // fills both `close_pct_from_prev_day` and `change_pct` there — and
        // `open_percentage_change` is its `open_pct`. Neither is the
        // bar-over-bar figure above, and neither is the opening gap.
        let percentage_change = candle
            .as_ref()
            .map(|bar| bar.close_pct_from_prev_day)
            .filter(|p| p.is_finite());
        let open_percentage_change = candle
            .as_ref()
            .map(|bar| bar.open_pct)
            .filter(|p| p.is_finite());

        // The three receipt-delay pairs (operator, 2026-09-19 Quote D) are NOT
        // on this row: the 2026-09-19 fifteen-column contract removed them
        // from `top_volume`, and they live on `candles_<tf>`, where the fold
        // holds the receipt stamps. (2026-09-22: a comment block describing
        // how this projection computed them stood here after the fields were
        // gone.)
        rows.push(TopVolumeRankRow {
            snapshot_ts_ist_nanos: ts,
            cadence,
            family: family.as_str(),
            feed: SNAPSHOT_FEED,
            segment: contract.segment.as_str(),
            contract: contract_label.unwrap_or(UNLABELLED_CONTRACT),
            security_id,
            underlying_id,
            // `per_lot_quantity` is a `u32` at the source, so `i64::from` is
            // lossless and needs no saturation arm. A zero lot is already
            // impossible here — `rank` only reaches this projection for
            // contracts whose lots divided, and that division returns `None`
            // on a zero lot.
            per_lot_quantity: i64::from(contract.lot_size),
            total_lots_traded,
            volume_percentage_change,
            subscribed: is_subscribed(contract.security_id, contract.segment),
            // The candle's OWN signed volume for this window — the SAME number
            // `candles_<tf>` carries for the same instrument and the same
            // window, read off the fold probe and COPIED, never recomputed
            // (operator, 2026-09-19: "no extra claucltion or derivation").
            // NULL when the fold has no bar — never a zero, which would read
            // as "traded nothing".
            volume: candle.as_ref().map(|bar| bar.signed_volume),
            percentage_change,
            open_percentage_change,
        });
    }

    SnapshotProjection { rows, refusals }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The label closure every test passes unless it is testing the label.
    ///
    /// A `const fn` pointer rather than a closure literal at eighteen call
    /// sites: the point of the tests below is the projection, and a resolvable
    /// label is the ordinary case.
    #[allow(clippy::unnecessary_wraps)]
    const fn test_label(_id: u64, _segment: ExchangeSegment) -> Option<&'static str> {
        Some("RELIANCE-25Sep2026-1400-CE")
    }
    const TEST_LABEL: fn(u64, ExchangeSegment) -> Option<&'static str> = test_label;

    /// The complement: a label snapshot that answers nothing, for the drift
    /// case. Named rather than inlined so a reader sees which test is about
    /// the fallback.
    const fn no_label(_id: u64, _segment: ExchangeSegment) -> Option<&'static str> {
        None
    }
    const NO_LABEL: fn(u64, ExchangeSegment) -> Option<&'static str> = no_label;

    /// The fold reading every test passes unless it is testing the fold.
    ///
    /// `None` — no aggregator slot — is the ordinary case for a synthetic
    /// `RankedContract` that was never folded, and it is what every test
    /// written before 2026-09-18 implicitly assumed. Passing it explicitly
    /// keeps those tests asserting exactly what they asserted before the
    /// three fold columns existed.
    const fn no_candle(
        _id: u64,
        _segment: ExchangeSegment,
        _window: u32,
    ) -> Option<CandleBarReading> {
        None
    }
    const NO_CANDLE: fn(u64, ExchangeSegment, u32) -> Option<CandleBarReading> = no_candle;

    /// A fold reading for the row's own window, carrying a NEGATIVE volume and
    /// the two day-scoped percentages beside it -- the three fields the probe
    /// still exists to carry after the 2026-09-19 strip.
    #[allow(clippy::unnecessary_wraps)]
    fn test_candle(_id: u64, _segment: ExchangeSegment, _window: u32) -> Option<CandleBarReading> {
        Some(CandleBarReading {
            signed_volume: -4_242,
            // Internally consistent, so a reader checking the arithmetic
            // finds it holds: a 100.00 previous-day close, a 100.50 session
            // open, and a 101.63 close give 1.63% and 1.12%.
            close_pct_from_prev_day: 1.63,
            open_pct: 1.12,
        })
    }
    const TEST_CANDLE: fn(u64, ExchangeSegment, u32) -> Option<CandleBarReading> = test_candle;

    // ⚠ REMOVED 2026-09-19 — `contract_under` was a pass-through to
    // [`contract`] that existed only to NAME the underlying for the tests
    // exercising the gain closure, because that closure was the one input
    // keyed on the underlying rather than the contract. The closure is gone
    // and nothing in the surviving projection is underlying-keyed, so the
    // alias had no remaining job. A dead helper in a test module is a
    // warning, not a spare.

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
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        assert_eq!(p.rows.len(), 3);
        // The `rank` COLUMN was removed on 2026-09-13 (operator: "rmeve the
        // rank in top volume"). The ORDER it recorded is still the property
        // that matters, and it is still asserted — the projection must emit
        // rows in the ranked slice's order, so a reader can recover position
        // from the row order without a stored integer that could disagree
        // with it.
        assert_eq!(
            p.rows.iter().map(|r| r.security_id).collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
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
            |_, _| true,
            TEST_LABEL,
            NO_CANDLE,
        );
        // ONE stamp for the whole snapshot — the property this test is named
        // for, and the reason the DEDUP key can collapse a re-emit.
        let stamp = p.rows[0].snapshot_ts_ist_nanos;
        assert!(p.rows.iter().all(|r| r.snapshot_ts_ist_nanos == stamp));

        // UPDATED 2026-09-18: the stamp is the grid cell the closed window
        // OPENED on — one period BEFORE the boundary the fire floors to. The
        // grid alignment (2026-09-12) put the two on the same lattice; this
        // subtraction is what makes a `top_volume` row and the `candles_5s`
        // row for the same window carry the SAME `ts`, which the 2026-09-18
        // directive requires ("both of them should be precisely matchable").
        // The sub-second part is still gone, which is what the DEDUP key needs.
        assert_eq!(stamp, floor_to_grid(fired_at, 5) - 5 * NANOS_PER_SECOND);
        assert_eq!(
            stamp % NANOS_PER_SECOND,
            0,
            "a snapshot stamp must never carry a sub-second remainder"
        );
    }

    #[test]
    fn the_stamp_names_the_window_that_closed_not_the_one_beginning() {
        // The joinability property, stated as arithmetic rather than as a
        // claim in a docstring — which is how it was wrong from 2026-09-12 to
        // 2026-09-18. A sweep firing at 09:20:05 on the 5s board measured
        // 09:20:00..09:20:05, so its row must carry 09:20:00 — the same `ts`
        // the `candles_5s` bar for that window carries.
        for (cadence, period) in [
            (SnapshotCadence::OneSecond, 1_i64),
            (SnapshotCadence::ThreeSecond, 3),
            (SnapshotCadence::FiveSecond, 5),
            (SnapshotCadence::OneMinute, 60),
        ] {
            // An exact boundary: 09:00 + 60 periods, so every cadence lands on
            // one and the arithmetic is checkable by hand.
            let boundary_secs = SNAPSHOT_GRID_ANCHOR_SECS_OF_DAY_IST + 60 * period;
            let fired_at = boundary_secs * NANOS_PER_SECOND;
            let ranked = [contract(10, 1, 500)];
            let p = project_snapshot(
                fired_at,
                cadence,
                OptionFamily::Index,
                &ranked,
                |_, _| true,
                TEST_LABEL,
                NO_CANDLE,
            );
            assert_eq!(
                p.rows[0].snapshot_ts_ist_nanos,
                (boundary_secs - period) * NANOS_PER_SECOND,
                "{cadence:?}: the stamp must be the closed window's OPEN"
            );
        }
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
            |sid, _| sid == 10,
            TEST_LABEL,
            NO_CANDLE,
        );
        assert!(p.rows[0].subscribed);
        assert!(!p.rows[1].subscribed);
    }

    /// The projection carries the rank division's two INPUTS, not just its
    /// result — and carries them per contract, never a blanket value.
    ///
    /// ⚠ The fixture volumes are 5,000 and 1,000 rather than the 500/400 the
    /// sibling tests use, and that is load-bearing rather than arbitrary.
    /// `total_lots_traded` became WHOLE lots on 2026-09-19; the fixture's
    /// `lot_size` is 1,000 and its key is `volume * 3`, so 500 and 400 both
    /// floor to ONE lot and the per-contract assertions below passed on two
    /// identical values — which is exactly what they exist to refuse. At
    /// 5,000 and 1,000 the lot counts are 15 and 3.
    #[test]
    fn project_snapshot_carries_the_rank_inputs_per_contract() {
        let ranked = [contract(10, 1, 5_000), contract(11, 1, 1_000)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| true,
            TEST_LABEL,
            NO_CANDLE,
        );
        assert_eq!(p.rows.len(), 2);

        for (row, src) in p.rows.iter().zip(ranked.iter()) {
            assert_eq!(row.per_lot_quantity, i64::from(src.lot_size));
            // The stored pair must close on the SOURCE's traded units, or the
            // column that exists to make the ordering checkable cannot check
            // it. The traded-unit count is no longer a column -- the operator
            // removed it on 2026-09-19 -- so the check reads it off the
            // ranked source instead of off the row.
            //
            // ⚠ The `1_000` the old form carried is GONE, and its absence is
            // the point. `total_lots_traded` is WHOLE lots since 2026-09-19,
            // and `floor(floor(1000d/L)/1000) == floor(d/L)` for every
            // positive `d` and `L` -- so the stored value is the lot count
            // itself, not a rounding of a rounding, and the check says so in
            // the units the operator reads.
            assert_eq!(
                i64::from(src.delta_units) / row.per_lot_quantity,
                row.total_lots_traded
            );
        }

        // Per contract, not blanket: the two rows differ.
        assert_ne!(p.rows[0].total_lots_traded, p.rows[1].total_lots_traded);
        assert_ne!(
            p.rows[0].volume_percentage_change,
            p.rows[1].volume_percentage_change
        );
    }

    #[test]
    fn project_snapshot_stamps_the_family_and_feed_labels() {
        let ranked = [contract(10, 1, 500)];
        let stock = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        let index = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Index,
            &ranked,
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
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
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
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
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        assert!(p.rows.is_empty());
        assert_eq!(p.refusal_count(), 1);
    }

    /// A non-finite CANDLE percentage is NULLed, and the row survives.
    ///
    /// ⚠ RE-POINTED 2026-09-19 from `project_snapshot_refuses_a_nan_or_infinite_gain`.
    /// The `gain_pct` column it was written for is gone with the operator's
    /// removal of the underlying percentage change, but the hazard it guards
    /// is not: the §28.4 poisoning class one table over, where a NaN in a
    /// DOUBLE makes every later comparison on the column silently false. The
    /// two candle percentages are the columns that carry it now, and each
    /// is `.filter(|p| p.is_finite())` for exactly this reason.
    #[test]
    fn project_snapshot_nulls_a_nan_or_infinite_candle_percentage() {
        let ranked = [contract(10, 91, 500), contract(11, 92, 400)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| false,
            TEST_LABEL,
            |id, _, _| {
                Some(CandleBarReading {
                    signed_volume: -4_242,
                    // Contract 10 gets the poison in BOTH percentages;
                    // contract 11 gets finite values, so the test separates
                    // "NULLed" from "never written".
                    close_pct_from_prev_day: if id == 10 { f64::INFINITY } else { 2.14 },
                    open_pct: if id == 10 { f64::NEG_INFINITY } else { -1.08 },
                })
            },
        );
        // BOTH rows survive. A display column that cannot be resolved must
        // never take the contract's volume out with it -- that is the whole
        // reason the table exists, and the 2026-09-12 fix that established it.
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[0].security_id, 10);
        assert_eq!(p.rows[0].percentage_change, None, "Inf is NULLed");
        assert_eq!(p.rows[0].open_percentage_change, None, "-Inf is NULLed");
        // And the volume the row exists to carry is untouched by any of it --
        // the candle's OWN signed number, which is finite and stays written.
        assert_eq!(p.rows[0].volume, Some(-4_242));
        assert_eq!(p.rows[1].security_id, 11);
        assert!(
            p.rows[1]
                .percentage_change
                .is_some_and(|g| (g - 2.14).abs() < f64::EPSILON)
        );
        // NOT counted as a refusal. The candle percentages have no refusal
        // reason of their own: unlike the retired `GainUnavailable`, an absent
        // candle is the ORDINARY state for a contract with no aggregator slot
        // or a bar in a neighbouring window, and counting it would report a
        // permanent nonzero rate for a system working as designed.
        assert_eq!(p.refusal_count(), 0);
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
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        // The refused row is GONE, and the survivors keep the ranked slice's
        // ORDER — which is all the removed `rank` column ever recorded. A
        // stored rank here would have read 1 and 3 (positions in the input),
        // and that gap was the property being asserted; row order carries it
        // with nothing that can disagree.
        assert_eq!(
            p.rows.iter().map(|r| r.security_id).collect::<Vec<_>>(),
            vec![10, 12]
        );
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
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        assert_eq!(p.rows.len(), 2);
        // ⚠ RE-POINTED 2026-09-19 at the column that IS the sort key. The
        // milli figure is gone from the table and the whole-lot count that
        // replaced it is **1 for BOTH rows** (1.5 lots and 1.2 lots each
        // floor to 1) -- so asserting the lot count would assert a number
        // that cannot order anything, and this test would pass while the
        // property it exists for was broken. The percentage separates them
        // because it is derived from the MILLI value, before the floor.
        assert_eq!(p.rows[0].total_lots_traded, 1);
        assert_eq!(p.rows[1].total_lots_traded, 1);
        assert_eq!(p.rows[0].volume_percentage_change, 50);
        assert_eq!(p.rows[1].volume_percentage_change, 20);
        // And it is genuinely a different column from the LOT COUNT beside
        // it: the two are derived from the same milli-lot figure, so a
        // refactor collapsing them would make this table order by a number
        // that has already been floored.
        assert_ne!(
            p.rows[0].volume_percentage_change,
            p.rows[0].total_lots_traded
        );
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
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        assert!(p.rows.is_empty());
        assert_eq!(p.refusals, vec![(10, SnapshotRefusal::LotsOutOfRange)]);
    }

    // ⚠ `project_snapshot_asks_the_gain_closure_for_the_underlying_not_the_contract`
    // was DELETED on 2026-09-19 with the closure it tested.
    //
    // It pinned a real 2026-09-09 defect -- the gain closure was handed the
    // CONTRACT id, no previous close is ever stored for an NSE_FNO contract,
    // so every row was refused and the table stayed empty all session. The
    // operator removed the underlying percentage change entirely (*"as of now
    // I believe we don't need this underlying percentage change"*), so there
    // is no closure left to hand the wrong id to.
    //
    // Recorded rather than silently dropped because the defect class is the
    // durable part: a projection keyed on the CONTRACT where the data is keyed
    // on the UNDERLYING fails totally and silently. Nothing in the surviving
    // projection is underlying-keyed -- `label_of` and `candle_of` both take
    // the contract -- so there is no live surface for it today.
    /// ⚠ REPOINTED 2026-09-19. It used to prove the VENDOR cumulative widened
    /// `u32` -> `i64` without loss, and that column left the table with the
    /// operator's strip. The widening it guards now is the one that survived
    /// and is larger: the milli-lot rank key is a `u64` and the stored lot
    /// count is an `i64`, and at `u32::MAX` volume this fixture's key is
    /// ~1.29e10 -- three times past `u32::MAX`, so a truncating cast anywhere
    /// on that path fails here rather than silently re-ordering the board.
    #[test]
    fn project_snapshot_widens_the_rank_key_without_loss() {
        let ranked = [contract(10, 1, u32::MAX)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        // `window_lots_milli` is `u32::MAX * 3` milli-lots on a 1,000-unit
        // lot, so the whole-lot figure is that over 1,000.
        let expected = (u64::from(u32::MAX) * 3 / 1_000) as i64;
        assert_eq!(p.rows[0].total_lots_traded, expected);
        assert!(
            expected > i64::from(u32::MAX) / 1_000,
            "the fixture must exceed a 32-bit lot count, or it proves nothing"
        );
    }

    #[test]
    fn project_snapshot_on_an_empty_ranking_writes_nothing_and_refuses_nothing() {
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &[],
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        assert!(p.rows.is_empty());
        assert_eq!(p.refusal_count(), 0);
    }

    #[test]
    fn refusal_count_reports_every_dropped_contract() {
        // Two DIFFERENT refusal reasons in one snapshot: the count is the
        // caller's single number for "how much of this snapshot is missing",
        // so it must not report only the first kind it met.
        //
        // ⚠ RE-POINTED 2026-09-19. This test used to pair the oversized id
        // with an unknown GAIN, and the gain column is gone. It now pairs it
        // with an unresolvable LABEL, which preserves the property that
        // actually mattered: the two reasons behave DIFFERENTLY — one deletes
        // the row, the other keeps it — and the count still covers both.
        let ranked = [
            contract(u64::MAX, 91, 500),
            contract(10, 92, 400),
            contract(11, 93, 300),
        ];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| false,
            |security_id, segment| {
                if security_id == 10 {
                    None
                } else {
                    test_label(security_id, segment)
                }
            },
            NO_CANDLE,
        );
        // TWO rows, not one: the oversized id DROPS its row (that column is in
        // the DEDUP key), the unresolvable label KEEPS its row with the
        // fallback string. Both are counted, and the count is still the
        // caller's single number for "how much of this snapshot needed a
        // refusal".
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[0].security_id, 10);
        assert_eq!(p.rows[0].contract, UNLABELLED_CONTRACT);
        assert_eq!(p.rows[1].security_id, 11);
        assert_eq!(p.refusal_count(), 2);
        assert_eq!(p.refusal_count(), p.refusals.len());
        // The two reasons are DISTINCT — a count of 2 built from one reason
        // twice would pass every assertion above.
        assert_eq!(
            p.refusals,
            vec![
                (u64::MAX, SnapshotRefusal::IdTooLargeForSignedColumn),
                (10, SnapshotRefusal::LabelUnavailable),
            ]
        );
    }

    /// The bite for the 2026-09-12 fix, re-pointed 2026-09-19: a contract the
    /// fold cannot answer for must NOT cost the row its volume.
    ///
    /// The original tested an unknown GAIN. That column is gone, and the
    /// property survives it: the candle passthroughs are leaf DISPLAY columns
    /// exactly as the gain was, so an absent bar NULLs them and leaves every
    /// key and ordering column intact.
    ///
    /// ⚠ The refusal half INVERTS, and that is the point of keeping the test.
    /// An unknown gain was COUNTED (`GainUnavailable`). An unknown candle is
    /// NOT, and must not be: a contract with no aggregator slot, or one whose
    /// bar sits in a neighbouring window, is the ORDINARY state, so counting
    /// it would report a permanent nonzero refusal rate for a system working
    /// as designed.
    #[test]
    fn a_row_with_an_unknown_candle_still_carries_its_volume() {
        let ranked = [contract(4_431, 2_885, 987_654)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| true,
            TEST_LABEL,
            // The fold has no bar for this contract in this window -- the
            // ordinary state at the open, and after any mid-session restart.
            NO_CANDLE,
        );
        assert_eq!(
            p.rows.len(),
            1,
            "an unknown candle must NULL its columns, never delete the row"
        );
        assert_eq!(p.rows[0].security_id, 4_431);
        // The RANK columns survive untouched: they come from the tick stream
        // by way of the leaderboard, never from the fold, so no candle answer
        // can erase them.
        assert_eq!(p.rows[0].total_lots_traded, 2_962);
        assert!(p.rows[0].subscribed);
        // Every candle passthrough NULL, and none of them counted.
        assert_eq!(p.rows[0].volume, None);
        assert_eq!(p.rows[0].percentage_change, None);
        assert_eq!(p.rows[0].open_percentage_change, None);
        assert!(
            p.refusals.is_empty(),
            "an absent bar is the ordinary state, not a refusal"
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
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
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
        // ⚠ The literal `4` that stood here is GONE (2026-09-19), and not
        // merely because `ALL` is now three long. It claimed to prove "ALL is
        // complete", and it could never do that: a variant added to the enum
        // but NOT to `ALL` leaves the length unchanged, so the literal passes
        // on exactly the defect it named. What actually guards completeness is
        // the EXHAUSTIVE `match` inside `index()` and `as_str()` -- adding a
        // variant fails the build there, at compile time, without any test.
        //
        // What this assertion can honestly carry is non-vacuity: the loop
        // above proves nothing over an empty or one-element array.
        assert!(SnapshotRefusal::ALL.len() >= 2);
        // And `ALL` must list each reason ONCE -- a duplicate would give two
        // slots the same `index()` and the loop above would still pass for
        // whichever position happened to match.
        let mut seen = SnapshotRefusal::ALL;
        seen.sort_unstable_by_key(|r| r.index());
        seen.windows(2).for_each(|w| {
            assert_ne!(w[0].index(), w[1].index(), "duplicate slot in ALL");
        });
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

    /// The label reaches the row, and a MISSING one falls back without
    /// dropping the row.
    ///
    /// The fallback is the half worth pinning: the label is a display column,
    /// so losing the contract's VOLUME row over it would repeat the mistake
    /// `GainUnavailable` was corrected for on 2026-09-12. It must be COUNTED
    /// though — the two snapshots are built from the same artifact rows in the
    /// same pass, so a miss is real drift and nothing else would say so.
    #[test]
    fn a_missing_label_falls_back_and_is_counted_but_never_drops_the_row() {
        let ranked = [contract(10, 1, 500)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| true,
            NO_LABEL,
            NO_CANDLE,
        );
        assert_eq!(p.rows.len(), 1, "a missing label must never delete the row");
        assert_eq!(p.rows[0].contract, UNLABELLED_CONTRACT);
        assert_eq!(p.refusals, vec![(10, SnapshotRefusal::LabelUnavailable)]);

        // And the ordinary case: the label is carried through verbatim.
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| true,
            TEST_LABEL,
            NO_CANDLE,
        );
        assert_eq!(p.rows[0].contract, "RELIANCE-25Sep2026-1400-CE");
        assert!(p.refusals.is_empty());
    }

    /// The stored percentage is the exact transform of the stored key, and
    /// carries the sign that makes the change form worth storing.
    #[test]
    fn the_projected_percentage_is_the_transform_of_the_projected_key() {
        for milli in [0_i64, 500, 1_000, 42_500, 4_294_967_295_000] {
            assert_eq!(
                net_volume_chg_milli_pct(milli),
                Some(milli * 100 - 100_000),
                "the stored percentage must follow from the stored key"
            );
        }
        // One lot is exactly zero; below one lot is negative.
        assert_eq!(net_volume_chg_milli_pct(1_000), Some(0));
        assert!(net_volume_chg_milli_pct(500).is_some_and(|v| v < 0));
        // Refused rather than wrapped — a wrapped percentage stored as the
        // reason a contract ranked first is worse than an absent row.
        assert_eq!(net_volume_chg_milli_pct(i64::MAX), None);
    }

    #[test]
    fn as_str_labels_are_distinct_for_every_refusal_reason() {
        // ⚠ DERIVED from `ALL` since 2026-09-19, never hand-listed. The
        // hand-listed form named `GainUnavailable`, and when that variant was
        // deleted the test stopped compiling -- which is the loud failure.
        // The quiet one is the other direction: a variant ADDED to `ALL` and
        // not to the list would leave its label unchecked while the test
        // reported every reason distinct. `ALL` is the enum's own manifest,
        // so reading it closes both directions at once.
        let labels = SnapshotRefusal::ALL.map(SnapshotRefusal::as_str);
        let mut sorted = labels;
        sorted.sort_unstable();
        sorted.iter().zip(sorted.iter().skip(1)).for_each(|(a, b)| {
            assert_ne!(a, b, "refusal labels must be distinct: {labels:?}");
        });
        // Non-vacuous: a one-element array has no adjacent pair, so the loop
        // above would pass over an enum that had lost every reason but one.
        assert!(labels.len() >= 2);
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
    // ---- the three fold columns (operator, 2026-09-18) ----------------
    //
    // *"i clelary told you to precisely have the same volume even in top
    // volume also as simialr to candles tables volume rigth dude … it shodu
    // lbe precise as it is evenw ith minus also"* and, the same day, *"why i
    // cant see any normal percnetage change and volume percentage change
    // columns sepaartely"*.

    /// A helper that parameterises the ONE field its callers still vary --
    /// the bar's signed volume.
    ///
    /// ⚠ RENAMED from `candle_advanced_by` to `candle_with_volume` on 2026-09-19. It used to take a
    /// bucket-advance and a bar-over-bar percentage too, and both went with
    /// the columns the operator stripped off `top_volume` that day. A helper
    /// that still named a parameter nothing stores would read, to the next
    /// author, as a column that exists.
    fn candle_with_volume(
        signed_volume: i64,
    ) -> impl Fn(u64, ExchangeSegment, u32) -> Option<CandleBarReading> {
        move |_id, _segment, _window| {
            Some(CandleBarReading {
                signed_volume,
                // The two day-scoped percentages are FIXED here and match
                // `test_candle` exactly, so a caller can assert a passthrough
                // column by value without restating numbers it does not care
                // about.
                close_pct_from_prev_day: 1.63,
                open_pct: 1.12,
            })
        }
    }

    // ⚠ REMOVED 2026-09-18 — `row_ts_secs` re-derived the row's stamp so the
    // fixtures could compute a bar-open that would land on a given skew.
    // `bar_for_window` resolves the window by integer equality, so a fixture
    // cannot express a mismatched one, and since the 2026-09-19 strip there is
    // no skew column to land on either. Nothing called it, and a dead helper
    // in a test module is a warning, not a spare.

    #[test]
    fn the_fold_reading_lands_in_the_three_candle_columns() {
        let ranked = vec![contract(10, 1, 500)];
        let p = project_snapshot(
            10 * NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| false,
            TEST_LABEL,
            candle_with_volume(-4_242),
        );
        let row = &p.rows[0];
        assert_eq!(
            row.volume,
            Some(-4_242),
            "the candles table's SIGNED volume must reach the column verbatim, \
             minus and all — that is the whole of the operator's ask"
        );
        assert!(
            (row.percentage_change.expect("day change present") - 1.63).abs() < f64::EPSILON,
            "the bar's close-vs-yesterday move must reach its own column"
        );
        assert!(
            (row.open_percentage_change.expect("open change present") - 1.12).abs() < f64::EPSILON,
            "the bar's close-vs-session-open move must reach its own column"
        );
    }

    #[test]
    fn the_three_candle_columns_are_null_together_when_the_fold_has_no_slot() {
        let ranked = vec![contract(10, 1, 500)];
        let p = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| false,
            TEST_LABEL,
            NO_CANDLE,
        );
        let row = &p.rows[0];
        assert_eq!(row.volume, None);
        assert_eq!(row.percentage_change, None);
        assert_eq!(row.open_percentage_change, None);
        // And the ROW still exists. A missing fold reading is a NULL column,
        // never a dropped row — the `gain_pct` lesson of 2026-09-12, where an
        // unknowable leaf column cost the table the volume it exists to record.
        assert_eq!(p.rows.len(), 1);
    }

    // ⚠ RETIRED 2026-09-18 — `a_bar_that_never_opened_is_refused_rather_than_
    // stored_as_a_zero_skew` tested a shape this layer can no longer be
    // handed. It fed a reading whose bar-open was the fold's never-opened
    // sentinel (`bucket_start_ist_secs == 0`) and asserted the projection
    // refused it.
    //
    // The projection no longer receives a bar-open at all. The probe resolves
    // the row's own window by integer equality inside
    // `MultiTfAggregator::bar_for_window`, which returns `None` when neither
    // the open bucket nor the last sealed bucket matches — and a never-opened
    // cell carries `0`, which can never equal a real row's window. So the
    // refusal moved DOWN a layer and became structural rather than a
    // downstream check.
    //
    // The replacement lives where the decision now is:
    // `multi_tf_aggregator::tests::bar_for_window_refuses_a_never_opened_cell`.
    // Recorded rather than deleted, because a reader finding the test gone
    // would reasonably suspect the guarantee went with it.

    // ⚠ RETIRED 2026-09-19 — `the_advance_column_says_open_or_sealed_and_
    // never_a_wrong_window` asserted the bucket-skew column, and the operator
    // removed that column from `top_volume` on 2026-09-19. There is nothing
    // left in this table to tell an OPEN bar from a SEALED one, which is a
    // real narrowing and is recorded here rather than in a passing test: a
    // cross-verification wanting only final numbers can no longer filter on
    // it from the row alone.
    //
    // What did NOT go with it is the structural half, which is where the
    // guarantee actually lives: `bar_for_window` still resolves the row's own
    // window by integer equality and returns `None` otherwise, so the
    // projection is handed the right bar or nothing — never a bar for the
    // wrong interval. That is tested at
    // `multi_tf_aggregator::tests::bar_for_window_refuses_a_never_opened_cell`.

    // ⚠ RETIRED 2026-09-19 — `a_non_finite_price_change_never_reaches_the_
    // column` guarded the bar-over-bar percentage, removed the same day. Its
    // property is NOT lost: the two surviving percentages carry the identical
    // finite gate, and `project_snapshot_nulls_a_nan_or_infinite_candle_
    // percentage` above feeds an Inf and a -Inf through both of them and
    // asserts the volume beside them survives. Deleting a test whose subject
    // is gone while its sibling covers the class is the honest move; keeping
    // two names for one assertion is not.

    /// The operator's constraint on this change, in a test:
    ///
    /// *"bro just keep evrythign … as logn as we confirm that manually and
    /// cross evrify on modnay dont remvoe anythign dude okay?"*
    ///
    /// The SORT KEY and the lot arithmetic may not move when the fold reading
    /// arrives. That ordering is what depth steering reads, and a change that
    /// only ADDS a column must leave it byte-identical.
    ///
    /// ⚠ NARROWED 2026-09-19 — this test also asserted `cumulative_day_volume`
    /// and `delta_units` were unmoved. Both columns LEFT the table the same
    /// day (the operator stripped `top_volume` to the ranking columns), so
    /// there is nothing left to compare; the assertion is dropped rather than
    /// repointed at a survivor, which would have claimed coverage it no longer
    /// has. What the test still proves is the half that matters: the sort key
    /// is untouched by the fold reading.
    #[test]
    fn the_fold_reading_changes_none_of_the_existing_volume_columns() {
        let ranked = vec![contract(10, 1, 500), contract(11, 1, 400)];
        let without = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| true,
            TEST_LABEL,
            NO_CANDLE,
        );
        let with = project_snapshot(
            NANOS_PER_SECOND,
            SnapshotCadence::OneSecond,
            OptionFamily::Stock,
            &ranked,
            |_, _| true,
            TEST_LABEL,
            TEST_CANDLE,
        );
        assert_eq!(without.rows.len(), with.rows.len());
        for (a, b) in without.rows.iter().zip(with.rows.iter()) {
            assert_eq!(a.snapshot_ts_ist_nanos, b.snapshot_ts_ist_nanos);
            assert_eq!(a.security_id, b.security_id);
            assert_eq!(a.per_lot_quantity, b.per_lot_quantity);
            assert_eq!(
                a.total_lots_traded, b.total_lots_traded,
                "the lot count must not move"
            );
            assert_eq!(
                a.volume_percentage_change, b.volume_percentage_change,
                "THE SORT KEY must not move — the ordering is what depth \
                 steering reads, and this change is additive"
            );
            assert_eq!(a.subscribed, b.subscribed);
        }
        // And the fold reading really did arrive in the second projection —
        // otherwise this test would pass by both sides being empty.
        assert_eq!(with.rows[0].volume, Some(-4_242));
        assert_eq!(without.rows[0].volume, None);
    }

    // -- from_bar (2026-09-22, hostile finding B1) -------------------------

    /// The sweep fires AT the window close, so the bar it probes is usually
    /// still the OPEN bucket, whose two percentages are unstamped (0.0). A
    /// field-by-field copy stored that 0.0 as a real "unchanged on the day"
    /// while the candle row for the same bar later carried the true figure.
    /// `from_bar` must stamp the copy with the seal's own function.
    #[test]
    fn from_bar_stamps_an_open_bucket_rather_than_copying_its_zeros() {
        use tickvault_trading::candles::LiveCandleState;
        let mut open = LiveCandleState::empty();
        open.close = 110.0;
        open.prev_day_close = 100.0;
        open.session_open = 105.0;
        // What an OPEN bucket carries: the seal has not stamped it yet.
        open.close_pct_from_prev_day = 0.0;
        open.open_pct = 0.0;

        let r = CandleBarReading::from_bar(open);
        assert_eq!(r.close_pct_from_prev_day, 10.0, "110 vs 100 is +10%");
        assert_eq!(r.open_pct, 4.76, "110 vs 105 is +4.76% (2 dp)");

        // It must equal what the seal itself stamps on the same bar, so the
        // two tables agree by construction.
        let mut sealed = open;
        sealed.stamp_seal_percentages();
        assert_eq!(r.close_pct_from_prev_day, sealed.close_pct_from_prev_day);
        assert_eq!(r.open_pct, sealed.open_pct);
        assert_eq!(r.signed_volume, sealed.signed_volume());
    }

    /// Idempotent on an already-sealed bar: stamping twice changes nothing.
    #[test]
    fn from_bar_is_idempotent_on_a_sealed_bar() {
        use tickvault_trading::candles::LiveCandleState;
        let mut bar = LiveCandleState::empty();
        bar.close = 95.0;
        bar.prev_day_close = 100.0;
        bar.session_open = 100.0;
        bar.stamp_seal_percentages();
        let r = CandleBarReading::from_bar(bar);
        assert_eq!(r.close_pct_from_prev_day, -5.0);
        assert_eq!(r.open_pct, -5.0);
    }

    /// The feed stack's probe closure must go through `from_bar`. A revert
    /// to a field-by-field struct literal re-opens B1 and compiles cleanly,
    /// so it is pinned at the source.
    #[test]
    fn the_feed_stack_probe_reads_the_candle_through_from_bar() {
        let src = include_str!("dhan_feed_stack.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(
            prod.contains(".map(crate::top_volume_snapshot::CandleBarReading::from_bar)"),
            "the top_volume candle probe must stamp via from_bar"
        );
        assert!(
            !prod.contains("crate::top_volume_snapshot::CandleBarReading {"),
            "a field-by-field CandleBarReading literal copies the open bucket's unstamped zeros"
        );
    }
}
