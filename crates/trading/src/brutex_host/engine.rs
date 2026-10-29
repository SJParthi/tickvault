//! The one boundary where brutex's strategy crate plugs in (contract §2.2).
//!
//! tickvault never implements strategy logic itself (rule
//! `brutex-strategy-host-2026-10-29.md`). When the crate exists, an adapter maps
//! its API onto [`StrategyEngine`]. [`ReferenceDouble`] is a TEST DOUBLE with a
//! fixed, documented toy rule, used only by tests and the fixture dry run; it is
//! not a strategy and never trades.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use super::recipe::{ParamSnapshot, Recipe};
use super::{InstrumentKey, Refusal};

/// One closed candle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarInput {
    pub key: InstrumentKey,
    /// Index into the recipe's `timeframes`.
    pub timeframe: u8,
    /// Bar open time, IST seconds since the epoch.
    pub open_ist_secs: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
}

/// One live tick, as tickvault can supply it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TickInput {
    pub key: InstrumentKey,
    pub exchange_time_ist_secs: u32,
    pub receipt_ns: i64,
    pub ltp: f64,
    pub cumulative_volume: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntentKind {
    Entry,
    Exit,
}

/// What the crate asks tickvault to do. tickvault's own risk checks come after.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderIntent {
    pub key: InstrumentKey,
    pub timeframe: u8,
    pub kind: IntentKind,
    pub side: Side,
    pub qty_lots: u32,
    /// 0.0 = market.
    pub entry_price: f64,
    pub stop_loss: f64,
    pub target: f64,
    pub intent_id: u64,
    pub at_ist_secs: i64,
}

/// A fixed-capacity sink: no allocation on the hot path. Pushing past the
/// capacity is refused and counted, never grown.
#[derive(Debug)]
pub struct IntentSink {
    buf: Vec<OrderIntent>,
    cap: usize,
    overflowed: u64,
}

impl IntentSink {
    /// Allocates once, at construction (cold).
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
            overflowed: 0,
        }
    }
    /// O(1), no allocation.
    pub fn push(&mut self, intent: OrderIntent) -> bool {
        if self.buf.len() >= self.cap {
            self.overflowed = self.overflowed.saturating_add(1);
            return false;
        }
        self.buf.push(intent);
        true
    }
    #[must_use]
    pub fn intents(&self) -> &[OrderIntent] {
        &self.buf
    }
    #[must_use]
    pub fn overflowed(&self) -> u64 {
        self.overflowed
    }
    /// O(1) (truncate keeps capacity).
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// The boundary. Contract R3–R7: deterministic, no I/O, no blocking, no
/// allocation in `on_tick`/`on_bar`/`bar_values` after `configure`, O(1) per call
/// in the number of instruments, never panics.
pub trait StrategyEngine {
    /// Sizes all state. Cold; may allocate.
    fn configure(
        &mut self,
        recipe: &Recipe,
        params: Option<&ParamSnapshot>,
        universe: &[InstrumentKey],
    ) -> Result<(), Refusal>;
    fn on_tick(&mut self, tick: &TickInput, out: &mut IntentSink);
    fn on_bar(&mut self, bar: &BarInput, out: &mut IntentSink);
    /// The value columns, in order, that `bar_values` writes (must equal the
    /// check file's header).
    fn value_columns(&self) -> &[String];
    /// The values after the latest bar of `(key, timeframe)`; returns false if
    /// that instrument has had no bar.
    fn bar_values(&self, key: InstrumentKey, timeframe: u8, out: &mut [f64]) -> bool;
}

/// TEST DOUBLE. Toy rule, documented so fixtures can be written by hand:
/// values `[close, prev_close]`; an `Entry`/`Buy` of 1 lot when close > prev
/// close, an `Exit`/`Sell` when close < prev close, else nothing; market entry,
/// stop-loss `close - 1.0`, target `close + 2.0`. Not a strategy; never trades.
#[derive(Debug, Default)]
pub struct ReferenceDouble {
    columns: Vec<String>,
    slots: std::collections::HashMap<(InstrumentKey, u8), (f64, f64)>,
    next_id: u64,
}

impl ReferenceDouble {
    #[must_use]
    pub fn new() -> Self {
        Self {
            columns: vec!["close".to_owned(), "prev_close".to_owned()],
            slots: std::collections::HashMap::with_capacity(64),
            next_id: 1,
        }
    }
}

impl StrategyEngine for ReferenceDouble {
    fn configure(
        &mut self,
        recipe: &Recipe,
        _params: Option<&ParamSnapshot>,
        universe: &[InstrumentKey],
    ) -> Result<(), Refusal> {
        if universe.is_empty() {
            return Err(Refusal::Engine("empty universe".to_owned()));
        }
        self.slots.clear();
        self.slots.reserve(universe.len() * recipe.timeframes.len());
        self.next_id = 1;
        Ok(())
    }

    fn on_tick(&mut self, _tick: &TickInput, _out: &mut IntentSink) {}

    fn on_bar(&mut self, bar: &BarInput, out: &mut IntentSink) {
        let slot = self
            .slots
            .entry((bar.key, bar.timeframe))
            .or_insert((f64::NAN, f64::NAN));
        let prev = slot.0;
        *slot = (bar.close, prev);
        if prev.is_nan() {
            return;
        }
        let (kind, side) = if bar.close > prev {
            (IntentKind::Entry, Side::Buy)
        } else if bar.close < prev {
            (IntentKind::Exit, Side::Sell)
        } else {
            return;
        };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        out.push(OrderIntent {
            key: bar.key,
            timeframe: bar.timeframe,
            kind,
            side,
            qty_lots: 1,
            entry_price: 0.0,
            stop_loss: bar.close - 1.0,
            target: bar.close + 2.0,
            intent_id: id,
            at_ist_secs: bar.open_ist_secs,
        });
    }

    fn value_columns(&self) -> &[String] {
        &self.columns
    }

    fn bar_values(&self, key: InstrumentKey, timeframe: u8, out: &mut [f64]) -> bool {
        match self.slots.get(&(key, timeframe)) {
            Some(&(close, prev)) if out.len() >= 2 => {
                out[0] = close;
                out[1] = if prev.is_nan() { 0.0 } else { prev };
                true
            }
            _ => false,
        }
    }
}
// O(1) EXEMPT: end
