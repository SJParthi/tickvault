//! The option-contract ranking TYPES and pure helpers the depth steering code
//! reads: the option family, a ranked contract row, the distinct-underlying
//! pass and the gainer-eligibility helpers.
//!
//! # Why this module exists (2026-10-22, plan item 62)
//!
//! These lived in `volume_leaderboard`, which ranked option contracts by
//! traded volume for the four `top_volume_<tf>` tables and handed the same
//! board to the depth-20 and depth-200 candidate lists. Operator Quote 34
//! (2026-10-21, `websocket-connection-scope-lock.md` "2026-10-21 (EVENING) —
//! CANDLES_3S, CANDLES_5S AND TOP_VOLUME ARE REMOVED") removed `top_volume`
//! and the ranking that only fed it; the depth-200 choice it also fed was
//! switched off the same day (`[dhan_universe] depth_sockets_enabled =
//! false`). That section deletes no depth code, so the pieces the depth
//! planners and the contract map still name moved here, unchanged:
//!
//! | piece | read by |
//! |---|---|
//! | [`OptionFamily`] | `contract_underlying_map`, `depth200_min_hold`, `dhan_live_crossverify_boot` |
//! | [`RankedContract`], [`distinct_underlying_over`] | `depth200_candidates` |
//! | [`pre_register_gainer_filter_counter`] | `depth200_ranked_steer` |
//! | [`eligible_gain_pct`], [`MAX_PLAUSIBLE_GAIN_PCT`] | `prev_close_store`, `depth20_name_board` |
//! | the gainer verdict and walk | kept with them; their only production caller was the removed sweep |
//!
//! **Honest consequence:** nothing publishes the depth-20 / depth-200 ranked
//! candidate lists any more. Depth is off; if it is turned back on, its
//! ranked steering has no input until a new ranking is authorized by a fresh
//! dated quote.
//!
//! This module is pure: no I/O, no clock, no database.

use std::collections::HashMap;

use tickvault_common::types::ExchangeSegment;

/// The largest `k` [`distinct_underlying_over`] will pre-size for. 25,000, the
/// per-family contract cap the removed leaderboard used, so an absurd caller
/// `k` clamps instead of aborting the process on a capacity overflow.
pub const MAX_TRACKED_CONTRACTS: usize = 25_000;

/// Which option family a contract belongs to.
///
/// The two are ranked in separate leaderboards because their volumes are not
/// comparable — see the module header. This is the same reasoning the depth-200
/// selector already applies one level up, where cross-underlying ranking is
/// normalised rather than raw rupees because a 24,000 index and a 57,000 index
/// are not comparable in absolute terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OptionFamily {
    /// `OPTIDX` — NIFTY, BANKNIFTY and the other index chains.
    Index,
    /// `OPTSTK` — the only family depth subscribes as of the 2026-09-06 lock.
    Stock,
}

impl OptionFamily {
    /// Metric label.
    ///
    /// NOTE: being a `&'static str` is NOT what makes a label cheap.
    /// `metrics::counter!` selects its zero-allocation arm on the label value
    /// being a LITERAL; a variable of any type falls through to the arm that
    /// builds a `Vec`. The counters are therefore resolved ONCE into handles on
    /// the removed leaderboard's `Family` rather than being re-macro'd per call.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Stock => "stock",
        }
    }
}

/// The I-P1-11 composite identity of a contract.
///
/// `security_id` ALONE is not unique — Dhan reuses one numeric id across
/// segments, so id 13 is NIFTY in `IDX_I` and an unrelated cash stock in
/// `NSE_EQ`. Keying on the bare id would silently merge two instruments'
/// volumes into one leaderboard entry.
pub type ContractKey = (u64, ExchangeSegment);

/// The board's order: most milli-lots in the window first, then
/// `security_id`, then segment. A TOTAL order over distinct keys, so an
/// unstable sort, a merge sort and a sort done in slices all produce the same
/// board — which is what lets the drain sort in slices (audit PR4).
#[must_use]
pub fn board_order(a: &RankedContract, b: &RankedContract) -> std::cmp::Ordering {
    b.window_lots_milli
        .cmp(&a.window_lots_milli)
        .then_with(|| a.security_id.cmp(&b.security_id))
        .then_with(|| (a.segment as u8).cmp(&(b.segment as u8)))
}

