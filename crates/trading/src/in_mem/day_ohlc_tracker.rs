//! Day OHLC tracker for IDX_I (indices).
//!
//! ## Why this exists
//!
//! Dhan's Ticker mode (16-byte packet) carries only LTP + LTT — no day open /
//! high / low / volume fields. For our 4 IDX_I SIDs (NIFTY, BANKNIFTY, SENSEX,
//! INDIA VIX) we are LOCKED to Ticker mode per the operator-charter §I
//! WebSocket scope lock.
//!
//! Yet we still need day OHLC for:
//! - 1-day candle: cell.open / cell.high / cell.low / cell.close at session close
//! - Indicators that reference day range (ATR, Bollinger Bands, day range %)
//!
//! ## Design (locked 2026-05-26 — pre-market buffer deletion)
//!
//! Per-SID state held in a small `papaya::HashMap<(SecurityId, ExchangeSegment),
//! DayOhlc>` keyed by composite (security_id, exchange_segment) per the I-P1-11
//! uniqueness invariant. Updated on every Ticker tick:
//!
//! - `day_open` — auto-armed from the FIRST tick observed for the SID after
//!   the daily reset. NOT from a pre-market REST fetch. This means
//!   `day_open == first traded LTP after 09:15:00 IST` (or whenever the first
//!   tick lands). Operator accepts the ~paise difference vs NSE equilibrium
//!   open since Dhan historical / cross-verify / backfill is being removed.
//! - `day_high` — `max(day_high, tick.last_price)` on every tick
//! - `day_low` — `min(day_low, tick.last_price)` on every tick
//! - `day_close` — set to last tick's LTP at 15:30:00 IST seal
//! - `day_volume` — STAYS 0 (Ticker mode carries no volume field; documented gap)
//!
//! Daily reset at 00:00 IST resets all fields. The reset task lives in
//! `day_ohlc_orchestrator.rs::spawn_midnight_reset_task`.
//!
//! ## Hot-path budget
//!
//! Per `.claude/rules/project/hot-path.md`: zero allocation, O(1) per update.
//! Bench-tested under `bench_score_compute_le_1us` — first-tick auto-arm adds
//! a single branch (~1 ns).
//!
//! ## Composite key per I-P1-11
//!
//! Keyed by `(security_id, exchange_segment)` not `security_id` alone, per
//! `.claude/rules/project/security-id-uniqueness.md`. The 4 SIDs are all
//! `ExchangeSegment::IdxI` so the composite is degenerate today, but the
//! invariant must hold for future scope extension.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use papaya::HashMap as PapayaHashMap;
use parking_lot::Mutex;

use tickvault_common::types::ExchangeSegment;

/// Day OHLC state for a single instrument.
///
/// Fields are intentionally non-`Option<f64>` because the tracker initialises
/// all 4 fields atomically when the first tick lands. `day_open` may carry a
/// sentinel value of `f64::NAN` only between IST midnight reset and the first
/// tick — `is_armed()` reflects this.
#[derive(Debug, Clone, Copy)]
pub struct DayOhlc {
    /// First-traded LTP for the trading day. Auto-armed from the first
    /// `update_tick()` call after a daily reset; never mutated thereafter.
    pub day_open: f64,
    /// Cumulative max LTP since first tick.
    pub day_high: f64,
    /// Cumulative min LTP since first tick.
    pub day_low: f64,
    /// Most recent LTP — becomes the day_close at 15:30:00 IST seal.
    pub day_close: f64,
    /// Has `day_open` been initialised by the first tick yet?
    /// `false` between IST midnight reset and first post-reset tick.
    ///
    /// Note: VOLUME is intentionally NOT tracked. Operator-locked 2026-05-18:
    /// Dhan historical data has no volume field for indices, BRUTEX backtesting
    /// does not use volume, our trading decisions do not reference volume.
    armed: bool,
}

/// Smallest exchange-supplied day open treated as a real price.
///
/// A hundredth of the 0.05 NSE tick, so no real instrument can fall below it,
/// while every f64 subnormal (the `5e-324` class that is finite and positive
/// and passes every other check) is refused. Absolute rather than relative
/// BECAUSE relative was tried first and was wrong: see the refusal site.
pub const DAY_OPEN_MIN_PLAUSIBLE_PRICE: f64 = 0.000_5;

/// Largest exchange-supplied day open treated as a real price.
///
/// One crore rupees. The most expensive scrip ever listed on an Indian
/// exchange is orders of magnitude below this, and `f32::MAX` (3.4e38) --
/// the corrupt-payload shape that motivated the band -- is thirty-one orders
/// of magnitude above it. The gap is deliberately enormous: this bound exists
/// to reject numbers that are not prices, never to have an opinion about a
/// price that is.
pub const DAY_OPEN_MAX_PLAUSIBLE_PRICE: f64 = 10_000_000.0;

/// Is this a plausible rupee price at all?
///
/// The single predicate behind BOTH the last-traded-price gate and the
/// exchange-day-open gate, so the two can never drift apart — which they had,
/// and the drift was found by the day-open gate's own regression test: a
/// subnormal `1e-320` was refused as an OPEN and accepted as a PRICE in the
/// same call, moving `day_low` to a number 320 orders of magnitude below a
/// paisa.
///
/// `is_finite() && > 0.0` is not enough on its own. Every f64 subnormal is
/// finite and positive, and so is `f32::MAX` widened to f64 — the two shapes
/// that actually appear in corrupt payloads. This holds NO opinion about how
/// far a price may move (an option can legitimately go from 5.60 to 0.05 in a
/// session); it only asks whether the number could be a price on an Indian
/// exchange at all.
#[must_use]
pub fn is_plausible_price(price: f64) -> bool {
    if !price.is_finite() {
        return false;
    }
    // Two gates disagree about the next line and only an exemption satisfies
    // both: the banned-pattern scanner reads any `.contains(` as the O(n) `Vec`
    // form, while clippy's `manual_range_contains` rejects the two-comparison
    // spelling. The exemption is honest rather than a way around a check --
    // `RangeInclusive::contains` on an `f64` IS two comparisons, with no
    // iteration and no allocation. Split from the `is_finite` guard so the
    // exemption sits on the line immediately above the call, which is the only
    // place the scanner reads it.
    // O(1) EXEMPT: RangeInclusive::contains on a scalar is two comparisons, not a scan.
    (DAY_OPEN_MIN_PLAUSIBLE_PRICE..=DAY_OPEN_MAX_PLAUSIBLE_PRICE).contains(&price)
}

