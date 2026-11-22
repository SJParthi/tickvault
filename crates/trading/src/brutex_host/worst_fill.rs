//! The backtest's worst-case fill model, ported so a live or paper fill can be
//! compared with what brutex assumed when it approved the strategy.
//!
//! The model (owner's rule, implemented exactly):
//!
//! 1. A signal is decided only at candle close (the decision second).
//! 2. The first fill can come from the first second after that candle
//!    (`FillParams::first_fill_sec`).
//! 3. Fill second `i` is priced at `P_i`, the WORST (highest for a buy, lowest
//!    for a sell) of:
//!    * that second's high (buy) or low (sell);
//!    * the next second's high or low, only when the data holds the literal
//!      next clock second (`sec + 1`). A gap means nothing traded in that
//!      second, so it gives no candidate; a later bar is never used in its
//!      place;
//!    * the decision second's high or low;
//!    * the best ask (buy) or bid (sell) on the book at the decision, when a
//!      book is given.
//! 4. Depth: shares are counted in total across seconds. The first `best_qty`
//!    shares use `P_i`. Shares in visible level `j` (`levels[j - 1]`) use
//!    `worse(P_i, level price)`, so when `P_i` is already worse than that
//!    level they are priced at `P_i` itself (the level adds nothing; this is
//!    intended: the price can only get worse, never better). Shares beyond
//!    every visible level use `worse(P_i, deepest visible price)` moved
//!    `(levels.len() + 1)` ticks against us: one tick for each visible level
//!    that is exhausted (the best level counts), so a deeper book costs more
//!    to run through. Without a book there is no depth step.
//! 5. Then one full tick against us on every share (+ for a buy, - for a sell).
//! 6. At most `participation_bps / 10_000` of a second's traded volume is ours,
//!    rounded down (default 25%). A zero-volume second fills nothing and the
//!    remainder rolls to the next second.
//! 7. Fills stop when the quantity is done, at the end of the horizon
//!    (`max_seconds` clock seconds from `first_fill_sec`) or at the end of the
//!    data; the rest is the unfilled remainder.
//!
//! Exits, stops and square-off use the same rule (the side decides).
//!
//! All money is integer paise. Cost sums run in `i128`.
//!
//! Complexity: O(seconds used + levels) per call. The book is validated once
//! (O(levels)), each fill second is visited once, and the depth cursor only
//! moves forward through the levels, so the number of slices written is at
//! most `seconds used + levels + 1`. No allocation: the schedule is a
//! caller-owned buffer sized once (see [`FillSchedule::required_capacity`]).

use super::engine::Side;

/// Largest price accepted anywhere in the model: 10^15 paise. Keeps every
/// price, penalty and cost far from `i64` / `i128` overflow.
pub const MAX_PRICE_PAISE: i64 = 1_000_000_000_000_000;
/// Largest order quantity accepted (shares).
pub const MAX_QTY: u64 = 1_000_000_000;
/// [`MAX_PRICE_PAISE`] as `f64`; 10^15 is exactly representable.
const MAX_PRICE_PAISE_F64: f64 = MAX_PRICE_PAISE as f64;
/// Basis-point denominator.
pub const BPS_DENOMINATOR: u32 = 10_000;
/// The backtest's default: at most 25% of a second's volume is ours.
pub const DEFAULT_PARTICIPATION_BPS: u32 = 2_500;

/// Why an `f64` price could not become paise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PaiseRefusal {
    #[error("price is NaN or infinite")]
    NotFinite,
    #[error("price is zero or negative")]
    NotPositive,
    #[error("price rounds outside 1..={MAX_PRICE_PAISE} paise")]
    OutOfRange,
}

/// The one checked `f64` (rupees) to paise conversion. Rounds to the nearest
/// paisa. Refuses NaN, infinities, zero, negatives, a value that rounds to 0
/// paise and anything above [`MAX_PRICE_PAISE`]. O(1).
pub fn rupees_to_paise(rupees: f64) -> Result<i64, PaiseRefusal> {
    if !rupees.is_finite() {
        return Err(PaiseRefusal::NotFinite);
    }
    if rupees <= 0.0 {
        return Err(PaiseRefusal::NotPositive);
    }
    // Round the exact binary value to the nearest paisa. `rupees * 100.0`
    // can itself round onto a .5 tie (0.015 * 100.0 == 1.5 although 0.015 is
    // just below 0.015); the fused error term says which side the true
    // product is on, and only then is a tie corrected (review finding,
    // 2026-11-12).
    let product = rupees * 100.0;
    let err = rupees.mul_add(100.0, -product);
    let paise = if product.fract() == 0.5 && err < 0.0 {
        product.floor()
    } else {
        product.round()
    };
    // Both bounds are exact in f64, so these compares are exact. Two plain
    // compares, not a range `contains`.
    if paise < 1.0 {
        return Err(PaiseRefusal::OutOfRange);
    }
    if paise > MAX_PRICE_PAISE_F64 {
        return Err(PaiseRefusal::OutOfRange);
    }
    // A whole number in 1..=10^15, so the cast is exact.
    Ok(paise as i64)
}