/// One contract's ranking row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankedContract {
    /// The contract's own Dhan `SecurityId` — not the underlying's.
    pub security_id: u64,
    /// The contract's segment. Part of the identity, never dropped.
    pub segment: ExchangeSegment,
    /// The UNDERLYING's `SecurityId`, carried for the depth-200
    /// distinct-underlying constraint and for gainer eligibility.
    pub underlying_id: u64,
    /// Cumulative traded volume for the day, as last accepted.
    ///
    /// This is the OBSERVATION — what the vendor's counter reads. It is what
    /// the monotonicity gate guards. Since 2026-09-07 it is no longer the sort
    /// key, and since audit PR4c-2 (2026-09-26) nothing is measured against it
    /// either: the window's volume is read from the candle bar.
    pub volume: u32,
    /// The RANK KEY: lots traded in the window that just closed, × 1000.
    ///
    /// **Written by `VolumeLeaderboard::rank` (removed 2026-10-22), ignored by
    /// `VolumeLeaderboard::observe` (removed 2026-10-22).** A caller constructing an observation
    /// leaves it 0 and the ranking overwrites it; a value set here on the way
    /// IN has no effect and is not read.
    ///
    /// Milli-lots rather than whole lots because a 250-deep board is decided at
    /// its bottom edge: with integer lots every contract trading under one lot
    /// in a one-second window collapses to 0 and the cut becomes an arbitrary
    /// tie-break rather than a measurement. See the 2026-09-07 scope-lock
    /// section.
    pub window_lots_milli: u64,
    /// The rank key's NUMERATOR: units traded in the window that just closed.
    ///
    /// **Written by `VolumeLeaderboard::rank` (removed 2026-10-22), ignored by
    /// `VolumeLeaderboard::observe` (removed 2026-10-22)** — the same contract as
    /// `window_lots_milli` above, for the same reason: it is an output of
    /// the ranking pass, not an observation, and a value set on the way IN
    /// is overwritten.
    ///
    /// Carried so the persisted row can show the division rather than only
    /// its result. Since audit PR4c-2 it is the volume of the contract's
    /// candle bar for the window (it was `volume − baseline` until then).
    pub delta_units: u32,
    /// The rank key's DENOMINATOR: units per contract, as the ranking used
    /// it.
    ///
    /// Also written by `rank`, from the SAME `lot_of` probe that fed the
    /// division — never a second lookup, which could disagree with the
    /// number actually divided by and would make the stored row a plausible
    /// lie rather than a record.
    pub lot_size: u32,
}

/// Greedy "at most one contract per underlying" pass over an already-ordered
/// ranking.
///
/// The depth-200 rule: operator, 2026-09-06, *"ensure on depth 200 top 5
/// should never ever be same symbols strike"*. Greedy over the volume order,
/// so each underlying is represented by its heaviest contract. **The honest
/// cost, which the scope lock records:** when the true top five are five
/// strikes of one stock, this forces four substitutions into thinner books —
/// the shape that produced 800 rows/minute against 100,800 on 2026-08-26.
///
/// A free function over an ALREADY-ranked slice, deliberately — the
/// production 5-second arm ranks the full population ONCE and applies this
/// to that slice. It was extracted on 2026-09-08 from a `rank_distinct_underlying`
/// method that ranked internally, and that method was deleted the same day
/// once its only callers were its own tests, because "without ranking again"
/// is the load-bearing half. `VolumeLeaderboard::rank` (removed 2026-10-22) drains the cadence's
/// work list as it sweeps, so a second `rank` on the same cadence inside one
/// window finds the list already consumed and ranks almost nothing: a
/// confident, completely wrong board with no counter to show for it, which
/// is the class this repository keeps removing. (Until audit PR4c-2 the same
/// mistake read zero deltas off rolled baselines; the shape is unchanged.)
///
/// `ordered` must already be sorted best-first; this function does not sort and
/// makes no attempt to check, because the only honest check is the sort itself.
///
/// O(`ordered.len()` × k) — the outer loop walks `ordered`, and each row costs a
/// linear `contains` over the underlyings already picked, which is bounded by
/// `k`. On the depth-200 path `ordered` is the gainer-eligible output (≤ 300)
/// and `k` is `DEPTH200_EXIT_UNDERLYINGS` (20), so ≈ 6,000 integer compares once
/// a minute. Still cheaper than building a 20-entry `HashSet`.
///
/// ⚠ CORRECTED 2026-09-13: this read "O(k × distinct-seen) with both bounded by
/// `k`. `k` is 5 on the depth-200 path" — wrong twice. The outer loop is over
/// `ordered`, not `k`; and `k` became 20 when `DEPTH200_HYSTERESIS_RANKS`
/// widened 3 → 15 on 2026-09-11. Claimed ~25 compares against a real ~6,000:
/// understated ~240x, in the reassuring direction. CLAUDE.md's own O(1) table
/// has carried the correct figure since that widening — this docstring was the
/// stale copy, which is the direction that matters least to a reader of the
/// table and most to a reader of the code.
#[must_use]
pub fn distinct_underlying_over(ordered: &[RankedContract], k: usize) -> Vec<RankedContract> {
    // CLAMPED before the allocations. `Vec::with_capacity(k)` PANICS on a
    // capacity overflow and the release profile is `panic = "abort"`, so an
    // unclamped caller-supplied `k` is a process abort rather than a bad
    // ranking.
    let k = k.min(MAX_TRACKED_CONTRACTS);
    let mut seen: Vec<u64> = Vec::with_capacity(k);
    let mut out: Vec<RankedContract> = Vec::with_capacity(k);
    for row in ordered {
        if out.len() >= k {
            break;
        }
        if seen.contains(&row.underlying_id) {
            continue;
        }
        seen.push(row.underlying_id);
        out.push(*row);
    }
    out
}

// ---------------------------------------------------------------------------
// Gainer eligibility (2026-09-06 lock: gainer = filter, volume = order)
// ---------------------------------------------------------------------------