/// How many exchange day-opens have been refused this process, for the
/// power-of-two log throttle. Separate from the metric because
/// `metrics::Counter` does not expose its value.
static REFUSED_DAY_OPEN_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl DayOhlc {
    /// Sentinel `disarmed` state — all fields meaningless until first tick.
    #[must_use]
    pub const fn disarmed() -> Self {
        Self {
            day_open: f64::NAN,
            day_high: f64::NAN,
            day_low: f64::NAN,
            day_close: f64::NAN,
            armed: false,
        }
    }

    /// Initialises all 4 OHLC fields from the first tick's LTP.
    /// Called internally by `update_tick` on the first call after a reset.
    fn arm_from_first_tick(&mut self, first_tick_price: f64) {
        debug_assert!(
            first_tick_price.is_finite() && first_tick_price > 0.0,
            "first_tick_price must be a finite positive price"
        );
        self.day_open = first_tick_price;
        self.day_high = first_tick_price;
        self.day_low = first_tick_price;
        self.day_close = first_tick_price;
        self.armed = true;
    }

    /// True iff a tick has been observed for the current trading day.
    #[must_use]
    #[inline]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }

    /// Hot-path tick update. O(1), zero allocation.
    ///
    /// On the FIRST call after a daily reset, auto-arms by setting all 4 fields
    /// to `last_price`. On subsequent calls, updates `day_high`/`day_low`/`day_close`.
    /// `day_open` is set ONCE per trading day and never mutated thereafter.
    #[inline]
    pub fn update_tick(&mut self, last_price: f64) {
        // INGEST GATE (2026-08-10, hostile review). The only protection here
        // used to be the `debug_assert!` inside `arm_from_first_tick`, and the
        // release profile does not enable debug assertions — so in production
        // that check does not exist.
        //
        // The failure it lets through is absorbing and silent. The Dhan
        // parsers are PROVEN to emit NaN LTP (four parser tests assert
        // `last_traded_price.is_nan()` on real packet shapes). If the FIRST
        // tick of a day carries NaN, all four fields arm to NaN and `armed`
        // flips true. Every later comparison is then false — `100.0 > NaN` and
        // `100.0 < NaN` are both false — so `day_high` and `day_low` stay NaN
        // for the entire session while `is_armed()` reports true and
        // `snapshot()` hands the NaN out as a valid reading. Only the daily
        // reset clears it.
        //
        // A zero or negative first tick is the quieter variant: it arms
        // `day_low` at 0.0 and nothing is ever below it, producing a
        // plausible-looking "day low 0.00" that no reader would question. The
        // old excuse for tolerating that — "tick_processor filters it
        // downstream" — refers to a module deleted in the 2026-07-17 sweep.
        //
        // Refusing costs one comparison and loses nothing: a non-finite or
        // non-positive price is not a price. Same shape as the ingest gate
        // §28.4 added to IndicatorEngine::update, and as
        // RiskEngine::update_market_price.
        //
        // Dormant today (no tick publisher exists) but not hypothetical: the
        // Dhan live main-feed revival was operator-authorized 2026-08-09, and
        // restoring the publisher is precisely what that work does.
        // 2026-08-26: was `!is_finite() || <= 0.0`. That admitted every f64
        // subnormal -- finite, positive, and 320 orders of magnitude below a
        // paisa -- straight into `day_low`, where it is absorbing until the
        // IST-midnight reset. Found by the day-open gate's own regression
        // test refusing a value as an OPEN that this line accepted as a PRICE
        // in the same call. One predicate now decides both.
        if !is_plausible_price(last_price) {
            return;
        }
        if !self.armed {
            self.arm_from_first_tick(last_price);
            return;
        }
        if last_price > self.day_high {
            self.day_high = last_price;
        }
        if last_price < self.day_low {
            self.day_low = last_price;
        }
        self.day_close = last_price;
    }

    /// Records a tick AND adopts the EXCHANGE's own day-open price.
    ///
    /// # Why the exchange's value and not our first observed tick
    ///
    /// Operator directive 2026-08-25 (recorded with his verbatim words in
    /// `websocket-connection-scope-lock.md`): *"whatever the 9.12 am close
    /// price should be the 9.15 am open price … we need to make the pre market
    /// price as 9.15 am open price always"*.
    ///
    /// He is describing how NSE defines the open. The 09:15 opening price IS
    /// the equilibrium discovered in the 09:08–09:12 pre-open matching window,
    /// so "the 09:12 price is the open" is the exchange's own rule, not a
    /// preference.
    ///
    /// [`Self::update_tick`] instead arms `day_open` from the first in-session
    /// tick it happens to SEE. For any instrument that does not trade in the
    /// first moments of the session those are different numbers: a thin stock
    /// option whose first print is 09:31 had a 09:31 price stored wearing the
    /// day's opening label. Across ~20,000 subscribed stock options that is a
    /// systematic error rather than an edge case.
    ///
    /// The authoritative value is already on the wire — `ParsedTick.day_open`
    /// rides in every Quote and Full packet — and was simply being discarded
    /// in favour of a derived one.
    ///
    /// # The OHLC invariant, which is why this also touches high/low
    ///
    /// Adopting an open the tracker never observed can put it OUTSIDE the
    /// range built from observed ticks, producing `open < low` — an
    /// internally inconsistent row that reads as corruption to every
    /// downstream consumer. So the adopted open WIDENS the range to contain
    /// itself. That is not scope creep beyond the directive; it is what keeps
    /// the directive's result a valid OHLC record.
    ///
    /// # Fallback
    ///
    /// `exchange_day_open` of `0.0` is the DOCUMENTED absent sentinel for
    /// Ticker-mode packets (`tick_types.rs`: *"Day open price (from
    /// Quote/Full; 0.0 for Ticker)"*), and non-finite is the NaN class the
    /// ingest gate above exists for. Either one leaves the first-tick open
    /// exactly as it was — never overwritten with zero.
    pub fn update_tick_with_exchange_open(&mut self, last_price: f64, exchange_day_open: f64) {
        // Whether THIS packet's price passed the ingest gate, decided BEFORE
        // the mutation so the answer is about this tick and not about history.
        //
        // 2026-08-26: this used to be `if !self.armed { return; }`, whose
        // comment ("an instrument whose LTP was refused has no armed state to
        // attach an open to") is true only of the FIRST tick. For every tick
        // after arming, `armed` is already true, so a packet whose LTP was
        // refused as corrupt still had its `day_open` adopted — and adopting
        // one field of a packet whose other field is proven garbage is exactly
        // the trust this gate exists to withhold. The parsers are PROVEN to
        // emit NaN LTP (`quote.rs:382` asserts it on a real packet shape), so
        // the corrupt-packet case is not hypothetical.
        let price_accepted = is_plausible_price(last_price);
        self.update_tick(last_price);
        if !price_accepted || !self.armed {
            return;
        }
        if !exchange_day_open.is_finite() || exchange_day_open <= 0.0 {
            return;
        }
        // An ABSOLUTE plausibility band, not a ratio against this packet's own
        // price. The adopted open WIDENS the range (see the invariant note
        // above), and that widening is irreversible until the IST-midnight
        // reset, so a single bad packet permanently distorts the day's high or
        // low for that instrument. Something must refuse `3.4e38` (f32::MAX)
        // and the subnormal class, both of which are finite and positive and
        // pass every check above.
        //
        // CORRECTED 2026-08-26, hours after the ratio version shipped: a 100x
        // ratio band was WRONG, and wrong in the direction that re-creates the
        // bug this whole change exists to fix. Its justification was "NSE
        // circuit limits cap a day's move at +/-20%" -- true of EQUITIES, and
        // options have no circuit limit at all. An option opening at 5.60 and
        // trading at the 0.05 tick floor is a 112x ratio and an utterly
        // ordinary expiry-day print; the ratio band silently discarded the
        // exchange's open for it and left the first-observed-tick open in
        // place, which across ~20,000 subscribed stock options is a systematic
        // error rather than an edge case. The band also stopped being a bound
        // at all in the subnormal region, where `last_price / 100.0` underflows
        // to 0.0 and lets anything positive through.
        //
        // An absolute band holds no opinion about how far a price may MOVE, so
        // it cannot reject a real move however violent. It only asks whether
        // the number is a plausible rupee price at all. Every instrument on
        // NSE and BSE lives inside it with orders of magnitude to spare: the
        // floor is a hundredth of the 0.05 tick, and the ceiling is far above
        // the most expensive scrip ever listed.
        if !is_plausible_price(exchange_day_open) {
            metrics::counter!("tv_day_ohlc_exchange_open_refused_total").increment(1);
            // Logged, not only counted: a counter that reaches no operator
            // surface measures the loss and then discards the measurement.
            // Throttled to powers of two so a corrupt-payload storm cannot
            // flood the sink -- the house pattern, and the reason this is a
            // `warn!` rather than an unconditional line on a per-tick path.
            let n = REFUSED_DAY_OPEN_SEEN
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            if n.is_power_of_two() {
                tracing::warn!(
                    exchange_day_open,
                    last_price,
                    min = DAY_OPEN_MIN_PLAUSIBLE_PRICE,
                    max = DAY_OPEN_MAX_PLAUSIBLE_PRICE,
                    occurrences = n,
                    "exchange day open REFUSED as corrupt -- it is not a plausible \
                     rupee price, so adopting it would have widened the day range \
                     irreversibly until the IST-midnight reset. The previous open is \
                     kept."
                );
            }
            return;
        }
        self.day_open = exchange_day_open;
        // Keep `low <= open <= high` true by construction.
        if exchange_day_open > self.day_high {
            self.day_high = exchange_day_open;
        }
        if exchange_day_open < self.day_low {
            self.day_low = exchange_day_open;
        }
    }

    /// Daily reset at IST midnight. Drops `armed` to false; sentinel values restored.
    pub fn reset_daily(&mut self) {
        *self = Self::disarmed();
    }
}