/// One second of trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecondBar {
    pub sec: i64,
    pub high_paise: i64,
    pub low_paise: i64,
    pub volume: u64,
}

/// One side of the book at the decision: the ask side for a buy, the bid side
/// for a sell. `levels` are the levels after the best one, moving away from it
/// (ascending for asks, descending for bids); it may be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookAtDecision<'a> {
    pub best_paise: i64,
    pub best_qty: u64,
    pub levels: &'a [(i64, u64)],
}

/// One fill call's inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillParams {
    pub side: Side,
    /// Shares wanted.
    pub qty: u64,
    pub tick_paise: i64,
    /// Our share of a second's volume, in basis points (2,500 = 25%).
    pub participation_bps: u32,
    /// The decision second (the last second of the closed candle).
    pub decision_sec: i64,
    /// The first second a fill may come from; must be after `decision_sec`.
    pub first_fill_sec: i64,
    /// Horizon in clock seconds from `first_fill_sec`.
    pub max_seconds: u32,
}

impl FillParams {
    /// The backtest defaults: 25% participation, first fill the second right
    /// after the decision.
    #[must_use]
    pub const fn new(
        side: Side,
        qty: u64,
        tick_paise: i64,
        decision_sec: i64,
        max_seconds: u32,
    ) -> Self {
        Self {
            side,
            qty,
            tick_paise,
            participation_bps: DEFAULT_PARTICIPATION_BPS,
            decision_sec,
            first_fill_sec: decision_sec.saturating_add(1),
            max_seconds,
        }
    }
}

/// One priced piece of the fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillSlice {
    pub sec: i64,
    pub qty: u64,
    pub price_paise: i64,
}

/// A fixed-capacity, caller-owned schedule: no allocation per call. A push
/// past the capacity refuses the whole fill (never a silent truncation).
#[derive(Debug)]
pub struct FillSchedule {
    buf: Vec<FillSlice>,
    cap: usize,
}

impl FillSchedule {
    /// Allocates once, at construction (cold).
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
        }
    }

    /// Slices one call can write at most: one per fill second plus one per
    /// depth boundary crossed (`levels + 1`). O(1).
    #[must_use]
    pub const fn required_capacity(max_seconds: u32, levels: usize) -> usize {
        (max_seconds as usize)
            .saturating_add(levels)
            .saturating_add(1)
    }

    #[must_use]
    pub fn slices(&self) -> &[FillSlice] {
        &self.buf
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// O(1) (keeps capacity).
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Appends, merging into the last slice when second and price match.
    /// O(1), no allocation.
    fn push(&mut self, slice: FillSlice) -> bool {
        if let Some(last) = self.buf.last_mut()
            && last.sec == slice.sec
            && last.price_paise == slice.price_paise
        {
            last.qty = last.qty.saturating_add(slice.qty);
            return true;
        }
        if self.buf.len() >= self.cap {
            return false;
        }
        self.buf.push(slice);
        true
    }
}

/// What one fill call produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillSummary {
    pub filled_qty: u64,
    /// Volume-weighted average price, rounded against us (up for a buy, down
    /// for a sell). `None` when nothing filled.
    pub vwap_paise: Option<i64>,
    pub unfilled_qty: u64,
    /// Fill seconds visited inside the horizon.
    pub seconds_used: u32,
    /// True when some shares went beyond every visible book level.
    pub depth_exhausted: bool,
}

/// Why a fill could not be modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FillRefusal {
    #[error("quantity is zero")]
    ZeroQty,
    #[error("quantity is above the {MAX_QTY} share limit")]
    QtyOutOfRange,
    #[error("tick size is zero or negative")]
    ZeroTick,
    #[error("tick size is above the price limit")]
    TickOutOfRange,
    #[error("participation must be 1..=10000 basis points")]
    BadParticipation,
    #[error("horizon is zero seconds")]
    ZeroHorizon,
    #[error("the decision bar or the first fill second is inconsistent")]
    DecisionMismatch,
    #[error("no fill seconds were given")]
    EmptySeconds,
    #[error("fill seconds are not strictly increasing from the first fill second")]
    SecondsOutOfOrder,
    #[error("a second's high is below its low")]
    HighBelowLow,
    #[error("a price is zero, negative or above the limit")]
    PriceOutOfRange,
    #[error("the book has a zero price or a zero quantity")]
    ZeroBook,
    #[error("the book's levels do not move away from the best price")]
    CrossedBook,
    #[error("a penalised price left the valid range")]
    AdjustedPriceOutOfRange,
    #[error("the fill schedule is full")]
    ScheduleFull,
}

/// True when a live or paper fill's average price is worse for us than the
/// model's: higher for a buy, lower for a sell. O(1).
#[must_use]
pub const fn fill_is_worse_than_model(
    side: Side,
    actual_avg_paise: i64,
    model_avg_paise: i64,
) -> bool {
    match side {
        Side::Buy => actual_avg_paise > model_avg_paise,
        Side::Sell => actual_avg_paise < model_avg_paise,
    }
}

#[inline]
const fn worse(side: Side, a: i64, b: i64) -> i64 {
    match side {
        Side::Buy => {
            if a > b {
                a
            } else {
                b
            }
        }
        Side::Sell => {
            if a < b {
                a
            } else {
                b
            }
        }
    }
}