/// The segment a STOCK option's underlying trades in.
///
/// The [`OptionFamily::Stock`] board ranks `NSE_FNO` contracts whose
/// underlying is an NSE cash equity — the same `(underlying, NSE_EQ)` key the
/// spot store and the previous-close store hold. Index options are never on
/// the depth path (the lock bans them), so this is the only underlying
/// segment the eligibility filter needs, and naming it once keeps the two
/// lookups below from ever disagreeing about which store row is "the
/// underlying".
///
/// # Why there is no family-derived form of this
///
/// Until 2026-09-19 a `underlying_segment(family)` helper sat here, mapping
/// `Index` to `IDX_I` and `Stock` to this constant, so that the persisted
/// row's `gain_pct` could never probe an index id in the equity segment.
/// Both of its reasons are now gone: the operator's 2026-09-18 ruling removed
/// `gain_pct` from the row entirely, and the same day's stock-options-only
/// ruling removed the Index family from `top_volume`. The helper's last call
/// site went with `gain_pct`, and the pub-fn wiring guard flagged it as
/// dormant on the next push.
///
/// The hazard it guarded is REAL and is recorded here so nothing re-derives a
/// segment casually: an index option's `underlying_id` is an index id
/// (NIFTY=13, BANKNIFTY=25, SENSEX=51), and probing `(13, NSE_EQ)` is exactly
/// the I-P1-11 collision this repository bans. At best it finds nothing; at
/// worst an NSE cash equity carries id 13 — low ids are where equities live —
/// and the row is ACCEPTED carrying that stock's percentage under an index's
/// name, with no NaN to catch it and no refusal counter moving. If a future
/// change puts index options back on any path that pairs an underlying id
/// with a segment, derive the segment from the family rather than reaching
/// for this constant.
pub const STOCK_OPTION_UNDERLYING_SEGMENT: ExchangeSegment = ExchangeSegment::NseEquity;

/// Metric: how the gainer filter judged each ranked contract's underlying,
/// per 5-second pass. Labels are [`GAINER_VERDICT_LABELS`]. Local exporter
/// only — never EMF-shipped (three series a human reads while already
/// looking at the console; the cardinality rule in the noise lock §2.3).
pub const GAINER_FILTER_COUNTER: &str = "tv_depth200_gainer_filter_total";

/// Every label value [`GAINER_FILTER_COUNTER`] can carry, for pre-registration
/// (the CloudWatch agent drops the first sample of an unseen series).
pub const GAINER_VERDICT_LABELS: [&str; 3] = ["gainer", "not_gainer", "unknown"];

/// Whether an underlying counts as a gainer for depth eligibility.
///
/// The 2026-09-06 lock: *"An instrument qualifies if its underlying is in the
/// day's gainers; volume then decides the order."* A contract on a FALLING
/// stock is not a candidate however busy it is; a contract whose underlying
/// cannot be judged is not a candidate either, and is counted separately so
/// a day where nothing can be judged (no previous closes arrived) reads as
/// `unknown`, never as "no gainers".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GainerVerdict {
    /// Spot is strictly above the previous close.
    Gainer,
    /// Spot is at or below the previous close.
    NotGainer,
    /// Either input is absent or not a price (no spot today, no previous
    /// close, non-finite, non-positive).
    Unknown,
}

impl GainerVerdict {
    /// The metric label for this verdict.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gainer => "gainer",
            Self::NotGainer => "not_gainer",
            Self::Unknown => "unknown",
        }
    }
}

/// Judges one underlying from the RAM spot price (paise, as the spot store
/// holds it) and the previous close (rupees, as the prev-close store holds
/// it).
///
/// The comparison is done in INTEGER paise on both sides, through the same
/// `rupees_to_paise` the ladder-centring path uses — so a previous close that
/// cannot centre a ladder cannot make a gainer either, and no float ever
/// enters the decision. Zero and negative previous closes are the documented
/// sentinels and land in `Unknown`, never in `NotGainer`.
#[must_use]
pub fn underlying_gainer_verdict(
    spot_paise: Option<i64>,
    prev_close: Option<f64>,
) -> GainerVerdict {
    let (Some(spot), Some(close_paise)) = (
        spot_paise,
        prev_close.and_then(crate::spot_price_store::rupees_to_paise),
    ) else {
        return GainerVerdict::Unknown;
    };
    if spot <= 0 {
        return GainerVerdict::Unknown;
    }
    if spot > close_paise {
        GainerVerdict::Gainer
    } else {
        GainerVerdict::NotGainer
    }
}

/// The UNDERLYING's percent change — the same number, from the same two
/// inputs, that [`underlying_gainer_verdict`] turns into a boolean.
///
/// # Why this exists and why it takes the verdict's inputs, not its own
///
/// The persisted `top_volume_rank.gain_pct` and the gainer FILTER must never
/// be able to disagree. If the column were computed from a different source
/// than the verdict, a row could read `+5.0` and still have been filtered out
/// as not-a-gainer, and no reader could tell which half was wrong. Taking the
/// identical `(spot_paise, prev_close)` pair makes that contradiction
/// unrepresentable: the sign of this value IS the verdict.
///
/// # Why the UNDERLYING and not the contract
///
/// Until 2026-09-09 the snapshot asked for the CONTRACT's own previous close.
/// Nothing has ever written one — `record_prev_close_from_tick` stores only
/// [`STOCK_OPTION_UNDERLYING_SEGMENT`] — so the probe returned `None` on every
/// row, became `NaN`, and the projection refused the row as `NonFiniteGain`.
/// **Every row of every snapshot was dropped, so the table was empty for the
/// whole session while every counter read healthy.**
///
/// Feeding the underlying is also what [`eligible_gain_pct`] was written for:
/// its own [`MAX_PLAUSIBLE_GAIN_PCT`] doc says a deep-out-of-the-money option
/// "can move that far" and is "not what this function is fed". So the old call
/// site was wrong twice — a NaN in the ordinary case, and a bound that would
/// have refused a legitimately explosive strike in the case where a contract
/// close did exist.
///
/// O(1): two `Option` unwraps, one integer compare, one divide. Cold — once
/// per ranked row on the snapshot arm, never per tick.
#[must_use]
pub fn underlying_gain_pct(spot_paise: Option<i64>, prev_close: Option<f64>) -> Option<f64> {
    let (Some(spot), Some(close_paise)) = (
        spot_paise,
        prev_close.and_then(crate::spot_price_store::rupees_to_paise),
    ) else {
        return None;
    };
    if spot <= 0 || close_paise <= 0 {
        return None;
    }
    // Paise on BOTH sides. A ratio is scale-invariant, so this is the same
    // percentage the rupee values give, without a second rounding. Both are
    // bounded by `rupees_to_paise`'s own ceiling, far inside f64's exact
    // integer range, so neither widening loses a digit.
    eligible_gain_pct(spot as f64, close_paise as f64)
}