impl Default for DayOhlc {
    fn default() -> Self {
        Self::disarmed()
    }
}

/// Shared per-instrument day OHLC tracker.
///
/// `papaya::HashMap` is the project-standard concurrent map per the hot-path
/// rule banning `DashMap`. Each value is `Mutex<DayOhlc>` (parking_lot) for
/// O(1) atomic compound update.
///
/// Keyed by `(SecurityId, ExchangeSegment)` per I-P1-11. At the 4-SID indices-
/// only scope the segment is always `IdxI` but the composite key is preserved
/// for future scope extension to other segments.
#[derive(Debug, Clone)]
pub struct DayOhlcTracker {
    inner: Arc<PapayaHashMap<(u64, ExchangeSegment), Mutex<DayOhlc>>>,
}

impl DayOhlcTracker {
    /// Pre-allocated capacity hint. **This is a sizing hint, NOT a limit** —
    /// see [`Self::MAX_TRACKED_INSTRUMENTS`] for the bound that is actually
    /// enforced. The name is kept for compatibility; the doc is corrected.
    ///
    /// O(1) EXEMPT: boot-time allocation, NOT a hot-path resize.
    pub const TRACKER_CAPACITY: usize = 8;

    /// Hard ceiling on distinct `(security_id, exchange_segment)` slots.
    ///
    /// # Why this exists (added 2026-08-11)
    ///
    /// `update_tick` inserts a slot for every unseen instrument and **nothing
    /// ever evicts one**. Until today the only thing standing between that and
    /// unbounded memory growth was caller convention — the doc above asserted
    /// the tracker "is never expected to grow beyond 4 entries", which is a
    /// statement about who calls it, not a property of the code. A workspace
    /// complexity audit named this the single genuine space-complexity
    /// violation in the workspace, and it is the exact class
    /// `spot_bar_store.rs` closed with `MAX_SPOT_BAR_SLOTS`: per-op cost is
    /// O(1) and always was, but memory was unbounded.
    ///
    /// That mattered little at four indices. It matters a great deal at the
    /// ~25,000-instrument target the r8g.xlarge was sized for, where a
    /// convention-only bound is the difference between a working box and an
    /// OOM — and `TRACKER_CAPACITY = 8` reads exactly like a cap while
    /// enforcing nothing, which is worse than having no number at all.
    ///
    /// Set to the same 25,000 ceiling the aggregator and indicator engine use,
    /// so the three bounds agree rather than each inventing a number.
    pub const MAX_TRACKED_INSTRUMENTS: usize = 25_000;