/// The side's extreme of one second: high for a buy, low for a sell.
#[inline]
const fn extreme(side: Side, bar: &SecondBar) -> i64 {
    match side {
        Side::Buy => bar.high_paise,
        Side::Sell => bar.low_paise,
    }
}

/// Moves `price` by `amount` paise against us; the result must stay positive.
#[inline]
fn against(side: Side, price: i64, amount: i64) -> Result<i64, FillRefusal> {
    let moved = match side {
        Side::Buy => price.checked_add(amount),
        Side::Sell => price.checked_sub(amount),
    };
    match moved {
        Some(p) if p > 0 => Ok(p),
        _ => Err(FillRefusal::AdjustedPriceOutOfRange),
    }
}

#[inline]
const fn price_ok(p: i64) -> bool {
    p > 0 && p <= MAX_PRICE_PAISE
}

#[inline]
fn validate_bar(bar: &SecondBar) -> Result<(), FillRefusal> {
    if !price_ok(bar.high_paise) || !price_ok(bar.low_paise) {
        return Err(FillRefusal::PriceOutOfRange);
    }
    if bar.high_paise < bar.low_paise {
        return Err(FillRefusal::HighBelowLow);
    }
    Ok(())
}

/// The depth cursor: which book level the next share comes from.
struct Depth<'a> {
    side: Side,
    levels: &'a [(i64, u64)],
    /// 0 = best, `j` = `levels[j - 1]`, `levels.len() + 1` = beyond the book.
    level: usize,
    /// Shares left in the current visible level.
    left: u64,
    /// Price of the deepest visible level.
    deepest: i64,
    /// `(levels.len() + 1) * tick`, the beyond-the-book penalty.
    penalty: i64,
    exhausted: bool,
}

impl<'a> Depth<'a> {
    /// Validates the book once. O(levels).
    fn new(side: Side, book: &BookAtDecision<'a>, tick: i64) -> Result<Self, FillRefusal> {
        if book.best_paise <= 0 || book.best_qty == 0 {
            return Err(FillRefusal::ZeroBook);
        }
        if !price_ok(book.best_paise) {
            return Err(FillRefusal::PriceOutOfRange);
        }
        let mut prev = book.best_paise;
        for &(price, qty) in book.levels {
            if price <= 0 || qty == 0 {
                return Err(FillRefusal::ZeroBook);
            }
            if !price_ok(price) {
                return Err(FillRefusal::PriceOutOfRange);
            }
            let away = match side {
                Side::Buy => price > prev,
                Side::Sell => price < prev,
            };
            if !away {
                return Err(FillRefusal::CrossedBook);
            }
            prev = price;
        }
        let steps = i64::try_from(book.levels.len())
            .ok()
            .and_then(|l| l.checked_add(1))
            .ok_or(FillRefusal::AdjustedPriceOutOfRange)?;
        let penalty = steps
            .checked_mul(tick)
            .ok_or(FillRefusal::AdjustedPriceOutOfRange)?;
        Ok(Self {
            side,
            levels: book.levels,
            level: 0,
            left: book.best_qty,
            deepest: prev,
            penalty,
            exhausted: false,
        })
    }

    /// Takes up to `take` shares at the current level for a second priced
    /// `p`. Returns the shares taken and their price before the final tick.
    /// O(1) amortised: the level only moves forward.
    fn next(&mut self, take: u64, p: i64) -> Result<(u64, i64), FillRefusal> {
        let n = self.levels.len();
        if self.level > n {
            self.exhausted = true;
            let price = against(self.side, worse(self.side, p, self.deepest), self.penalty)?;
            return Ok((take, price));
        }
        let price = if self.level == 0 {
            p
        } else {
            match self.levels.get(self.level - 1) {
                Some(&(lp, _)) => worse(self.side, p, lp),
                None => p,
            }
        };
        let chunk = take.min(self.left);
        self.left -= chunk;
        if self.left == 0 {
            self.level += 1;
            self.left = match self.levels.get(self.level - 1) {
                Some(&(_, q)) if self.level <= n => q,
                _ => 0,
            };
        }
        Ok((chunk, price))
    }
}

/// Models the worst-case fill. `seconds` starts at the first fill second and
/// must be strictly increasing. On any refusal `out` is left empty.
///
/// O(seconds used + levels), no allocation (see the module docs).
pub fn worst_fill(
    params: &FillParams,
    decision_bar: &SecondBar,
    seconds: &[SecondBar],
    book: Option<&BookAtDecision<'_>>,
    out: &mut FillSchedule,
) -> Result<FillSummary, FillRefusal> {
    out.clear();
    let result = worst_fill_inner(params, decision_bar, seconds, book, out);
    if result.is_err() {
        out.clear();
    }
    result
}