/// How many underlyings fell into each verdict during one filter pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GainerTally {
    /// Contracts whose underlying is a gainer — the eligible set.
    pub gainer: usize,
    /// Contracts whose underlying is flat or falling.
    pub not_gainer: usize,
    /// Contracts whose underlying could not be judged.
    pub unknown: usize,
}

/// Keeps the first `limit` ranked contracts whose underlying `verdict_of` calls a
/// gainer, PRESERVING the volume order. Pure.
///
/// `verdict_of` is called once per DISTINCT underlying visited (memoised —
/// the ~102 strikes of one stock share a verdict), so the caller pays one
/// spot-store probe and one prev-close probe per underlying, and the walk
/// STOPS once `limit` gainers are collected — so on an ordinary day it visits
/// a few hundred rows of a population that may be thousands. The tally counts
/// rows visited, not probes made. **The honest worst case, which is a real
/// day and not a corner:** every stock falling, so nothing is ever collected
/// and the walk runs to the END of the ranked population — O(n) in the traded
/// population with one hash probe per row, MEASURED by
/// `gainer_eligible_sweep_cost_at_the_authorized_ceiling`. Cold: once per
/// 5-second sweep on the drain's timer arm, never per tick. (Until 2026-09-08
/// the probes were per ROW, so that same down day cost ~40,000 store probes
/// per sweep; the memo caps them at the underlying count whatever the day.)
///
/// Applied AFTER the volume sort rather than inside `rank` deliberately: the
/// persisted `top_volume_rank` rows must keep recording which contracts were
/// busiest whether or not their stock rose, so the filter narrows only what
/// the DEPTH steering is handed, never the record.
#[must_use]
pub fn gainer_eligible<F>(
    ranked: &[RankedContract],
    limit: usize,
    verdict_of: F,
) -> (Vec<RankedContract>, GainerTally)
where
    F: Fn(u64) -> GainerVerdict,
{
    // The one walk, run to completion. The drain runs the SAME walk in
    // slices through `GainerWalk` (audit PR4), so the two cannot disagree
    // about which contracts qualify.
    let mut walk = GainerWalk::with_capacity(ranked.len().min(limit));
    walk.reset(limit);
    let _done = walk.step(ranked, usize::MAX, verdict_of);
    (walk.out, walk.tally)
}

/// [`gainer_eligible`] as a resumable walk, so the drain can run it a bounded
/// number of rows at a time instead of in one pass over up to ~20,000 rows.
///
/// The per-underlying verdict memo is carried across steps, so the probe
/// bound is the same as the one-pass walk's: at most one verdict per
/// underlying per sweep. Buffers are reused across sweeps — `reset` clears
/// them and keeps their capacity.
#[derive(Debug)]
pub struct GainerWalk {
    out: Vec<RankedContract>,
    tally: GainerTally,
    /// One verdict per UNDERLYING, not per row. A stock's ~102 strikes share
    /// one spot and one previous close, so the verdict is identical for every
    /// one of them; without the memo a down day — every underlying falling,
    /// nothing collected, the walk running to the END of the population —
    /// paid two store probes per row across all ~20,000. The memo caps the
    /// probes at the number of underlyings (~210) whatever the day looks like.
    /// Sized for the measured F&O underlying count; a larger day grows it
    /// once, and `reset` keeps the grown capacity.
    memo: HashMap<u64, GainerVerdict>,
    pos: usize,
    limit: usize,
}