    /// Constructs an empty tracker. Populated lazily by the first
    /// `update_tick()` call for each SID.
    #[must_use]
    pub fn new() -> Self {
        Self {
            // O(1) EXEMPT: bounded boot-time allocation. TRACKER_CAPACITY only
            // pre-sizes the map for the live 4-SID universe; the map still
            // grows on demand, and the real ceiling is the fail-closed
            // MAX_TRACKED_INSTRUMENTS check in `update_tick`, NOT this figure.
            // (Corrected 2026-08-11: this comment used to claim the tracker
            // "never grows past 4 SIDs", which stopped being true the moment
            // the 25,000 bound was added above it.)
            inner: Arc::new(PapayaHashMap::with_capacity(Self::TRACKER_CAPACITY)),
        }
    }

    /// Hot-path per-tick update. O(1), zero allocation on the happy path.
    ///
    /// On the FIRST call for a given (security_id, segment) after a daily reset
    /// (or on a fresh tracker), auto-arms `day_open = day_high = day_low =
    /// day_close = last_price`. On subsequent calls, updates high/low/close.
    ///
    /// Returns `true` when the tick was recorded, `false` when the instrument
    /// could not be admitted because the tracker is at
    /// [`Self::MAX_TRACKED_INSTRUMENTS`].
    ///
    /// # Refusal is fail-closed and LOUD
    ///
    /// An ALREADY-TRACKED instrument is never refused — the capacity check
    /// runs only on the insert path, so a full tracker keeps updating every
    /// instrument it already holds and turns away only new ones. That is the
    /// right direction: losing the day's OHLC for instruments already being
    /// tracked, in order to admit one more, would be strictly worse than
    /// refusing the newcomer.
    ///
    /// A refusal increments `tv_day_ohlc_tracker_refused_total` and logs at
    /// `error!`. It is never silent: a silently-refused instrument would carry
    /// a stale or absent day high/low into every downstream consumer while
    /// every counter read normal.
    #[inline]
    pub fn update_tick_with_exchange_open(
        &self,
        security_id: u64,
        segment: ExchangeSegment,
        last_price: f64,
        exchange_day_open: f64,
    ) -> bool {
        let pinned = self.inner.pin();
        let key = (security_id, segment);
        if let Some(slot) = pinned.get(&key) {
            slot.lock()
                .update_tick_with_exchange_open(last_price, exchange_day_open);
            return true;
        }
        // First tick for this instrument. `papaya::len` is NOT a single
        // maintained counter — it sums a striped counter across
        // `next_power_of_two(available_parallelism())` cache-padded shards, so
        // it is O(shards): a handful of loads, independent of map size, off
        // the already-tracked path entirely. Cheap enough to be free here, but
        // it is a constant, not a load. (Corrected 2026-08-11 — the earlier
        // comment asserted "a maintained counter, not a walk", which named the
        // wrong mechanism even though the conclusion held.)
        //
        // The check races: two threads can both observe len < MAX and both
        // insert, so the true ceiling is MAX + (concurrent inserters - 1).
        // That is deliberate. Making it exact needs a lock or a CAS loop on
        // the hot path to buy a bound that is already an arbitrary round
        // number, and the overshoot is bounded by thread count — single-digit
        // slots against a 25,000 ceiling. A racy bound that costs nothing
        // beats an exact bound that costs a lock.
        if pinned.len() >= Self::MAX_TRACKED_INSTRUMENTS {
            metrics::counter!("tv_day_ohlc_tracker_refused_total").increment(1);
            tracing::error!(
                security_id,
                ?segment,
                tracked = pinned.len(),
                max = Self::MAX_TRACKED_INSTRUMENTS,
                "day OHLC tracker is FULL — refusing a new instrument; its day \
                 high/low/open will be absent downstream. Already-tracked \
                 instruments are unaffected."
            );
            return false;
        }
        let mut fresh = DayOhlc::disarmed();
        fresh.update_tick_with_exchange_open(last_price, exchange_day_open);
        pinned.insert(key, Mutex::new(fresh));
        true
    }

    /// Records a tick WITHOUT an exchange-supplied open.
    ///
    /// Retained so callers that genuinely have no `day_open` — a Ticker-mode
    /// packet, a test, a replayed row — keep their existing behaviour by
    /// construction. `0.0` is the documented absent sentinel, so this is the
    /// same call with the sentinel spelled out rather than a second code path.
    pub fn update_tick(&self, security_id: u64, segment: ExchangeSegment, last_price: f64) -> bool {
        self.update_tick_with_exchange_open(security_id, segment, last_price, 0.0)
    }

    /// Snapshot the current OHLC for one instrument. Returns `None` if no
    /// tick has been observed yet (slot does not exist).
    #[must_use]
    pub fn snapshot(&self, security_id: u64, segment: ExchangeSegment) -> Option<DayOhlc> {
        let pinned = self.inner.pin();
        let key = (security_id, segment);
        let slot = pinned.get(&key)?;
        let guard = slot.lock();
        if !guard.is_armed() {
            return None;
        }
        Some(*guard)
    }

    /// Number of currently tracked instruments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.pin().len()
    }

    /// True iff zero instruments tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reset every tracked instrument to disarmed sentinel.
    /// Called by the daily reset scheduler at IST midnight.
    pub fn reset_daily_all(&self) {
        let pinned = self.inner.pin();
        for (_key, slot) in pinned.iter() {
            slot.lock().reset_daily();
        }
    }
}

impl Default for DayOhlcTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Boot orchestration helpers
// ---------------------------------------------------------------------------

/// Number of seconds from `now` (IST) until the next occurrence of the given
/// (hour, minute, second) in IST. Returns 0 if the target is in the past for
/// today (caller must handle by waiting 24h or skipping).
///
/// Pure helper. Tested by `test_secs_until_next_ist_*`.
#[must_use]
pub fn secs_until_next_ist(target_h: u32, target_m: u32, target_s: u32, now_ist_secs: u32) -> u32 {
    let target_secs = target_h * 3600 + target_m * 60 + target_s;
    if now_ist_secs < target_secs {
        target_secs - now_ist_secs
    } else {
        // Already past today's target — wait until tomorrow's same time.
        (24 * 3600) - (now_ist_secs - target_secs)
    }
}