fn validate_params(params: &FillParams) -> Result<(), FillRefusal> {
    if params.qty == 0 {
        return Err(FillRefusal::ZeroQty);
    }
    if params.qty > MAX_QTY {
        return Err(FillRefusal::QtyOutOfRange);
    }
    if params.tick_paise <= 0 {
        return Err(FillRefusal::ZeroTick);
    }
    if params.tick_paise > MAX_PRICE_PAISE {
        return Err(FillRefusal::TickOutOfRange);
    }
    if params.participation_bps == 0 || params.participation_bps > BPS_DENOMINATOR {
        return Err(FillRefusal::BadParticipation);
    }
    if params.max_seconds == 0 {
        return Err(FillRefusal::ZeroHorizon);
    }
    if params.first_fill_sec <= params.decision_sec {
        return Err(FillRefusal::DecisionMismatch);
    }
    Ok(())
}

fn worst_fill_inner(
    params: &FillParams,
    decision_bar: &SecondBar,
    seconds: &[SecondBar],
    book: Option<&BookAtDecision<'_>>,
    out: &mut FillSchedule,
) -> Result<FillSummary, FillRefusal> {
    validate_params(params)?;
    validate_bar(decision_bar)?;
    if decision_bar.sec != params.decision_sec {
        return Err(FillRefusal::DecisionMismatch);
    }
    if seconds.is_empty() {
        return Err(FillRefusal::EmptySeconds);
    }
    let side = params.side;
    let tick = params.tick_paise;
    let mut depth = match book {
        Some(b) => Some(Depth::new(side, b, tick)?),
        None => None,
    };
    let mut base = extreme(side, decision_bar);
    if let Some(b) = book {
        base = worse(side, base, b.best_paise);
    }
    let horizon_end = params
        .first_fill_sec
        .saturating_add(i64::from(params.max_seconds));
    let mut remaining = params.qty;
    let mut filled: u64 = 0;
    let mut cost: i128 = 0;
    let mut seconds_used: u32 = 0;
    let mut prev_sec = params.first_fill_sec.saturating_sub(1);

    for (i, s) in seconds.iter().enumerate() {
        if remaining == 0 {
            break;
        }
        validate_bar(s)?;
        if s.sec <= prev_sec {
            return Err(FillRefusal::SecondsOutOfOrder);
        }
        prev_sec = s.sec;
        if s.sec >= horizon_end {
            break;
        }
        seconds_used = seconds_used.saturating_add(1);
        let mut p = worse(side, base, extreme(side, s));
        if let Some(n) = seconds.get(i + 1) {
            validate_bar(n)?;
            if n.sec <= s.sec {
                return Err(FillRefusal::SecondsOutOfOrder);
            }
            // Only the literal next clock second is a candidate: a gap means
            // nothing traded in `s.sec + 1`.
            if n.sec == s.sec.saturating_add(1) {
                p = worse(side, p, extreme(side, n));
            }
        }
        let cap_wide = u128::from(s.volume) * u128::from(params.participation_bps)
            / u128::from(BPS_DENOMINATOR);
        let cap = u64::try_from(cap_wide).unwrap_or(u64::MAX);
        let mut take = cap.min(remaining);
        remaining -= take;
        while take > 0 {
            let (chunk, price) = match depth.as_mut() {
                Some(d) => d.next(take, p)?,
                None => (take, p),
            };
            let final_price = against(side, price, tick)?;
            if !out.push(FillSlice {
                sec: s.sec,
                qty: chunk,
                price_paise: final_price,
            }) {
                return Err(FillRefusal::ScheduleFull);
            }
            cost += i128::from(final_price) * i128::from(chunk);
            filled += chunk;
            take -= chunk;
        }
    }

    let vwap_paise = if filled == 0 {
        None
    } else {
        let f = i128::from(filled);
        let avg = match side {
            Side::Buy => (cost + f - 1) / f,
            Side::Sell => cost / f,
        };
        Some(i64::try_from(avg).map_err(|_| FillRefusal::AdjustedPriceOutOfRange)?)
    };
    Ok(FillSummary {
        filled_qty: filled,
        vwap_paise,
        unfilled_qty: remaining,
        seconds_used,
        depth_exhausted: depth.is_some_and(|d| d.exhausted),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECISION: i64 = 33_305;

    fn bar(sec: i64, high: i64, low: i64, volume: u64) -> SecondBar {
        SecondBar {
            sec,
            high_paise: high,
            low_paise: low,
            volume,
        }
    }

    fn decision_bar() -> SecondBar {
        bar(DECISION, 10_010, 9_990, 1_000)
    }

    fn params(side: Side, qty: u64) -> FillParams {
        FillParams::new(side, qty, 5, DECISION, 5)
    }

    fn run(
        p: &FillParams,
        secs: &[SecondBar],
        book: Option<&BookAtDecision<'_>>,
    ) -> (Result<FillSummary, FillRefusal>, Vec<FillSlice>) {
        let mut out = FillSchedule::with_capacity(64);
        let r = worst_fill(p, &decision_bar(), secs, book, &mut out);
        (r, out.slices().to_vec())
    }

    /// Five fill seconds, quiet prices, plenty of volume.
    const FIVE: [SecondBar; 5] = [
        SecondBar {
            sec: DECISION + 1,
            high_paise: 10_000,
            low_paise: 9_995,
            volume: 400,
        },
        SecondBar {
            sec: DECISION + 2,
            high_paise: 10_005,
            low_paise: 9_995,
            volume: 400,
        },
        SecondBar {
            sec: DECISION + 3,
            high_paise: 10_000,
            low_paise: 9_990,
            volume: 400,
        },
        SecondBar {
            sec: DECISION + 4,
            high_paise: 10_020,
            low_paise: 9_980,
            volume: 400,
        },
        SecondBar {
            sec: DECISION + 5,
            high_paise: 10_000,
            low_paise: 9_995,
            volume: 400,
        },
    ];

    #[test]
    fn decision_bar_is_the_worst_for_a_buy() {
        let (r, s) = run(&params(Side::Buy, 100), &FIVE[..2], None);
        let r = r.unwrap();
        // max(10000, 10005, 10010) = 10010, plus one tick.
        assert_eq!(
            s,
            vec![FillSlice {
                sec: DECISION + 1,
                qty: 100,
                price_paise: 10_015
            }]
        );
        assert_eq!(r.vwap_paise, Some(10_015));
        assert_eq!(r.filled_qty, 100);
        assert_eq!(r.unfilled_qty, 0);
        assert_eq!(r.seconds_used, 1);
        assert!(!r.depth_exhausted);
    }

    #[test]
    fn next_second_is_the_worst() {
        let secs = [FIVE[2], FIVE[3]];
        let mut p = params(Side::Buy, 100);
        p.first_fill_sec = DECISION + 3;
        let (r, s) = run(&p, &secs, None);
        assert!(r.is_ok());
        // max(10000 this, 10020 next, 10010 decision) = 10020 + 5.
        assert_eq!(s[0].price_paise, 10_025);
        let mut p = params(Side::Sell, 100);
        p.first_fill_sec = DECISION + 3;
        let (_, s) = run(&p, &secs, None);
        // min(9990, 9980, 9990) = 9980 - 5.
        assert_eq!(s[0].price_paise, 9_975);
    }

    #[test]
    fn book_best_is_a_candidate() {
        let book = BookAtDecision {
            best_paise: 10_100,
            best_qty: 1_000,
            levels: &[],
        };
        let (r, s) = run(&params(Side::Buy, 50), &FIVE, Some(&book));
        assert!(r.is_ok());
        assert_eq!(s[0].price_paise, 10_105);
    }

    #[test]
    fn participation_cap_rolls_remainder() {
        // 25% of 400 = 100 per second; 250 shares need three seconds.
        let (r, s) = run(&params(Side::Buy, 250), &FIVE, None);
        let r = r.unwrap();
        assert_eq!(
            s.iter().map(|x| x.qty).collect::<Vec<_>>(),
            vec![100, 100, 50]
        );
        assert_eq!(r.seconds_used, 3);
        assert_eq!(r.unfilled_qty, 0);
    }

    #[test]
    fn zero_volume_second_fills_nothing() {
        let secs = [
            bar(DECISION + 1, 10_000, 9_990, 0),
            bar(DECISION + 2, 10_000, 9_990, 40),
        ];
        let (r, s) = run(&params(Side::Buy, 20), &secs, None);
        let r = r.unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].sec, DECISION + 2);
        assert_eq!(s[0].qty, 10);
        assert_eq!(r.filled_qty, 10);
        assert_eq!(r.unfilled_qty, 10);
        assert_eq!(r.seconds_used, 2);
    }

    #[test]
    fn single_second_has_no_next() {
        let secs = [bar(DECISION + 1, 10_050, 9_950, 4_000)];
        let (r, s) = run(&params(Side::Sell, 10), &secs, None);
        assert_eq!(r.unwrap().filled_qty, 10);
        // min(9950, decision 9990) - 5.
        assert_eq!(s[0].price_paise, 9_945);
    }

    #[test]
    fn qty_above_capacity_leaves_remainder() {
        let (r, _) = run(&params(Side::Buy, 10_000), &FIVE, None);
        let r = r.unwrap();
        assert_eq!(r.filled_qty, 500);
        assert_eq!(r.unfilled_qty, 9_500);
        assert_eq!(r.seconds_used, 5);
    }

    #[test]
    fn horizon_stops_fills() {
        let mut p = params(Side::Buy, 10_000);
        p.max_seconds = 2;
        let (r, _) = run(&p, &FIVE, None);
        let r = r.unwrap();
        assert_eq!(r.filled_qty, 200);
        assert_eq!(r.seconds_used, 2);
    }

    #[test]
    fn depth_walks_levels_then_exhausts() {
        let levels = [(10_020, 30), (10_040, 20)];
        let book = BookAtDecision {
            best_paise: 10_000,
            best_qty: 50,
            levels: &levels,
        };
        let secs = [bar(DECISION + 1, 10_000, 9_990, 1_000)];
        let (r, s) = run(&params(Side::Buy, 200), &secs, Some(&book));
        let r = r.unwrap();
        // P = max(10000, 10010 decision, 10000 best) = 10010.
        assert_eq!(
            s,
            vec![
                FillSlice {
                    sec: DECISION + 1,
                    qty: 50,
                    price_paise: 10_015
                },
                FillSlice {
                    sec: DECISION + 1,
                    qty: 30,
                    price_paise: 10_025
                },
                FillSlice {
                    sec: DECISION + 1,
                    qty: 20,
                    price_paise: 10_045
                },
                // beyond: 10040 + 3 ticks + 1 tick.
                FillSlice {
                    sec: DECISION + 1,
                    qty: 100,
                    price_paise: 10_060
                },
            ]
        );
        assert!(r.depth_exhausted);
        assert_eq!(r.filled_qty, 200);
        // (50*10015 + 30*10025 + 20*10045 + 100*10060) / 200 = 10042 exactly.
        assert_eq!(r.vwap_paise, Some(10_042));
    }

    #[test]
    fn depth_is_counted_across_seconds() {
        let book = BookAtDecision {
            best_paise: 10_000,
            best_qty: 150,
            levels: &[],
        };
        let (r, s) = run(&params(Side::Buy, 250), &FIVE, Some(&book));
        assert!(r.unwrap().depth_exhausted);
        // second 3 is beyond the 150-share best level: +1 tick penalty.
        assert_eq!(s.last().map(|x| x.price_paise), Some(10_020 + 5 + 5));
        assert_eq!(s.iter().map(|x| x.qty).sum::<u64>(), 250);
    }

    #[test]
    fn one_sided_book_is_none_and_fine() {
        let (r, _) = run(&params(Side::Sell, 10), &FIVE, None);
        assert!(r.unwrap().vwap_paise.is_some());
    }

    #[test]
    fn crossed_and_zero_books_refused() {
        let bad = [(9_990, 10)];
        let book = BookAtDecision {
            best_paise: 10_000,
            best_qty: 10,
            levels: &bad,
        };
        assert_eq!(
            run(&params(Side::Buy, 1), &FIVE, Some(&book)).0,
            Err(FillRefusal::CrossedBook)
        );
        let bad = [(10_010, 10)];
        let book = BookAtDecision {
            best_paise: 10_000,
            best_qty: 10,
            levels: &bad,
        };
        assert_eq!(
            run(&params(Side::Sell, 1), &FIVE, Some(&book)).0,
            Err(FillRefusal::CrossedBook)
        );
        let book = BookAtDecision {
            best_paise: 0,
            best_qty: 10,
            levels: &[],
        };
        assert_eq!(
            run(&params(Side::Buy, 1), &FIVE, Some(&book)).0,
            Err(FillRefusal::ZeroBook)
        );
        let book = BookAtDecision {
            best_paise: 10_000,
            best_qty: 0,
            levels: &[],
        };
        assert_eq!(
            run(&params(Side::Buy, 1), &FIVE, Some(&book)).0,
            Err(FillRefusal::ZeroBook)
        );
        let zero = [(10_010, 0)];
        let book = BookAtDecision {
            best_paise: 10_000,
            best_qty: 5,
            levels: &zero,
        };
        assert_eq!(
            run(&params(Side::Buy, 1), &FIVE, Some(&book)).0,
            Err(FillRefusal::ZeroBook)
        );
    }

    #[test]
    fn bad_inputs_refused() {
        let mut p = params(Side::Buy, 0);
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::ZeroQty));
        p.qty = MAX_QTY + 1;
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::QtyOutOfRange));
        p.qty = 1;
        p.tick_paise = 0;
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::ZeroTick));
        p.tick_paise = MAX_PRICE_PAISE + 1;
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::TickOutOfRange));
        p.tick_paise = 5;
        p.participation_bps = 0;
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::BadParticipation));
        p.participation_bps = 10_001;
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::BadParticipation));
        p.participation_bps = 2_500;
        p.max_seconds = 0;
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::ZeroHorizon));
        p.max_seconds = 5;
        p.first_fill_sec = DECISION;
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::DecisionMismatch));
        p.first_fill_sec = DECISION + 1;
        p.decision_sec = DECISION - 1;
        assert_eq!(run(&p, &FIVE, None).0, Err(FillRefusal::DecisionMismatch));
        let p = params(Side::Buy, 1);
        assert_eq!(run(&p, &[], None).0, Err(FillRefusal::EmptySeconds));
        let hl = [bar(DECISION + 1, 9_000, 9_100, 100)];
        assert_eq!(run(&p, &hl, None).0, Err(FillRefusal::HighBelowLow));
        let order = [FIVE[1], FIVE[0]];
        assert_eq!(run(&p, &order, None).0, Err(FillRefusal::SecondsOutOfOrder));
        let early = [bar(DECISION, 10_000, 9_990, 100)];
        assert_eq!(run(&p, &early, None).0, Err(FillRefusal::SecondsOutOfOrder));
    }

    #[test]
    fn prices_near_i64_limits_refused() {
        let p = params(Side::Buy, 1);
        let huge = [bar(DECISION + 1, i64::MAX, i64::MAX - 1, 100)];
        assert_eq!(run(&p, &huge, None).0, Err(FillRefusal::PriceOutOfRange));
        let neg = [bar(DECISION + 1, 10, i64::MIN, 100)];
        assert_eq!(run(&p, &neg, None).0, Err(FillRefusal::PriceOutOfRange));
        let book = BookAtDecision {
            best_paise: i64::MAX,
            best_qty: 1,
            levels: &[],
        };
        assert_eq!(
            run(&p, &FIVE, Some(&book)).0,
            Err(FillRefusal::PriceOutOfRange)
        );
        // A sell pushed to zero by the tick is refused, never a free fill.
        let mut p = FillParams::new(Side::Sell, 1, 5, DECISION, 5);
        p.tick_paise = 100;
        let low = [bar(DECISION + 1, 100, 50, 100)];
        let mut out = FillSchedule::with_capacity(8);
        let db = bar(DECISION, 100, 60, 100);
        assert_eq!(
            worst_fill(&p, &db, &low, None, &mut out),
            Err(FillRefusal::AdjustedPriceOutOfRange)
        );
        assert!(out.slices().is_empty());
    }

    #[test]
    fn schedule_full_refused_and_cleared() {
        let levels = [(10_020, 1), (10_040, 1)];
        let book = BookAtDecision {
            best_paise: 10_000,
            best_qty: 1,
            levels: &levels,
        };
        let mut out = FillSchedule::with_capacity(2);
        let r = worst_fill(
            &params(Side::Buy, 10),
            &decision_bar(),
            &FIVE,
            Some(&book),
            &mut out,
        );
        assert_eq!(r, Err(FillRefusal::ScheduleFull));
        assert!(out.slices().is_empty());
        assert_eq!(FillSchedule::required_capacity(5, 2), 8);
    }

    #[test]
    fn tick_rounding_and_vwap_against_us() {
        // Both seconds price at 10015 + one tick.
        let secs = [
            bar(DECISION + 1, 10_010, 9_990, 4),
            bar(DECISION + 2, 10_015, 9_985, 4),
        ];
        let mut p = params(Side::Buy, 2);
        p.max_seconds = 5;
        // 25% of 4 = 1 a second.
        let (r, s) = run(&p, &secs, None);
        assert_eq!(
            s.iter().map(|x| x.price_paise).collect::<Vec<_>>(),
            vec![10_020, 10_020]
        );
        assert_eq!(r.unwrap().vwap_paise, Some(10_020));
        let secs = [
            bar(DECISION + 1, 10_010, 9_990, 4),
            bar(DECISION + 2, 10_010, 9_983, 4),
            bar(DECISION + 3, 10_010, 9_990, 4),
        ];
        let (r, s) = run(&params(Side::Sell, 2), &secs, None);
        // min(9990, 9983, 9990)=9983-5=9978; then min(9983,9990,9990)=9983-5=9978.
        assert_eq!(s.iter().map(|x| x.qty).sum::<u64>(), 2);
        assert_eq!(r.unwrap().vwap_paise, Some(9_978));
        // Odd average rounds against us: buy up. s1: max(10010, 10000,
        // 10000) + 5 = 10015; s2: max(10010, 10000, 10015) + 5 = 10020.
        // Average 10017.5 -> 10018.
        let secs = [
            bar(DECISION + 1, 10_000, 9_995, 4),
            bar(DECISION + 2, 10_000, 9_995, 4),
            bar(DECISION + 3, 10_015, 9_985, 4),
        ];
        let (r, s) = run(&params(Side::Buy, 2), &secs, None);
        assert_eq!(
            s.iter().map(|x| x.price_paise).collect::<Vec<_>>(),
            vec![10_015, 10_020]
        );
        assert_eq!(r.unwrap().vwap_paise, Some(10_018));
        // Sell: s1: min(9990, 9995, 9995) - 5 = 9985; s2: min(9990, 9995,
        // 9985) - 5 = 9980. Average 9982.5 -> down to 9982.
        let (r, _) = run(&params(Side::Sell, 2), &secs, None);
        assert_eq!(r.unwrap().vwap_paise, Some(9_982));
    }

    #[test]
    fn buy_and_sell_mirror() {
        const C: i64 = 20_000;
        let levels_b = [(10_020, 30), (10_040, 20)];
        let levels_s = [(C - 10_020, 30), (C - 10_040, 20)];
        let book_b = BookAtDecision {
            best_paise: 10_000,
            best_qty: 50,
            levels: &levels_b,
        };
        let book_s = BookAtDecision {
            best_paise: C - 10_000,
            best_qty: 50,
            levels: &levels_s,
        };
        let mirror = |b: &SecondBar| bar(b.sec, C - b.low_paise, C - b.high_paise, b.volume);
        let secs_s: Vec<SecondBar> = FIVE.iter().map(mirror).collect();
        let db = decision_bar();
        let db_s = mirror(&db);
        let pb = params(Side::Buy, 900);
        let ps = params(Side::Sell, 900);
        let mut ob = FillSchedule::with_capacity(32);
        let mut os = FillSchedule::with_capacity(32);
        let rb = worst_fill(&pb, &db, &FIVE, Some(&book_b), &mut ob).unwrap();
        let rs = worst_fill(&ps, &db_s, &secs_s, Some(&book_s), &mut os).unwrap();
        assert_eq!(rb.filled_qty, rs.filled_qty);
        assert_eq!(rb.depth_exhausted, rs.depth_exhausted);
        assert_eq!(rb.vwap_paise.map(|v| C - v), rs.vwap_paise);
        assert_eq!(ob.slices().len(), os.slices().len());
        for (a, b) in ob.slices().iter().zip(os.slices()) {
            assert_eq!(
                (a.sec, a.qty, C - a.price_paise),
                (b.sec, b.qty, b.price_paise)
            );
        }
    }

    #[test]
    fn worse_than_model() {
        assert!(fill_is_worse_than_model(Side::Buy, 101, 100));
        assert!(!fill_is_worse_than_model(Side::Buy, 100, 100));
        assert!(fill_is_worse_than_model(Side::Sell, 99, 100));
        assert!(!fill_is_worse_than_model(Side::Sell, 101, 100));
    }

    #[test]
    fn test_rupees_to_paise_refuses_bad_input_and_rounds_to_nearest() {
        // Refusals.
        assert_eq!(rupees_to_paise(f64::NAN), Err(PaiseRefusal::NotFinite));
        // Double-rounding ties: 0.015 and 0.045 are stored just BELOW the half
        // paisa, though `x * 100.0` rounds onto exactly 1.5 and 4.5. An exact
        // half (0.125 rupees = 12.5 paise) still rounds away from zero.
        assert_eq!(rupees_to_paise(0.015), Ok(1));
        assert_eq!(rupees_to_paise(0.045), Ok(4));
        assert_eq!(rupees_to_paise(0.125), Ok(13));
        assert_eq!(rupees_to_paise(f64::INFINITY), Err(PaiseRefusal::NotFinite));
        assert_eq!(
            rupees_to_paise(f64::NEG_INFINITY),
            Err(PaiseRefusal::NotFinite)
        );
        assert_eq!(rupees_to_paise(0.0), Err(PaiseRefusal::NotPositive));
        assert_eq!(rupees_to_paise(-0.0), Err(PaiseRefusal::NotPositive));
        assert_eq!(rupees_to_paise(-1.0), Err(PaiseRefusal::NotPositive));
        assert_eq!(
            rupees_to_paise(-f64::MIN_POSITIVE),
            Err(PaiseRefusal::NotPositive)
        );
        // Rounds to 0 paise.
        assert_eq!(rupees_to_paise(0.004), Err(PaiseRefusal::OutOfRange));
        assert_eq!(
            rupees_to_paise(f64::MIN_POSITIVE),
            Err(PaiseRefusal::OutOfRange)
        );
        // Above the maximum.
        assert_eq!(rupees_to_paise(1e14), Err(PaiseRefusal::OutOfRange));
        assert_eq!(
            rupees_to_paise(10_000_000_000_000.02),
            Err(PaiseRefusal::OutOfRange)
        );
        assert_eq!(rupees_to_paise(f64::MAX), Err(PaiseRefusal::OutOfRange));
        // Exact values and the edges.
        assert_eq!(rupees_to_paise(100.25), Ok(10_025));
        assert_eq!(rupees_to_paise(0.01), Ok(1));
        assert_eq!(rupees_to_paise(0.005), Ok(1));
        assert_eq!(rupees_to_paise(1e13), Ok(MAX_PRICE_PAISE));
        // Nearest, half away from zero: 0.125 is exact in binary, so 12.5
        // paise rounds up to 13; 99.99 x 100 is 9998.999... in f64 and
        // still lands on 9999, 0.07 x 100 is 7.000...1 and lands on 7.
        assert_eq!(rupees_to_paise(0.125), Ok(13));
        assert_eq!(rupees_to_paise(0.124), Ok(12));
        assert_eq!(rupees_to_paise(99.99), Ok(9_999));
        assert_eq!(rupees_to_paise(0.07), Ok(7));
        assert_eq!(rupees_to_paise(24_567.85), Ok(2_456_785));
        assert_eq!(MAX_PRICE_PAISE_F64 as i64, MAX_PRICE_PAISE);
    }

    #[test]
    fn test_worst_fill_next_second_candidate_is_the_literal_next_second() {
        // D+1 then D+60: the gap means nothing traded at D+2, so the D+1
        // slice is priced from D+1 and the decision bar only, never from the
        // D+60 bar.
        let secs = [
            bar(DECISION + 1, 10_000, 9_990, 400),
            bar(DECISION + 60, 10_500, 9_400, 400),
        ];
        let mut p = params(Side::Buy, 150);
        p.max_seconds = 120;
        let (r, s) = run(&p, &secs, None);
        assert!(r.is_ok());
        // max(10000, decision 10010) + 5.
        assert_eq!(s[0].sec, DECISION + 1);
        assert_eq!(s[0].price_paise, 10_015);
        // D+60 has no D+61: max(10500, 10010) + 5.
        assert_eq!(s[1].sec, DECISION + 60);
        assert_eq!(s[1].price_paise, 10_505);
        // Sell mirror: min(9990, 9990) - 5, not the D+60 low.
        let mut p = params(Side::Sell, 150);
        p.max_seconds = 120;
        let (_, s) = run(&p, &secs, None);
        assert_eq!(s[0].price_paise, 9_985);
        // A gap still has to be in order.
        let back = [
            bar(DECISION + 2, 10_000, 9_990, 400),
            bar(DECISION + 2, 10_000, 9_990, 400),
        ];
        assert_eq!(
            run(&params(Side::Buy, 1_000), &back, None).0,
            Err(FillRefusal::SecondsOutOfOrder)
        );
    }
}