impl GainerWalk {
    /// A walk whose output buffer holds `out_capacity` rows without growing.
    #[must_use]
    pub fn with_capacity(out_capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(out_capacity),
            tally: GainerTally::default(),
            memo: HashMap::with_capacity(GAINER_MEMO_CAPACITY),
            pos: 0,
            limit: 0,
        }
    }

    /// Starts a new walk that stops once `limit` gainers are collected.
    pub fn reset(&mut self, limit: usize) {
        self.out.clear();
        self.tally = GainerTally::default();
        self.memo.clear();
        self.pos = 0;
        self.limit = limit;
    }

    /// Visits at most `budget` more rows of `ranked`. Returns `true` when
    /// the walk is finished — `limit` gainers found or the rows exhausted.
    /// `ranked` must be the same slice on every step of one walk.
    pub fn step<F>(&mut self, ranked: &[RankedContract], budget: usize, verdict_of: F) -> bool
    where
        F: Fn(u64) -> GainerVerdict,
    {
        let mut visited = 0usize;
        while visited < budget {
            if self.out.len() >= self.limit {
                return true;
            }
            let Some(row) = ranked.get(self.pos) else {
                return true;
            };
            self.pos = self.pos.saturating_add(1);
            visited = visited.saturating_add(1);
            let verdict = *self
                .memo
                .entry(row.underlying_id)
                .or_insert_with(|| verdict_of(row.underlying_id));
            match verdict {
                GainerVerdict::Gainer => {
                    self.tally.gainer = self.tally.gainer.saturating_add(1);
                    self.out.push(*row);
                }
                GainerVerdict::NotGainer => {
                    self.tally.not_gainer = self.tally.not_gainer.saturating_add(1);
                }
                GainerVerdict::Unknown => self.tally.unknown = self.tally.unknown.saturating_add(1),
            }
        }
        self.out.len() >= self.limit || self.pos >= ranked.len()
    }

    /// The gainers collected so far, in board order.
    #[must_use]
    pub fn gainers(&self) -> &[RankedContract] {
        &self.out
    }

    /// The verdict tally so far.
    #[must_use]
    pub const fn tally(&self) -> GainerTally {
        self.tally
    }
}

/// Pre-size for the per-underlying verdict memo in [`gainer_eligible`]:
/// the 2026-08-21 measured F&O underlying count (208) rounded up. A larger
/// population grows the map once per call, on the cold sweep, and is not a
/// refusal.
pub const GAINER_MEMO_CAPACITY: usize = 256;

/// The three verdict handles, resolved ONCE.
///
/// # Why a `OnceLock` and not `metrics::counter!` at the call site
///
/// `metrics::counter!(NAME, "k" => v)` with a NON-LITERAL label value drops to
/// the macro's ALLOCATING arm — it builds a `Vec<Label>` per call. This module
/// already documents that hazard twice (the `record_ws_lag` class, ~36M
/// allocations an hour), and `record_gainer_tally` was the one site left doing
/// it: three allocations per 5-second pass per family, on the drain's timer arm.
///
/// Small — ~0.6 allocations a second, not the 36M/hour the header warns about —
/// and fixed anyway, because the cost of being inconsistent here is that the
/// next reader takes the call site as permission rather than the doc as the
/// rule. Resolution is one `OnceLock` read; the handles themselves are atomics.
static GAINER_VERDICT_COUNTERS: std::sync::OnceLock<[metrics::Counter; 3]> =
    std::sync::OnceLock::new();

/// Resolves (once) the three verdict handles, in [`GAINER_VERDICT_LABELS`]
/// order.
fn gainer_verdict_counters() -> &'static [metrics::Counter; 3] {
    GAINER_VERDICT_COUNTERS.get_or_init(|| {
        GAINER_VERDICT_LABELS
            .map(|label| metrics::counter!(GAINER_FILTER_COUNTER, "verdict" => label))
    })
}

/// Publishes one pass's tally onto [`GAINER_FILTER_COUNTER`].
pub fn record_gainer_tally(tally: GainerTally) {
    // Positional, matching `GAINER_VERDICT_LABELS` — pinned by
    // `the_gainer_tally_fields_line_up_with_their_labels` so a reordered label
    // array can never silently credit one verdict's count to another.
    let counters = gainer_verdict_counters();
    for (counter, n) in counters
        .iter()
        .zip([tally.gainer, tally.not_gainer, tally.unknown])
    {
        counter.increment(u64::try_from(n).unwrap_or(u64::MAX));
    }
}

/// Seeds every verdict series at zero so its first real sample is not the
/// one the agent drops.
pub fn pre_register_gainer_filter_counter() {
    for counter in gainer_verdict_counters() {
        counter.increment(0);
    }
}
/// Percent change against the previous close, or `None` when it cannot be
/// computed honestly.
///
/// **The previous close is a PROVEN source of not-a-number** — the quote parser
/// carries a test that asserts `tick.day_close.is_nan()` — and `0.0` is a live
/// sentinel for Ticker-mode packets and pre-open instruments. A non-finite
/// value reaching a sort comparator is worse than one bad row: the comparison
/// is non-transitive, so the ORDERING corrupts wholesale.
///
/// This is the §28.4 poisoning class one module over, gated at ingest rather
/// than clamped at the output — clamping is what let a NaN reach indicator
/// state and sit there for the life of the process.
#[must_use]
pub fn eligible_gain_pct(ltp: f64, prev_close: f64) -> Option<f64> {
    if !ltp.is_finite() || !prev_close.is_finite() || ltp <= 0.0 || prev_close <= 0.0 {
        return None;
    }
    let pct = (ltp - prev_close) / prev_close * 100.0;
    // Finite is not the same as plausible. A previous close of 0.01 against
    // a 240-rupee LTP yields +2,400,000% — finite, sortable, and nonsense —
    // and it was persisted as a DOUBLE until 2026-09-08 (identity sweep row
    // 22). No NSE stock or option moves ten-fold in a session against its
    // own previous close; a value past the bound is a corrupt input, not a
    // move, and is refused the same way a NaN is.
    if pct.is_finite() && pct.abs() <= MAX_PLAUSIBLE_GAIN_PCT {
        Some(pct)
    } else {
        None
    }
}