/// Returns the current IST second-of-day [0, 86_400). Uses `chrono::Utc::now()`.
#[must_use]
pub fn ist_seconds_of_day() -> u32 {
    use chrono::Utc;
    use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
    let now_utc = Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let sec = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY));
    u32::try_from(sec).unwrap_or(0)
}

#[cfg(test)]
mod tests {

    /// BITE (2026-08-26, second round): the band must never reject a real
    /// OPTION open.
    ///
    /// The first version of this gate was a 100x ratio against the packet's own
    /// last price, justified by "NSE circuit limits cap a day's move at
    /// +/-20%". Circuit limits are an EQUITY rule; options have none. An option
    /// that opens at 5.60 and trades at the 0.05 tick floor is a 112x ratio and
    /// an ordinary expiry-day print — and the ratio band discarded the
    /// exchange's open for it, re-creating for ~20,000 stock options exactly
    /// the systematic error this whole mechanism exists to remove.
    #[test]
    fn a_real_option_open_is_never_rejected_however_violent_the_move() {
        for (open, last) in [
            (5.60, 0.05),           // expiry-day decay to the tick floor: 112x
            (12.00, 0.05),          // 240x
            (0.05, 48.00),          // the other way: a 960x expiry-day spike
            (0.05, 1_200.00),       // 24,000x — still an ordinary option
            (24_341.95, 24_343.05), // an index, for contrast
        ] {
            let mut d = DayOhlc::disarmed();
            d.update_tick_with_exchange_open(last, open);
            assert_eq!(
                d.day_open, open,
                "open {open} with last {last} is a real print and must be adopted"
            );
        }
    }

    /// BITE: and the band must still refuse what is not a price at all.
    /// Subnormals are the case the ratio version silently let through, because
    /// `last_price / 100.0` underflows to 0.0 and stops being a bound.
    #[test]
    fn a_number_that_is_not_a_price_is_still_refused() {
        for (open, last) in [
            (3.4e38, 100.0),  // f32::MAX
            (1e-38, 100.0),   // f32 subnormal class
            (5e-324, 1e-320), // f64 subnormal — the ratio band ACCEPTED this
            (f64::MAX, 100.0),
            (1e12, 100.0), // a trillion rupees is not a price
        ] {
            let mut d = DayOhlc::disarmed();
            d.update_tick_with_exchange_open(100.0, 100.0);
            let before = (d.day_open, d.day_high, d.day_low);
            d.update_tick_with_exchange_open(last, open);
            assert_eq!(
                (d.day_open, d.day_high, d.day_low),
                before,
                "open {open} with last {last} is not a rupee price and must not widen the range"
            );
        }
    }

    /// BITE (2026-08-26): a packet whose LTP is refused must not have its
    /// `day_open` trusted either.
    ///
    /// The old guard was `if !self.armed { return; }`, which is only about the
    /// FIRST tick. Once armed, a NaN-LTP packet — a shape the Dhan parsers are
    /// PROVEN to emit — still had its `day_open` adopted, and because the
    /// adopted open WIDENS high/low, one such packet distorted the day's range
    /// irreversibly until the IST-midnight reset.
    #[test]
    fn a_refused_price_does_not_let_its_day_open_through() {
        let mut d = DayOhlc::disarmed();
        d.update_tick_with_exchange_open(100.0, 100.0);
        assert_eq!(d.day_open, 100.0);
        assert_eq!(d.day_high, 100.0);
        assert_eq!(d.day_low, 100.0);

        // NaN LTP with a finite, positive, plausible-looking open.
        d.update_tick_with_exchange_open(f64::NAN, 101.0);
        assert_eq!(d.day_open, 100.0, "the corrupt packet's open is refused");
        assert_eq!(d.day_high, 100.0);
        assert_eq!(d.day_low, 100.0);

        // Zero and negative LTP are the quieter variants of the same class.
        d.update_tick_with_exchange_open(0.0, 101.0);
        d.update_tick_with_exchange_open(-5.0, 101.0);
        assert_eq!(d.day_open, 100.0);
    }

    /// BITE: the corruption band. `f32::MAX` and the subnormal class are both
    /// finite and positive, so every earlier check passes them; only a band
    /// against the packet's own price rejects them.
    #[test]
    fn an_absurd_day_open_is_refused_rather_than_widening_the_range() {
        let mut d = DayOhlc::disarmed();
        d.update_tick_with_exchange_open(100.0, 100.0);

        d.update_tick_with_exchange_open(100.0, 3.4e38);
        assert_eq!(d.day_high, 100.0, "f32::MAX must not become the day high");
        assert_eq!(d.day_open, 100.0);

        d.update_tick_with_exchange_open(100.0, 1e-38);
        assert_eq!(d.day_low, 100.0, "a subnormal must not become the day low");
        assert_eq!(d.day_open, 100.0);

        d.update_tick_with_exchange_open(100.0, 5e-324);
        assert_eq!(d.day_low, 100.0);
    }

    /// The band must never reject a REAL move. NSE circuit limits cap a day at
    /// +/-20%, so this sweeps well past anything the exchange permits and
    /// asserts every one of them is still adopted.
    #[test]
    fn every_move_an_exchange_can_actually_produce_is_still_adopted() {
        for (price, open) in [
            (100.0, 80.0),       // -20%, the circuit floor
            (100.0, 125.0),      // +25%, past the ceiling
            (100.0, 10.0),       // 10x, impossible on NSE
            (100.0, 1000.0),     // 10x the other way
            (0.05, 0.5),         // a penny option, 10x
            (99_000.0, 9_900.0), // an index-scale price, 10x
        ] {
            let mut d = DayOhlc::disarmed();
            d.update_tick_with_exchange_open(price, open);
            assert_eq!(
                d.day_open, open,
                "price {price} with open {open} is inside every real market's range"
            );
        }
    }

    /// The exchange's own open replaces the first-tick open.
    ///
    /// Operator 2026-08-25: the 09:15 open IS the 09:08-09:12 pre-open
    /// equilibrium, so a thin instrument whose first print is 09:31 must not
    /// store that 09:31 price as the day's open.
    #[test]
    fn the_exchange_open_replaces_the_first_tick_we_happened_to_see() {
        let mut d = DayOhlc::disarmed();
        // First observed print is 09:31 at 105; the exchange says the day
        // opened at 100.
        d.update_tick_with_exchange_open(105.0, 100.0);
        assert!(
            (d.day_open - 100.0).abs() < f64::EPSILON,
            "open must be the exchange's"
        );
        assert!(
            (d.day_close - 105.0).abs() < f64::EPSILON,
            "close is still the tick"
        );
    }

    /// THE OPERATOR'S RULE, stated as a test with the numbers the live box
    /// actually produced on 2026-08-26.
    ///
    /// "Always ensure the finalised pre-open 9.12 close price as the 9.15 am
    /// open price" (operator, 2026-08-26; the same requirement as his
    /// 2026-08-25 quote recorded in websocket-connection-scope-lock.md).
    ///
    /// The shape this pins is the one indices hit EVERY morning and which the
    /// sibling test above does NOT cover: the first ticks we see are pre-open
    /// (09:00), and Dhan sends them with NO day-open field at all -- measured
    /// live, NIFTY's 09:00:02 rows carry `open = NULL`. Only at 09:15 does the
    /// exchange publish its equilibrium open. If the fallback open taken from
    /// that first pre-open print were sticky, NIFTY's recorded day open would
    /// be 24035.25 -- a PRE-OPEN price -- instead of the exchange's 24341.95,
    /// and it would be wrong by ~307 points on the headline index every day.
    ///
    /// Live evidence the adoption works today (`ticks`, 2026-08-26):
    ///   09:00:02  ltp 24035.25  open NULL      <- pre-open, no open yet
    ///   09:15:00  ltp 24343.05  open 24341.95  <- exchange equilibrium open
    /// and note the 09:15 LTP and the open DIFFER, so "first tick at or after
    /// 09:15" would also have been wrong. Only the exchange's own field is
    /// right.
    #[test]
    fn a_late_exchange_open_corrects_the_preopen_price_we_fell_back_to() {
        let mut d = DayOhlc::disarmed();

        // 09:00 pre-open: a real print, but the packet carries no day open.
        // 0.0 is the documented absent sentinel.
        d.update_tick_with_exchange_open(24035.25, 0.0);
        assert!(
            (d.day_open - 24035.25).abs() < f64::EPSILON,
            "with no exchange open yet, the first print is the only candidate"
        );

        // 09:15: the exchange publishes its equilibrium open. It must WIN,
        // even though we already had an open.
        d.update_tick_with_exchange_open(24343.05, 24341.95);
        assert!(
            (d.day_open - 24341.95).abs() < f64::EPSILON,
            "the exchange open must replace the pre-open fallback, got {}",
            d.day_open
        );
        assert!(
            (d.day_open - 24035.25).abs() > 1.0,
            "the PRE-OPEN price must not survive as the day open"
        );
    }

    /// The corrective adoption must not be a one-shot: an index that receives
    /// several pre-open prints before 09:15 still ends the day on the
    /// exchange's open, whichever print happened to arrive first.
    #[test]
    fn many_preopen_prints_still_end_on_the_exchange_open() {
        let mut d = DayOhlc::disarmed();
        for p in [24035.25_f64, 24009.75, 24004.95, 24120.10] {
            d.update_tick_with_exchange_open(p, 0.0);
        }
        d.update_tick_with_exchange_open(24343.05, 24341.95);
        assert!(
            (d.day_open - 24341.95).abs() < f64::EPSILON,
            "exchange open must win over every pre-open print, got {}",
            d.day_open
        );
        // And the invariant the range depends on still holds.
        assert!(d.day_low <= d.day_open && d.day_open <= d.day_high);
    }

    /// `low <= open <= high` must hold even when the adopted open sits
    /// outside the range built from observed ticks — otherwise the row reads
    /// as corruption downstream.
    #[test]
    fn an_adopted_open_below_the_observed_low_widens_the_range() {
        let mut d = DayOhlc::disarmed();
        d.update_tick_with_exchange_open(105.0, 100.0);
        assert!(
            d.day_low <= d.day_open,
            "low {} must not exceed open {}",
            d.day_low,
            d.day_open
        );
        assert!(
            d.day_open <= d.day_high,
            "open {} must not exceed high {}",
            d.day_open,
            d.day_high
        );

        let mut up = DayOhlc::disarmed();
        up.update_tick_with_exchange_open(100.0, 120.0);
        assert!(
            up.day_high >= 120.0,
            "an open ABOVE the observed high must lift the high"
        );
        assert!(up.day_low <= up.day_open);
    }

    /// 0.0 is the DOCUMENTED absent sentinel for Ticker-mode packets. It must
    /// never overwrite a real open with zero — that would read as a free
    /// instrument.
    #[test]
    fn the_ticker_mode_zero_sentinel_never_overwrites_a_real_open() {
        let mut d = DayOhlc::disarmed();
        d.update_tick_with_exchange_open(100.0, 0.0);
        assert!(
            (d.day_open - 100.0).abs() < f64::EPSILON,
            "0.0 must leave the first-tick open"
        );
        assert!(d.day_low > 0.0, "and must never drag the low to zero");
    }