/// The widest percent move against the previous close that is treated as a
/// real reading rather than a corrupt previous close.
///
/// 1,000% is a ten-fold move in one session. Stocks with F&O have no daily
/// circuit band, but a ten-fold move has never happened on the NSE cash
/// segment in a session, and deep-out-of-the-money OPTIONS — which can move
/// that far — are not what this function is fed: the gainer verdict reads
/// the UNDERLYING's spot against the underlying's previous close.
pub const MAX_PLAUSIBLE_GAIN_PCT: f64 = 1_000.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn stock(id: u64, underlying: u64, volume: u32) -> RankedContract {
        RankedContract {
            security_id: id,
            segment: ExchangeSegment::NseFno,
            underlying_id: underlying,
            volume,
            window_lots_milli: 0,
            delta_units: 0,
            lot_size: 0,
        }
    }

    #[test]
    fn distinct_underlying_over_clamps_k_rather_than_aborting_the_process() {
        // The pure pass, tested directly. The production depth-200 candidate
        // publish applies it to an already-ranked slice (calling `rank` twice
        // in one tick consumes the window and silently collapses the order),
        // so a defect here reaches the five deep sockets directly.
        let row = |sid: u64, underlying: u64| RankedContract {
            security_id: sid,
            segment: ExchangeSegment::NseFno,
            underlying_id: underlying,
            volume: 0,
            window_lots_milli: 0,
            delta_units: 0,
            lot_size: 0,
        };

        // Distinctness: five strikes of one name yield ONE entry.
        let one_name: Vec<RankedContract> = (0..5).map(|s| row(s, 42)).collect();
        let picked = distinct_underlying_over(&one_name, 5);
        assert_eq!(
            picked.len(),
            1,
            "five strikes of one name are one candidate"
        );
        assert_eq!(picked[0].security_id, 0, "the first — i.e. heaviest — wins");

        // Order is preserved from the caller's sort; this fn never re-sorts.
        let many: Vec<RankedContract> = (0..8).map(|s| row(s, s)).collect();
        let five = distinct_underlying_over(&many, 5);
        assert_eq!(
            five.iter().map(|c| c.security_id).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );

        // THE reason the clamp exists. `Vec::with_capacity(k)` panics on a
        // capacity overflow and the release profile is `panic = "abort"`, so
        // an unclamped k from a caller would be a process abort — the whole
        // trading lane — rather than a short list.
        let clamped = distinct_underlying_over(&many, usize::MAX);
        assert_eq!(
            clamped.len(),
            8,
            "an absurd k must clamp and return what exists, never abort"
        );

        // Honest under-fill, and the empty case.
        assert_eq!(distinct_underlying_over(&many, 0).len(), 0);
        assert_eq!(distinct_underlying_over(&[], 5).len(), 0);
    }

    #[test]
    fn eligible_gain_pct_refuses_a_non_finite_or_zero_previous_close() {
        // The quote parser has a test that ASSERTS day_close is NaN, and 0.0 is
        // a live sentinel. A non-finite value in a sort comparator is
        // non-transitive, so the ORDERING corrupts wholesale rather than one
        // row. Gated at ingest, never clamped at the output — clamping is what
        // let a NaN sit in indicator state for the life of the process.
        assert_eq!(eligible_gain_pct(110.0, 100.0), Some(10.0));
        assert_eq!(eligible_gain_pct(90.0, 100.0), Some(-10.0));
        assert_eq!(eligible_gain_pct(100.0, f64::NAN), None, "NaN prev close");
        assert_eq!(eligible_gain_pct(f64::NAN, 100.0), None, "NaN ltp");
        assert_eq!(eligible_gain_pct(100.0, 0.0), None, "the zero sentinel");
        assert_eq!(eligible_gain_pct(0.0, 100.0), None, "zero ltp");
        assert_eq!(eligible_gain_pct(100.0, -1.0), None, "negative prev close");
        assert_eq!(eligible_gain_pct(f64::INFINITY, 100.0), None);
        assert_eq!(eligible_gain_pct(100.0, f64::INFINITY), None);
        // Identity sweep row 22 (2026-09-08): finite but impossible. A
        // previous close of one paisa against a 240-rupee print is a corrupt
        // input, and it used to persist as +2,400,000%.
        assert_eq!(
            eligible_gain_pct(240.0, 0.01),
            None,
            "a corrupt previous close"
        );
        let crash = eligible_gain_pct(0.01, 240.0)
            .expect("a real crash to one paisa is inside the bound and is kept");
        assert!(
            (crash - (-99.995_833_333_333_33)).abs() < 1e-9,
            "a crash is a large NEGATIVE move, never refused for size alone: {crash}"
        );
        assert_eq!(
            eligible_gain_pct(100.0 + MAX_PLAUSIBLE_GAIN_PCT, 100.0),
            Some(MAX_PLAUSIBLE_GAIN_PCT),
            "the bound itself is admitted"
        );
        assert_eq!(
            eligible_gain_pct(100.0 + MAX_PLAUSIBLE_GAIN_PCT + 1.0, 100.0),
            None,
            "one percent past the bound is refused"
        );
    }

    #[test]
    fn underlying_gain_pct_agrees_with_the_verdict_it_shares_inputs_with() {
        // THE property this function exists for: the persisted column and the
        // gainer FILTER read the same two inputs, so a row can never say "+5%"
        // and have been filtered out as not-a-gainer. Sign of the percentage
        // IS the verdict.
        for (spot_paise, close_rupees) in [
            (110_00_i64, 100.0_f64), // up
            (90_00, 100.0),          // down
            (100_00, 100.0),         // flat
        ] {
            let pct = underlying_gain_pct(Some(spot_paise), Some(close_rupees));
            let verdict = underlying_gainer_verdict(Some(spot_paise), Some(close_rupees));
            match verdict {
                GainerVerdict::Gainer => assert!(
                    pct.is_some_and(|p| p > 0.0),
                    "a gainer must carry a positive percent, got {pct:?}"
                ),
                GainerVerdict::NotGainer => assert!(
                    pct.is_some_and(|p| p <= 0.0),
                    "a non-gainer must never carry a positive percent, got {pct:?}"
                ),
                GainerVerdict::Unknown => assert_eq!(pct, None),
            }
        }
    }

    #[test]
    fn underlying_gain_pct_is_none_wherever_the_verdict_is_unknown() {
        // The two must refuse on exactly the same inputs, or the column would
        // carry a number for a row the filter could not judge.
        for (spot, close) in [
            (None, Some(100.0_f64)),
            (Some(110_00_i64), None),
            (Some(0), Some(100.0)),
            (Some(-1), Some(100.0)),
            (Some(110_00), Some(0.0)),
            (Some(110_00), Some(f64::NAN)),
        ] {
            assert_eq!(
                underlying_gainer_verdict(spot, close),
                GainerVerdict::Unknown
            );
            assert_eq!(
                underlying_gain_pct(spot, close),
                None,
                "spot={spot:?} close={close:?}"
            );
        }
    }

    #[test]
    fn underlying_gain_pct_computes_the_same_percent_the_rupee_form_would() {
        // Paise on both sides of the ratio; a ratio is scale-invariant, so the
        // answer must equal the rupee computation to the last bit.
        let paise = underlying_gain_pct(Some(112_35_i64), Some(100.0)).expect("finite");
        let rupees = eligible_gain_pct(112.35, 100.0).expect("finite");
        assert!(
            (paise - rupees).abs() < 1e-9,
            "paise {paise} vs rupees {rupees}"
        );
    }

    #[test]
    fn underlying_gainer_verdict_is_an_integer_paise_compare() {
        use GainerVerdict::{Gainer, NotGainer};
        assert_eq!(underlying_gainer_verdict(Some(11_000), Some(100.0)), Gainer);
        assert_eq!(
            underlying_gainer_verdict(Some(10_000), Some(100.0)),
            NotGainer,
            "flat is not a gainer"
        );
        assert_eq!(
            underlying_gainer_verdict(Some(9_999), Some(100.0)),
            NotGainer
        );
        assert_eq!(
            underlying_gainer_verdict(Some(10_001), Some(100.0)),
            Gainer,
            "one paise above the close is a gainer — the compare is exact, not a float band"
        );
    }

    #[test]
    fn underlying_gainer_verdict_refuses_to_judge_from_a_missing_or_sentinel_input() {
        use GainerVerdict::Unknown;
        assert_eq!(
            underlying_gainer_verdict(None, Some(100.0)),
            Unknown,
            "no spot today"
        );
        assert_eq!(
            underlying_gainer_verdict(Some(11_000), None),
            Unknown,
            "no previous close"
        );
        assert_eq!(
            underlying_gainer_verdict(Some(11_000), Some(0.0)),
            Unknown,
            "the zero sentinel"
        );
        assert_eq!(underlying_gainer_verdict(Some(11_000), Some(-1.0)), Unknown);
        assert_eq!(
            underlying_gainer_verdict(Some(11_000), Some(f64::NAN)),
            Unknown
        );
        assert_eq!(
            underlying_gainer_verdict(Some(11_000), Some(f64::INFINITY)),
            Unknown
        );
        assert_eq!(
            underlying_gainer_verdict(Some(0), Some(100.0)),
            Unknown,
            "a zero spot is an absence"
        );
        assert_eq!(underlying_gainer_verdict(Some(-5), Some(100.0)), Unknown);
    }

    #[test]
    fn gainer_eligible_keeps_the_volume_order_and_drops_falling_and_unknown_underlyings() {
        let ranked = [
            stock(1, 100, 900), // gainer
            stock(2, 200, 800), // falling
            stock(3, 300, 700), // unknown
            stock(4, 100, 600), // gainer, same underlying as #1
            stock(5, 400, 500), // gainer
        ];
        let (eligible, tally) =
            gainer_eligible(&ranked, usize::MAX, |underlying| match underlying {
                100 | 400 => GainerVerdict::Gainer,
                200 => GainerVerdict::NotGainer,
                _ => GainerVerdict::Unknown,
            });
        let ids: Vec<u64> = eligible.iter().map(|c| c.security_id).collect();
        assert_eq!(
            ids,
            vec![1, 4, 5],
            "membership by gain, order by the ranking"
        );
        assert_eq!(
            tally,
            GainerTally {
                gainer: 3,
                not_gainer: 1,
                unknown: 1
            }
        );
        let picked = distinct_underlying_over(&eligible, 5);
        let picked_ids: Vec<u64> = picked.iter().map(|c| c.security_id).collect();
        assert_eq!(
            picked_ids,
            vec![1, 5],
            "after the filter the distinct pass still keeps one contract per underlying"
        );
    }

    #[test]
    fn gainer_eligible_on_a_down_day_is_empty_and_the_tally_says_why() {
        let ranked = [stock(1, 100, 900), stock(2, 200, 800)];
        let (eligible, tally) = gainer_eligible(&ranked, usize::MAX, |_| GainerVerdict::NotGainer);
        assert!(
            eligible.is_empty(),
            "no gainers means no candidates — sockets hold"
        );
        assert_eq!(tally.not_gainer, 2);
        assert_eq!(
            tally.unknown, 0,
            "a down day must not read as an unjudgeable day"
        );
    }

    #[test]
    fn record_gainer_tally_accepts_every_shape_and_label_covers_every_verdict_once() {
        let labels = [
            GainerVerdict::Gainer.label(),
            GainerVerdict::NotGainer.label(),
            GainerVerdict::Unknown.label(),
        ];
        assert_eq!(labels, GAINER_VERDICT_LABELS);
        record_gainer_tally(GainerTally {
            gainer: 1,
            not_gainer: 2,
            unknown: 3,
        });
    }

    #[test]
    fn pre_register_gainer_filter_counter_seeds_every_label_without_panicking() {
        pre_register_gainer_filter_counter();
        pre_register_gainer_filter_counter();
    }

    /// The gainer walk stops once `limit` gainers are collected, and the tally
    /// counts only what was visited — so a 20,000-row population costs a few
    /// hundred probes on an ordinary day, not 40,000.
    #[test]
    fn gainer_eligible_stops_at_the_limit_and_tallies_only_what_it_visited() {
        let ranked: Vec<RankedContract> =
            (1..=10u64).map(|i| stock(i, i, 1_000 - i as u32)).collect();
        let (eligible, tally) = gainer_eligible(&ranked, 3, |u| {
            if u % 2 == 0 {
                GainerVerdict::Gainer
            } else {
                GainerVerdict::NotGainer
            }
        });
        let ids: Vec<u64> = eligible.iter().map(|c| c.security_id).collect();
        assert_eq!(
            ids,
            vec![2, 4, 6],
            "the first three gainers in volume order"
        );
        assert_eq!(tally.gainer, 3);
        assert_eq!(
            tally.not_gainer, 3,
            "rows 1, 3, 5 were visited; 7+ were not"
        );
        assert_eq!(tally.unknown, 0);
    }

    /// The memo: a stock's many strikes share one verdict, so the store is
    /// probed once per underlying, not once per row — and on a down day the
    /// walk still reaches the end of the population without a probe per row.
    #[test]
    fn gainer_eligible_probes_each_underlying_once_however_many_strikes_it_has() {
        use std::cell::Cell;
        // 5 underlyings × 40 strikes, all falling, so nothing is collected and
        // the walk visits every row.
        let ranked: Vec<RankedContract> = (0..200u64)
            .map(|i| stock(i, i % 5, 10_000 - i as u32))
            .collect();
        let probes = Cell::new(0usize);
        let (eligible, tally) = gainer_eligible(&ranked, 300, |_| {
            probes.set(probes.get() + 1);
            GainerVerdict::NotGainer
        });
        assert!(eligible.is_empty());
        assert_eq!(tally.not_gainer, 200, "every row was visited and tallied");
        assert_eq!(
            probes.get(),
            5,
            "but the store was probed once per underlying"
        );
    }

    #[test]
    fn test_gainer_walk_with_capacity_steps_in_slices_and_gainers_match_gainer_eligible() {
        let ranked: Vec<RankedContract> = (1..=900u64).map(|i| stock(i, i % 37, 10)).collect();
        let verdict = |u: u64| match u % 3 {
            0 => GainerVerdict::Gainer,
            1 => GainerVerdict::NotGainer,
            _ => GainerVerdict::Unknown,
        };
        let (expected, expected_tally) = gainer_eligible(&ranked, 250, verdict);
        let mut walk = GainerWalk::with_capacity(250);
        walk.reset(250);
        let mut steps = 0;
        while !walk.step(&ranked, 7, verdict) {
            steps += 1;
        }
        assert!(steps > 1, "the walk really was sliced");
        assert_eq!(walk.gainers(), expected.as_slice());
        assert_eq!(walk.tally(), expected_tally);
        // Reused for a second walk: same answer, buffers kept.
        walk.reset(250);
        while !walk.step(&ranked, 1_000, verdict) {}
        assert_eq!(walk.gainers(), expected.as_slice());
    }

    #[test]
    fn board_order_is_a_total_order_on_distinct_keys() {
        let mut a = stock(1, 1, 1);
        a.window_lots_milli = 5;
        let mut b = stock(2, 1, 1);
        b.window_lots_milli = 5;
        let mut c = stock(3, 1, 1);
        c.window_lots_milli = 9;
        assert_eq!(
            board_order(&c, &a),
            std::cmp::Ordering::Less,
            "more lots first"
        );
        assert_eq!(
            board_order(&a, &b),
            std::cmp::Ordering::Less,
            "then security_id"
        );
        assert_eq!(board_order(&a, &a), std::cmp::Ordering::Equal);
    }
}