    /// Non-finite is the NaN class the ingest gate exists for; it must be
    /// refused on this path too rather than poisoning the open forever.
    #[test]
    fn a_non_finite_exchange_open_is_refused_not_adopted() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let mut d = DayOhlc::disarmed();
            d.update_tick_with_exchange_open(100.0, bad);
            assert!(
                (d.day_open - 100.0).abs() < f64::EPSILON,
                "{bad} must not become the open"
            );
            assert!(d.day_open.is_finite() && d.day_low.is_finite() && d.day_high.is_finite());
        }
    }

    /// A refused LTP has no armed state, so there is nothing to attach an
    /// open to — the exchange open must not arm the slot by itself.
    #[test]
    fn an_exchange_open_cannot_arm_a_slot_whose_tick_was_refused() {
        let mut d = DayOhlc::disarmed();
        d.update_tick_with_exchange_open(f64::NAN, 100.0);
        assert!(!d.is_armed(), "a refused tick must leave the slot disarmed");
        d.update_tick_with_exchange_open(-5.0, 100.0);
        assert!(!d.is_armed());
    }
    use super::*;

    fn nifty() -> (u64, ExchangeSegment) {
        (13, ExchangeSegment::IdxI)
    }

    fn banknifty() -> (u64, ExchangeSegment) {
        (25, ExchangeSegment::IdxI)
    }

    #[test]
    fn test_update_tick_refuses_new_instruments_at_max_tracked_instruments() {
        // The defect this pins: `update_tick` inserted a slot per unseen
        // instrument and NOTHING evicted. The only thing bounding memory was
        // the doc's claim that callers never exceed four SIDs — a statement
        // about callers, not a property of the code. At the 25,000-instrument
        // target this is an OOM, and `TRACKER_CAPACITY = 8` read like a cap
        // while enforcing nothing.
        //
        // Uses a small local ceiling rather than driving 25,000 inserts: the
        // property under test is "the insert path consults a bound and refuses
        // past it", which does not depend on the bound's value.
        let tracker = DayOhlcTracker::new();
        let max = DayOhlcTracker::MAX_TRACKED_INSTRUMENTS;

        // Fill to exactly the ceiling. Every one of these must be admitted —
        // a bound that refuses BELOW its own limit is just as wrong.
        for sid in 0..max as u64 {
            assert!(
                tracker.update_tick(sid, ExchangeSegment::IdxI, 100.0),
                "instrument {sid} was refused below the ceiling of {max}"
            );
        }

        // One past the ceiling must be refused.
        assert!(
            !tracker.update_tick(max as u64, ExchangeSegment::IdxI, 100.0),
            "the tracker admitted instrument number {} past its own ceiling of \
             {max} — the bound is not enforced",
            max + 1
        );

        // ...and the refusal must not have created a slot.
        assert!(
            tracker
                .snapshot(max as u64, ExchangeSegment::IdxI)
                .is_none(),
            "a refused instrument must leave no slot behind"
        );
    }

    #[test]
    fn test_a_full_tracker_still_updates_instruments_it_already_holds() {
        // Fail-closed in the RIGHT direction. A full tracker must keep serving
        // every instrument it already tracks; refusing those in order to admit
        // a newcomer would lose the day's OHLC for instruments that were being
        // tracked correctly — strictly worse than turning the newcomer away.
        let tracker = DayOhlcTracker::new();
        let max = DayOhlcTracker::MAX_TRACKED_INSTRUMENTS;
        for sid in 0..max as u64 {
            tracker.update_tick(sid, ExchangeSegment::IdxI, 100.0);
        }
        assert!(
            !tracker.update_tick(max as u64, ExchangeSegment::IdxI, 100.0),
            "precondition: the tracker must be full"
        );

        // An instrument already in the map takes the get() path, which the
        // capacity check never guards.
        assert!(
            tracker.update_tick(7, ExchangeSegment::IdxI, 250.0),
            "a full tracker must still accept ticks for instruments it holds"
        );
        let snap = tracker
            .snapshot(7, ExchangeSegment::IdxI)
            .expect("instrument 7 was admitted before the tracker filled");
        assert!(
            (snap.day_high - 250.0).abs() < f64::EPSILON,
            "the update must have been applied, not merely accepted: high was {}",
            snap.day_high
        );
    }

    #[test]
    fn test_same_instrument_in_two_segments_takes_two_slots() {
        // I-P1-11: the key is the composite (security_id, exchange_segment),
        // so the bound counts instrument-segment pairs, not bare ids. Pinned
        // because a future "optimisation" to key on security_id alone would
        // silently merge two distinct instruments' day OHLC.
        let tracker = DayOhlcTracker::new();
        assert!(tracker.update_tick(13, ExchangeSegment::IdxI, 100.0));
        assert!(tracker.update_tick(13, ExchangeSegment::NseEquity, 200.0));

        let idx = tracker
            .snapshot(13, ExchangeSegment::IdxI)
            .expect("IDX_I slot exists");
        let eq = tracker
            .snapshot(13, ExchangeSegment::NseEquity)
            .expect("NSE_EQ slot exists");
        assert!(
            (idx.day_close - 100.0).abs() < f64::EPSILON
                && (eq.day_close - 200.0).abs() < f64::EPSILON,
            "the two segments must hold independent OHLC, got {} and {}",
            idx.day_close,
            eq.day_close
        );
    }

    #[test]
    fn test_day_ohlc_disarmed_by_default() {
        let ohlc = DayOhlc::default();
        assert!(!ohlc.is_armed());
        assert!(ohlc.day_open.is_nan());
        assert!(ohlc.day_high.is_nan());
        assert!(ohlc.day_low.is_nan());
        assert!(ohlc.day_close.is_nan());
    }

    #[test]
    fn test_first_tick_auto_arms_all_four_fields() {
        let mut ohlc = DayOhlc::default();
        ohlc.update_tick(25_650.5);
        assert!(ohlc.is_armed());
        assert_eq!(ohlc.day_open, 25_650.5);
        assert_eq!(ohlc.day_high, 25_650.5);
        assert_eq!(ohlc.day_low, 25_650.5);
        assert_eq!(ohlc.day_close, 25_650.5);
    }

    #[test]
    fn test_subsequent_ticks_never_mutate_day_open() {
        let mut ohlc = DayOhlc::default();
        ohlc.update_tick(25_650.5);
        ohlc.update_tick(25_665.0);
        ohlc.update_tick(25_640.0);
        ohlc.update_tick(25_660.0);
        // day_open MUST remain the first tick's LTP forever.
        assert_eq!(ohlc.day_open, 25_650.5);
        assert_eq!(ohlc.day_high, 25_665.0);
        assert_eq!(ohlc.day_low, 25_640.0);
        assert_eq!(ohlc.day_close, 25_660.0);
    }

    #[test]
    fn test_reset_daily_returns_to_disarmed() {
        let mut ohlc = DayOhlc::default();
        ohlc.update_tick(25_650.5);
        ohlc.update_tick(25_700.0);
        ohlc.reset_daily();
        assert!(!ohlc.is_armed());
        assert!(ohlc.day_open.is_nan());
    }

    #[test]
    fn test_post_reset_first_tick_re_arms_to_new_price() {
        let mut ohlc = DayOhlc::default();
        ohlc.update_tick(25_650.5);
        ohlc.reset_daily();
        ohlc.update_tick(25_700.0);
        assert!(ohlc.is_armed());
        assert_eq!(ohlc.day_open, 25_700.0);
        assert_eq!(ohlc.day_high, 25_700.0);
        assert_eq!(ohlc.day_low, 25_700.0);
        assert_eq!(ohlc.day_close, 25_700.0);
    }

    #[test]
    fn test_tracker_first_tick_creates_slot_and_arms() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        assert!(tracker.update_tick(sid, seg, 25_650.5));
        let snap = tracker.snapshot(sid, seg).unwrap();
        assert_eq!(snap.day_open, 25_650.5);
        assert_eq!(snap.day_high, 25_650.5);
        assert_eq!(snap.day_low, 25_650.5);
    }

    #[test]
    fn test_tracker_update_advances_high() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        tracker.update_tick(sid, seg, 25_650.5);
        tracker.update_tick(sid, seg, 25_700.0);
        let snap = tracker.snapshot(sid, seg).unwrap();
        assert_eq!(snap.day_open, 25_650.5);
        assert_eq!(snap.day_high, 25_700.0);
        assert_eq!(snap.day_low, 25_650.5);
        assert_eq!(snap.day_close, 25_700.0);
    }

    #[test]
    fn test_tracker_snapshot_on_fresh_sid_returns_none() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        assert!(tracker.snapshot(sid, seg).is_none());
    }

    #[test]
    fn test_tracker_isolates_securities() {
        let tracker = DayOhlcTracker::new();
        let (nifty_sid, nifty_seg) = nifty();
        let (bn_sid, bn_seg) = banknifty();
        tracker.update_tick(nifty_sid, nifty_seg, 25_650.5);
        tracker.update_tick(bn_sid, bn_seg, 55_000.0);
        tracker.update_tick(nifty_sid, nifty_seg, 25_700.0);
        // BANKNIFTY day_open untouched.
        let bn_snap = tracker.snapshot(bn_sid, bn_seg).unwrap();
        assert_eq!(bn_snap.day_open, 55_000.0);
        assert_eq!(bn_snap.day_high, 55_000.0);
        let nifty_snap = tracker.snapshot(nifty_sid, nifty_seg).unwrap();
        assert_eq!(nifty_snap.day_open, 25_650.5);
        assert_eq!(nifty_snap.day_high, 25_700.0);
    }

    #[test]
    fn test_tracker_reset_daily_disarms_all() {
        let tracker = DayOhlcTracker::new();
        let (nifty_sid, nifty_seg) = nifty();
        let (bn_sid, bn_seg) = banknifty();
        tracker.update_tick(nifty_sid, nifty_seg, 25_650.5);
        tracker.update_tick(bn_sid, bn_seg, 55_000.0);
        tracker.reset_daily_all();
        assert!(tracker.snapshot(nifty_sid, nifty_seg).is_none());
        assert!(tracker.snapshot(bn_sid, bn_seg).is_none());
    }

    #[test]
    fn test_tracker_len_and_is_empty() {
        let tracker = DayOhlcTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
        let (sid, seg) = nifty();
        tracker.update_tick(sid, seg, 25_650.5);
        assert!(!tracker.is_empty());
        assert_eq!(tracker.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Boot orchestration helpers
    // -----------------------------------------------------------------------

    #[test]
    fn test_secs_until_next_ist_target_in_future() {
        assert_eq!(secs_until_next_ist(9, 15, 0, 28_800), 33_300 - 28_800);
    }

    #[test]
    fn test_secs_until_next_ist_target_in_past_wraps_to_tomorrow() {
        assert_eq!(
            secs_until_next_ist(9, 15, 0, 36_000),
            86_400 - (36_000 - 33_300),
        );
    }

    #[test]
    fn test_secs_until_next_ist_exact_target_returns_full_day() {
        assert_eq!(secs_until_next_ist(9, 15, 0, 33_300), 86_400);
    }

    #[test]
    fn test_secs_until_next_ist_midnight() {
        assert_eq!(secs_until_next_ist(0, 0, 0, 86_340), 60);
    }

    #[test]
    fn test_secs_until_next_ist_1530_seal() {
        assert_eq!(secs_until_next_ist(15, 30, 0, 33_300), 55_800 - 33_300);
    }

    #[test]
    fn test_ist_seconds_of_day_within_bounds() {
        let sec = ist_seconds_of_day();
        assert!(sec < 86_400);
    }

    // ---- ingest gate (2026-08-10 hostile review) --------------------------

    #[test]
    fn test_nan_first_tick_does_not_arm_or_poison_the_day() {
        // THE BUG: pre-fix this armed all four fields to NaN and set
        // armed=true, after which every comparison was false forever. The
        // only guard was a debug_assert, which the release profile compiles
        // out — so this was live in production and invisible.
        let mut d = DayOhlc::disarmed();
        d.update_tick(f64::NAN);
        assert!(
            !d.is_armed(),
            "a NaN tick must not arm the day — arming on it poisons high/low \
             for the whole session, because every later comparison against NaN \
             is false in both directions"
        );

        // The next honest tick must arm cleanly, as if the NaN never arrived.
        d.update_tick(100.0);
        assert!(d.is_armed());
        assert_eq!(d.day_high, 100.0);
        assert_eq!(d.day_low, 100.0);
        assert_eq!(d.day_open, 100.0);
    }

    #[test]
    fn test_nan_mid_session_tick_cannot_poison_an_armed_day() {
        let mut d = DayOhlc::disarmed();
        d.update_tick(100.0);
        d.update_tick(105.0);
        d.update_tick(f64::NAN);
        d.update_tick(f64::INFINITY);
        d.update_tick(f64::NEG_INFINITY);
        assert_eq!(d.day_high, 105.0, "high must survive a NaN/Inf tick");
        assert_eq!(d.day_low, 100.0, "low must survive a NaN/Inf tick");
        assert_eq!(d.day_close, 105.0, "close must not adopt a non-finite tick");
    }

    #[test]
    fn test_zero_and_negative_first_tick_are_refused() {
        // The quiet variant: arming at 0.0 gives a day low nothing can beat,
        // and "day low 0.00" reads as data rather than as a fault.
        for bad in [0.0_f64, -1.0_f64] {
            let mut d = DayOhlc::disarmed();
            d.update_tick(bad);
            assert!(!d.is_armed(), "{bad} must not arm the day");
        }
    }

    #[test]
    fn test_a_refused_tick_leaves_close_untouched() {
        let mut d = DayOhlc::disarmed();
        d.update_tick(42.5);
        d.update_tick(0.0);
        assert_eq!(
            d.day_close, 42.5,
            "a refused tick must not become the day close — that would publish \
             a zero price as the last traded value"
        );
    }
}
