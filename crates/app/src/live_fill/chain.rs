//! The paper fill chain itself (plan item LF-6b): ticks in, strategy candles,
//! the approved strategy engine, held would-be orders, paper commands and
//! event rows out.
//!
//! Pure: no clock, no channel, no thread. [`LiveFillChain::on_tap`] takes one
//! tick, [`LiveFillChain::on_pass`] takes the wall and monotonic clocks, and
//! everything the chain wants sent is left in an outbox the thread drains
//! ([`LiveFillChain::drain_outputs`]). That keeps every rule testable with a
//! hand-driven clock.
//!
//! PAPER ONLY. The chain's only way out is [`PaperCmd`], which the order
//! runtime refuses unless its OMS is dry-run. The chain defines no strategy:
//! every decision comes from the engine it was given (brutex's crate, behind
//! [`StrategyEngine`]).
//!
//! # Per tick (the hot path of the `tv-live-fill` thread)
//!
//! [`LiveFillChain::on_tap`]: the stream watch (O(1)), the held-order taint
//! check when the watch tainted a stretch (O(64), rare), then for a tick that
//! carried a NEW trade: the candle book's `on_tick` (one hash probe plus
//! O(frames)), one more hash probe for the instrument's index (book quote,
//! late-price marks), the engine's `on_tick` (its own O(1) contract), and one
//! queue push per bar the tick closed (O(opening ends) for its release
//! instant). A repeat quote or a first-sight packet (`new_trade` false) only
//! refreshes the best bid/ask and the volume baseline (two hash probes): its
//! trade time and price are old and never reach a candle or the engine. A
//! real but OLDER trade (`older_trade`) goes through the candle book only:
//! its price joins the bar it belongs to without moving a close, or counts
//! late on a closed bar; it never reaches the engine. No allocation unless
//! the engine asks for an order on a tick (refused with a row; rows
//! allocate). DHAT-gated by `crates/app/tests/dhat_live_fill.rs`.
//!
//! An exit intent is reduce-only against the paper position the chain sent
//! (refused `exit_without_position` when none), and the runtime clamps it
//! again against the position it booked. Once an order filled something, the
//! instrument's 1 s closes are marked for the rest of the day.
//!
//! # Per pass
//!
//! [`LiveFillChain::on_pass`]: the clock-step check (O(1)), the book clock
//! (O(frames), plus the book's per-boundary scan), the release queue (see
//! `timing`), one hash probe per released 1 s bar, the engine's `on_bar` per
//! released strategy bar, and the held-order scan (O(64)). Per order (not per
//! tick): the limit price, `worst_fill`, and the event rows, which allocate
//! their text fields.

use std::collections::HashMap;

use tickvault_common::price_precision::f32_to_f64_clean;
use tickvault_common::types::ExchangeSegment;
use tickvault_storage::live_order_events_persistence::{
    LIVE_ORDER_EVENTS_FEED_DHAN, LIVE_ORDER_EVENTS_SYMBOL_NA, LiveCandleCloseBy, LiveLimitBound,
    LiveOrderEvent, LiveOrderEventKind, LiveOrderMode, LiveOrderSide,
};
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::engine::{
    IntentKind, IntentSink, OrderIntent, Side, StrategyEngine, TickInput,
};
use tickvault_trading::brutex_host::live_candles::{
    CloseBy, ClosedBar, ClosedBars, LiveCandleBook, TickVerdict,
};
use tickvault_trading::brutex_host::order_price::{LimitBound, marketable_limit};
use tickvault_trading::brutex_host::recipe::{ParamSnapshot, Recipe};
use tickvault_trading::brutex_host::worst_fill::{SecondBar, rupees_to_paise};

use super::fill_window::{Finished, HeldOrder, MAX_PENDING_ORDERS, PendingBook};
use super::taint::{
    FEED_GAP_NS, StreamWatch, TaintRanges, TaintReason, chain_start_taint, session_open_ns,
};
use super::timing::{
    BookClock, FEED_WATCH_CLOSE_SOD, ReleaseQueue, SESSION_OPEN_SOD, bar_in_session,
};
use crate::dhan_feed_stack::live_fill_tap::TapTick;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;
/// Intents one engine call may return.
pub const INTENT_SINK_CAPACITY: usize = 64;
/// Outbox slots kept for decision rows and finished orders (every intent of
/// the released bars plus every finished order, with room to spare). Marks
/// never take these slots (LF-6b review round 3, finding 1).
const OUTBOX_ORDER_ROOM: usize = INTENT_SINK_CAPACITY * 8 + MAX_PENDING_ORDERS * 4;
/// Reason on a refusal row when a late price reached the decision candle.
pub const REASON_LATE_PRICE: &str = "skip:late_price";
/// Reason on a refusal row when the instrument's first packet of the day came
/// after the decision candle opened, so the candle is partial.
pub const REASON_FIRST_BAR_PARTIAL: &str = "first_bar_partial";

/// Tick size, lot size and label of one tradable contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractSpec {
    pub tick_paise: i64,
    pub lot_size: u32,
    pub symbol: String,
}

/// What the chain needs for one trading day.
#[derive(Debug, Clone)]
pub struct ChainSetup {
    pub recipe: Recipe,
    pub snapshot: Option<ParamSnapshot>,
    /// The recipe's `timeframes`, in its order, as whole seconds.
    pub engine_frames_secs: Vec<u32>,
    pub universe: Vec<InstrumentKey>,
    /// Built from the day's contract artifact; a contract missing here has
    /// its orders refused.
    pub contracts: HashMap<InstrumentKey, ContractSpec>,
}

/// The chain's tuning, from `[live_fill]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainParams {
    pub wait_ms: u64,
    pub open_wait_ms: u64,
    pub band_bps: u32,
    pub participation_bps: u32,
    pub max_fill_seconds: u32,
}

impl From<&tickvault_common::config::LiveFillConfig> for ChainParams {
    fn from(c: &tickvault_common::config::LiveFillConfig) -> Self {
        Self {
            wait_ms: c.wait_ms,
            open_wait_ms: c.open_wait_ms,
            band_bps: c.band_bps,
            participation_bps: c.participation_bps,
            max_fill_seconds: c.max_fill_seconds,
        }
    }
}

/// One paper order, fully worked out, for the order runtime. The runtime
/// places it on the dry-run OMS, applies the fill, cancels the remainder and
/// writes the Sent / Ack / Fill / Cancel (or Reject) rows from `base`.
#[derive(Debug, Clone)]
pub struct PaperExecution {
    /// The signal's row template (the runtime sets event, seq, ts and the
    /// order fields).
    pub base: LiveOrderEvent,
    /// The first `seq` the runtime may use.
    pub next_seq: i32,
    pub security_id: u64,
    pub segment: ExchangeSegment,
    pub side: Side,
    /// Shares ordered.
    pub qty: i64,
    pub lot_size: u32,
    /// The marketable limit, rupees.
    pub limit_price: f64,
    /// Shares the worst-case model filled, rounded down to whole lots.
    pub filled_qty: i64,
    /// Their average price, rupees (0 when nothing filled).
    pub avg_price: f64,
    /// The same, paise (the model price on the Fill row).
    pub avg_price_paise: Option<i64>,
    /// The strategy asked to EXIT: the runtime makes it reduce-only against
    /// the paper position it actually holds (never a reversal).
    pub exit: bool,
}

/// What the chain sends to the order runtime.
#[derive(Debug, Clone)]
pub enum PaperCmd {
    Execute(Box<PaperExecution>),
    /// A 1 s close for an instrument this chain holds a paper position in
    /// (owner rule: mark paper positions every second). The runtime applies
    /// it only while the risk engine still holds a position there.
    Mark {
        security_id: u64,
        segment: ExchangeSegment,
        price: f64,
    },
}

/// One item for the thread to send.
#[derive(Debug, Clone)]
pub enum ChainOutput {
    Event(Box<LiveOrderEvent>),
    Cmd(PaperCmd),
}

/// Why the chain could not be built.
#[derive(Debug, Clone, PartialEq)]
pub enum ChainError {
    /// The strategy engine refused its configuration.
    Engine(String),
    /// The candle book refused its configuration.
    Book(String),
    /// More candle sizes than a `u8` frame index holds.
    TooManyFrames(usize),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(e) => write!(f, "the strategy engine refused its configuration: {e}"),
            Self::Book(e) => write!(f, "the candle book refused its configuration: {e}"),
            Self::TooManyFrames(n) => write!(f, "{n} candle sizes, at most 255"),
        }
    }
}

impl std::error::Error for ChainError {}

/// Counters the thread publishes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChainCounters {
    pub ticks: u64,
    pub ticks_refused: u64,
    pub late_ticks: u64,
    pub bars_released: u64,
    pub bars_lost_queue_full: u64,
    pub intents: u64,
    pub refused: u64,
    pub held: u64,
    pub sent: u64,
    pub late_price_skipped: u64,
    pub clock_steps: u64,
    pub marks: u64,
    /// Marks folded into the same instrument's earlier mark of this pass
    /// (only the newest second's close is sent).
    pub marks_coalesced: u64,
    pub outbox_overflow: u64,
    /// Filled paper orders that could not be queued (the outbox was full):
    /// the chain's own position count was left unchanged.
    pub executions_lost: u64,
}

/// Context carried with a held order, for its rows.
#[derive(Debug, Clone)]
struct OrderMeta {
    base: LiveOrderEvent,
    lot_size: u32,
    limit_paise: i64,
    exit: bool,
}

/// Last best quote per instrument (from the tap's Full packets).
#[derive(Debug, Clone, Copy, Default)]
struct Quote {
    bid: f32,
    ask: f32,
    bid_qty: u32,
    ask_qty: u32,
}

/// The newest released 1 s bar per instrument.
#[derive(Debug, Clone, Copy)]
struct LastSecond {
    bar: SecondBar,
}

/// See the module docs.
#[derive(Debug)]
pub struct LiveFillChain<E: StrategyEngine> {
    engine: E,
    setup: ChainSetup,
    params: ChainParams,
    /// Book frames: index 0 is the hidden 1 s frame.
    frames_secs: Vec<u32>,
    /// Engine timeframe index per book frame.
    engine_tf: Vec<Option<u8>>,
    book: LiveCandleBook,
    clock: BookClock,
    queue: ReleaseQueue,
    closed: ClosedBars,
    released: Vec<ClosedBar>,
    index: HashMap<InstrumentKey, u32, ahash::RandomState>,
    /// Per (instrument, frame): the newest bucket open (IST s) a late price
    /// landed in; `i64::MIN` for none.
    late_bucket: Vec<i64>,
    quotes: Vec<Quote>,
    last_second: Vec<Option<LastSecond>>,
    /// Signed shares this chain's paper fills hold, per instrument, as sent:
    /// the runtime may book less (a refusal), so this only sizes an exit
    /// early; the runtime's own clamp is the authority.
    net_shares: Vec<i64>,
    /// Per instrument: an Execute that filled something was sent today. Its
    /// 1 s closes are marked from then on (the runtime ignores a mark for a
    /// flat position), so a refused closing order cannot stop the marks.
    traded_today: Vec<bool>,
    /// Per instrument: the IST second its first tapped packet (of any kind)
    /// was received, `i64::MIN` when that was inside the session's opening
    /// release window (listening from the open), `i64::MAX` before any packet.
    /// A bar opening before it was built from part of its bucket only
    /// (`first_bar_partial`; LF-6b review round 2, finding 2).
    first_seen_secs: Vec<i64>,
    /// The newest tapped tick's receipt trailed the last pass's wall clock by
    /// more than the wait: the frame drain is behind, so passes run on the
    /// newest receipt, not the wall (round 2, finding 5).
    stream_lagging: bool,
    taints: TaintRanges,
    watch: StreamWatch,
    pending: PendingBook<OrderMeta>,
    sink: IntentSink,
    ready: Vec<usize>,
    outbox: Vec<ChainOutput>,
    outbox_cap: usize,
    /// Per instrument: the outbox position of its newest mark, `u32::MAX` for
    /// none. Valid only at or above `pass_floor` and while that slot still
    /// holds a mark for the same instrument.
    mark_slot: Vec<u32>,
    /// The outbox length when this pass began: a mark is folded only into one
    /// queued by the same pass, so it never moves ahead of an earlier pass's
    /// Execute.
    pass_floor: usize,
    day: i64,
    signal_base: i64,
    signal_next: i64,
    now_ns: i64,
    counters: ChainCounters,
}

impl<E: StrategyEngine> LiveFillChain<E> {
    // O(1) EXEMPT: begin — cold path: built once per trading day before the session; it is where every buffer the chain uses is sized.
    /// Configures the engine and sizes every buffer. Cold; the chain's only
    /// allocations besides event rows.
    ///
    /// # Errors
    /// The engine or the candle book refused, or too many candle sizes.
    pub fn new(
        mut engine: E,
        setup: ChainSetup,
        params: ChainParams,
        now_ist_ns: i64,
    ) -> Result<Self, ChainError> {
        engine
            .configure(&setup.recipe, setup.snapshot.as_ref(), &setup.universe)
            .map_err(|e| ChainError::Engine(e.to_string()))?;
        let mut frames_secs: Vec<u32> = Vec::with_capacity(setup.engine_frames_secs.len() + 1);
        frames_secs.push(1);
        for &f in &setup.engine_frames_secs {
            if !frames_secs.contains(&f) {
                frames_secs.push(f);
            }
        }
        if frames_secs.len() > 255 {
            return Err(ChainError::TooManyFrames(frames_secs.len()));
        }
        let mut engine_tf = vec![None; frames_secs.len()];
        for (tf, &f) in setup.engine_frames_secs.iter().enumerate() {
            if let Some(fi) = frames_secs.iter().position(|&x| x == f)
                && engine_tf[fi].is_none()
            {
                engine_tf[fi] = u8::try_from(tf).ok();
            }
        }
        let book = LiveCandleBook::configure(&setup.universe, &frames_secs)
            .map_err(|e| ChainError::Book(e.to_string()))?;
        let keys = setup.universe.len();
        let nf = frames_secs.len();
        let bar_cap = keys
            .saturating_mul(bars_waiting_per_key(&frames_secs, params.open_wait_ms))
            .max(1);
        let mut index = HashMap::with_capacity_and_hasher(keys, ahash::RandomState::new());
        for (i, k) in setup.universe.iter().enumerate() {
            index.insert(*k, u32::try_from(i).unwrap_or(u32::MAX));
        }
        // Outbox: one mark per instrument (a pass folds an instrument's marks
        // into one), plus the room kept for decision rows and finished orders.
        let outbox_cap = keys.saturating_add(OUTBOX_ORDER_ROOM);
        let mut chain = Self {
            clock: BookClock::new(&frames_secs, params.wait_ms, params.open_wait_ms),
            queue: ReleaseQueue::with_capacity(bar_cap),
            closed: ClosedBars::with_capacity(bar_cap),
            released: Vec::with_capacity(bar_cap),
            late_bucket: vec![i64::MIN; keys * nf],
            quotes: vec![Quote::default(); keys],
            last_second: vec![None; keys],
            net_shares: vec![0; keys],
            traded_today: vec![false; keys],
            first_seen_secs: vec![i64::MAX; keys],
            stream_lagging: false,
            taints: TaintRanges::new(),
            watch: StreamWatch::new(),
            pending: PendingBook::new(params.max_fill_seconds, keys),
            sink: IntentSink::with_capacity(INTENT_SINK_CAPACITY),
            ready: Vec::with_capacity(MAX_PENDING_ORDERS),
            outbox: Vec::with_capacity(outbox_cap),
            outbox_cap,
            mark_slot: vec![u32::MAX; keys],
            pass_floor: 0,
            day: now_ist_ns
                .div_euclid(NANOS_PER_SEC)
                .div_euclid(SECS_PER_DAY),
            signal_base: now_ist_ns.max(0),
            signal_next: 0,
            now_ns: now_ist_ns,
            counters: ChainCounters::default(),
            engine,
            setup,
            params,
            frames_secs,
            engine_tf,
            book,
            index,
        };
        chain.taint_chain_start(now_ist_ns);
        Ok(chain)
    }

    /// A start after 09:15:00 (a restart): every first bar is partial and the
    /// volume baseline swallows the first tick, so the stretch up to the end
    /// of the longest bucket open now is untrusted (`chain_start`). O(frames).
    fn taint_chain_start(&mut self, now_ist_ns: i64) {
        let longest = self.frames_secs.iter().copied().max().unwrap_or(1);
        if let Some((lo, hi)) = chain_start_taint(now_ist_ns, longest) {
            self.taints.add(lo, hi, TaintReason::ChainStart);
        }
    }

    /// A new trading day: a fresh candle book (the vendor's cumulative volume
    /// restarts), the engine configured again, every held order and taint
    /// dropped. Cold.
    ///
    /// # Errors
    /// The engine or the book refused.
    pub fn new_day(&mut self, now_ist_ns: i64) -> Result<usize, ChainError> {
        self.engine
            .configure(
                &self.setup.recipe,
                self.setup.snapshot.as_ref(),
                &self.setup.universe,
            )
            .map_err(|e| ChainError::Engine(e.to_string()))?;
        self.book = LiveCandleBook::configure(&self.setup.universe, &self.frames_secs)
            .map_err(|e| ChainError::Book(e.to_string()))?;
        self.clock = BookClock::new(
            &self.frames_secs,
            self.params.wait_ms,
            self.params.open_wait_ms,
        );
        let mut drained = Vec::with_capacity(self.released.capacity());
        self.queue.release_due(i64::MAX, &mut drained);
        self.late_bucket.iter_mut().for_each(|v| *v = i64::MIN);
        self.quotes.iter_mut().for_each(|q| *q = Quote::default());
        self.last_second.iter_mut().for_each(|s| *s = None);
        self.net_shares.iter_mut().for_each(|n| *n = 0);
        self.traded_today.iter_mut().for_each(|t| *t = false);
        self.first_seen_secs.iter_mut().for_each(|s| *s = i64::MAX);
        self.stream_lagging = false;
        self.taints.clear();
        self.watch.reset();
        self.day = now_ist_ns
            .div_euclid(NANOS_PER_SEC)
            .div_euclid(SECS_PER_DAY);
        let dropped = self.pending.clear();
        self.taint_chain_start(now_ist_ns);
        Ok(dropped)
    }
    // O(1) EXEMPT: end

    /// The counters so far.
    #[must_use]
    pub fn counters(&self) -> ChainCounters {
        self.counters
    }

    /// Orders held now.
    #[must_use]
    pub fn held_orders(&self) -> usize {
        self.pending.held()
    }

    /// The taint ranges added per reason, in [`TaintReason::ALL`] order.
    #[must_use]
    pub fn taints_added(&self) -> [u64; TaintReason::COUNT] {
        self.taints.added()
    }

    /// The IST day the chain is on.
    #[must_use]
    pub fn day(&self) -> i64 {
        self.day
    }

    /// The engine (tests and the thread's status line).
    #[must_use]
    pub fn engine(&self) -> &E {
        &self.engine
    }

    #[inline]
    fn key_index(&self, key: &InstrumentKey) -> Option<u32> {
        self.index.get(key).copied()
    }

    /// One tick from the tap. See the module docs for the cost.
    pub fn on_tap(&mut self, t: &TapTick) {
        self.counters.ticks = self.counters.ticks.saturating_add(1);
        let key = InstrumentKey {
            security_id: t.security_id,
            exchange_segment: t.segment,
        };
        let secs = i64::from(t.exchange_secs);
        let receipt = t.receipt_ist_ns;
        let sod = receipt.div_euclid(NANOS_PER_SEC).rem_euclid(SECS_PER_DAY);
        // The feed-gap watch runs to the end of F&O trading (15:40), past the
        // 15:30 strategy close, while held orders still fill (finding 4).
        let in_session = (SESSION_OPEN_SOD..FEED_WATCH_CLOSE_SOD).contains(&sod);
        self.watch
            .on_tick(t.drop_seq, secs, receipt, in_session, &mut self.taints);
        self.apply_fresh_taints();
        // The frame drain's lag behind this thread's last pass (finding 5).
        // O(1): one subtraction and one compare.
        self.stream_lagging = self.now_ns.saturating_sub(receipt) > self.wait_ns();
        // One hash probe, reused below.
        let ki = self.key_index(&key);
        if let Some(ki) = ki {
            self.note_first_seen(ki, receipt);
        }
        if t.older_trade && !t.new_trade {
            self.on_older_trade(key, ki, t);
            return;
        }
        if !t.new_trade {
            // A repeat quote (the book moved, no trade) or a first packet
            // after a subscribe: its trade time, price and cumulative are OLD.
            // Folding it would count it late on a closed bucket and skip an
            // order on a candle no new price reached. Only its best bid/ask
            // and its volume baseline are used. O(1): two hash probes.
            if let Some(ki) = ki {
                self.refresh_quote(ki, t);
            }
            let _known = self.book.note_baseline_volume(key, u64::from(t.cum_volume));
            return;
        }
        let ltp = f32_to_f64_clean(t.ltp);
        self.closed.clear();
        let verdict = self.book.on_tick(
            key,
            t.exchange_secs,
            receipt,
            ltp,
            u64::from(t.cum_volume),
            &mut self.closed,
        );
        let late = match verdict {
            TickVerdict::Accepted { late_frames } => late_frames > 0,
            _ => {
                self.counters.ticks_refused = self.counters.ticks_refused.saturating_add(1);
                return;
            }
        };
        if let Some(ki) = ki {
            self.refresh_quote(ki, t);
            if late {
                self.on_late(ki, secs, ltp);
            }
        }
        self.queue_closed();
        self.apply_fresh_taints();
        self.sink.clear();
        let input = TickInput {
            key,
            exchange_time_ist_secs: t.exchange_secs,
            receipt_ns: receipt,
            ltp,
            cumulative_volume: u64::from(t.cum_volume),
        };
        self.engine.on_tick(&input, &mut self.sink);
        if !self.sink.intents().is_empty() {
            // Orders are decided at candle close only (the backtest's rule);
            // a tick-time order is refused with a row.
            for i in 0..self.sink.intents().len() {
                let intent = self.sink.intents()[i];
                self.counters.intents = self.counters.intents.saturating_add(1);
                self.refuse_unplaced(&intent, None, "tick_intent_unsupported");
            }
            self.sink.clear();
        }
    }

    /// A real trade older than the newest one the fold has seen (a stale
    /// cumulative regression, a backup copy accepted late; LF-6b review
    /// finding 4). The candle book folds its price into the bar it belongs to
    /// without moving a close, or counts it late on a closed bar, which skips
    /// an order on that candle (owner rule). Never handed to the engine: its
    /// trade time is not the newest. O(1) hash probes plus O(frames).
    fn on_older_trade(&mut self, key: InstrumentKey, ki: Option<u32>, t: &TapTick) {
        let ltp = f32_to_f64_clean(t.ltp);
        self.closed.clear();
        let verdict = self.book.on_tick(
            key,
            t.exchange_secs,
            t.receipt_ist_ns,
            ltp,
            u64::from(t.cum_volume),
            &mut self.closed,
        );
        let TickVerdict::Accepted { late_frames } = verdict else {
            self.counters.ticks_refused = self.counters.ticks_refused.saturating_add(1);
            return;
        };
        if let Some(ki) = ki {
            self.refresh_quote(ki, t);
            if late_frames > 0 {
                self.on_late(ki, i64::from(t.exchange_secs), ltp);
            }
        }
        self.queue_closed();
        self.apply_fresh_taints();
    }

    /// The tap's loss total, read out of band by the thread before each pass
    /// (LF-6b review finding 3): a loss behind ticks still queued taints the
    /// stretch it touched before anything is released. O(1).
    pub fn note_tap_loss(&mut self, total: u64) {
        // APPROVED: the low 32 bits are the `drop_seq` the ticks carry.
        #[allow(clippy::cast_possible_truncation)]
        let seq = total as u32;
        self.watch.on_loss_total(seq, &mut self.taints);
        self.apply_fresh_taints();
    }

    /// The configured wait after a candle's end, nanoseconds. O(1).
    #[inline]
    fn wait_ns(&self) -> i64 {
        i64::try_from(self.params.wait_ms)
            .unwrap_or(i64::MAX / 2)
            .saturating_mul(1_000_000)
    }

    /// Records instrument `ki`'s first tapped packet of the day (LF-6b review
    /// round 2, finding 2): one array read, plus one write on the first
    /// packet. A packet received inside the opening release window (09:15:00
    /// plus one second plus `open_wait`) means the instrument was subscribed
    /// before the open, so nothing it traded was missed. O(1).
    #[inline]
    fn note_first_seen(&mut self, ki: u32, receipt_ns: i64) {
        let Some(slot) = self.first_seen_secs.get_mut(ki as usize) else {
            return;
        };
        if *slot != i64::MAX {
            return;
        }
        let open_wait_ns = i64::try_from(self.params.open_wait_ms)
            .unwrap_or(i64::MAX / 2)
            .saturating_mul(1_000_000);
        let from_open = session_open_ns(receipt_ns)
            .saturating_add(NANOS_PER_SEC)
            .saturating_add(open_wait_ns);
        *slot = if receipt_ns <= from_open {
            i64::MIN
        } else {
            receipt_ns.div_euclid(NANOS_PER_SEC)
        };
    }

    /// The tick's best bid/ask, when its packet carried depth. O(1).
    #[inline]
    fn refresh_quote(&mut self, ki: u32, t: &TapTick) {
        if (t.best_bid > 0.0 || t.best_ask > 0.0)
            && let Some(q) = self.quotes.get_mut(ki as usize)
        {
            *q = Quote {
                bid: t.best_bid,
                ask: t.best_ask,
                bid_qty: t.best_bid_qty,
                ask_qty: t.best_ask_qty,
            };
        }
    }

    /// Hands every stretch tainted since the last call to the held orders, so
    /// a drop, gap or step learned AFTER a decision still refuses an order
    /// whose candle or fill seconds it touches. O(1) when nothing is new, else
    /// O(64).
    #[inline]
    fn apply_fresh_taints(&mut self) {
        if let Some((lo, hi, reason)) = self.taints.take_fresh() {
            self.pending.on_taint(lo, hi, reason);
        }
    }

    /// A late tick of instrument `ki` at exchange second `secs`: records the
    /// bucket it belonged to on each frame that counted it late. Rare.
    fn on_late(&mut self, ki: u32, secs: i64, ltp: f64) {
        self.counters.late_ticks = self.counters.late_ticks.saturating_add(1);
        let price_paise = rupees_to_paise(ltp).ok();
        let nf = self.frames_secs.len();
        for fi in 0..nf {
            if !self.book.last_tick_late(fi) {
                continue;
            }
            let open = bucket_open(secs, self.frames_secs[fi]);
            let slot = &mut self.late_bucket[ki as usize * nf + fi];
            if open > *slot {
                *slot = open;
            }
            self.pending
                .on_late_tick(ki, u8::try_from(fi).unwrap_or(u8::MAX), secs, price_paise);
        }
    }

    /// Moves the bars the book just closed into the release queue. A bar the
    /// queue cannot hold taints its stretch, so no order is placed on what
    /// the strategy did not see.
    fn queue_closed(&mut self) {
        for i in 0..self.closed.bars().len() {
            let bar = self.closed.bars()[i];
            let rel = self
                .clock
                .release_ns(bar.bucket_open_secs, bar.bucket_end_secs);
            if !self.queue.push(bar, rel) {
                self.counters.bars_lost_queue_full =
                    self.counters.bars_lost_queue_full.saturating_add(1);
                self.taints.add(
                    bar.bucket_open_secs,
                    bar.bucket_end_secs.saturating_sub(1),
                    TaintReason::TickDropped,
                );
            }
        }
        self.closed.clear();
    }

    /// One pass of the thread: clocks in, due candles to the strategy, held
    /// orders finished. See the module docs for the cost.
    ///
    /// # Errors
    /// A day roll whose reconfiguration was refused (the chain then stops).
    pub fn on_pass(&mut self, wall_ist_ns: i64, mono_ns: i64) -> Result<(), ChainError> {
        self.on_pass_bounded(wall_ist_ns, mono_ns, false)
    }

    /// [`Self::on_pass`] for a thread that may have left ticks queued
    /// (`backlogged`: its last drain stopped at the per-pass cap). The book
    /// clock, the releases and the held orders' final seconds then run on the
    /// newest receipt read (never past the wall), not on the wall: the
    /// measured wait covers receipt to final price, not this thread's own
    /// backlog, so a second whose ticks are still queued is never closed,
    /// decided or filled (LF-6b review finding 3). O(1) more.
    ///
    /// The same holds when the FRAME DRAIN upstream is behind (round 2,
    /// finding 5): the tap ring is then empty, but the newest tick read
    /// trailed the last pass's wall by more than the wait, so its seconds may
    /// still be on their way. Until a tick arrives on time again, the pass
    /// runs on the newest receipt too, unless the stream has been silent for
    /// a feed gap: the gap watch has tainted that stretch, and the clock may
    /// move on the wall again.
    ///
    /// # Errors
    /// As [`Self::on_pass`].
    pub fn on_pass_bounded(
        &mut self,
        wall_ist_ns: i64,
        mono_ns: i64,
        backlogged: bool,
    ) -> Result<(), ChainError> {
        let day = wall_ist_ns
            .div_euclid(NANOS_PER_SEC)
            .div_euclid(SECS_PER_DAY);
        if day != self.day {
            self.new_day(wall_ist_ns)?;
        }
        self.pass_floor = self.outbox.len();
        self.now_ns = wall_ist_ns;
        if self.watch.on_pass(wall_ist_ns, mono_ns, &mut self.taints) {
            self.counters.clock_steps = self.counters.clock_steps.saturating_add(1);
        }
        let drain_behind = self.stream_lagging
            && self
                .watch
                .newest_receipt_ns()
                .is_some_and(|r| wall_ist_ns.saturating_sub(r) < FEED_GAP_NS);
        let stream_ns = if backlogged || drain_behind {
            self.watch
                .newest_receipt_ns()
                .map_or(wall_ist_ns, |r| r.min(wall_ist_ns))
        } else {
            wall_ist_ns
        };
        let c = self.clock.reading(stream_ns);
        self.closed.clear();
        self.book.on_clock(c, &mut self.closed);
        self.queue_closed();
        // Before any candle is decided: orders already held learn of this
        // pass's drops, gaps and steps; new decisions read them via `taint_of`.
        self.apply_fresh_taints();
        self.queue.release_due(stream_ns, &mut self.released);
        for i in 0..self.released.len() {
            let bar = self.released[i];
            self.on_released(&bar);
        }
        self.released.clear();
        self.pending
            .collect_ready(stream_ns, &self.clock, &mut self.ready);
        for r in 0..self.ready.len() {
            let slot = self.ready[r];
            if let Some(done) = self.pending.finish(slot) {
                self.on_finished(done);
            }
        }
        Ok(())
    }

    /// A bar whose release instant has come.
    fn on_released(&mut self, bar: &ClosedBar) {
        self.counters.bars_released = self.counters.bars_released.saturating_add(1);
        let fi = bar.frame_index as usize;
        if fi == 0 {
            self.on_second_bar(bar);
        }
        let Some(tf) = self.engine_tf.get(fi).copied().flatten() else {
            return;
        };
        if !bar_in_session(bar.bucket_open_secs, bar.bucket_end_secs) {
            return;
        }
        let mut input = bar.to_bar_input();
        input.timeframe = tf;
        self.sink.clear();
        self.engine.on_bar(&input, &mut self.sink);
        for i in 0..self.sink.intents().len() {
            let intent = self.sink.intents()[i];
            self.counters.intents = self.counters.intents.saturating_add(1);
            self.decide(&intent, bar);
        }
        self.sink.clear();
    }

    /// A final 1 s bar: the instrument's newest second, the held orders'
    /// fill seconds, and a mark for a paper position. One hash probe.
    fn on_second_bar(&mut self, bar: &ClosedBar) {
        let Some(ki) = self.key_index(&bar.key) else {
            return;
        };
        let (Ok(high), Ok(low)) = (rupees_to_paise(bar.high), rupees_to_paise(bar.low)) else {
            return;
        };
        let sb = SecondBar {
            sec: bar.bucket_open_secs,
            high_paise: high,
            low_paise: low,
            volume: bar.volume,
        };
        self.last_second[ki as usize] = Some(LastSecond { bar: sb });
        self.pending.on_second(ki, sb);
        if self.traded_today[ki as usize]
            && let Some(segment) = ExchangeSegment::from_byte(bar.key.exchange_segment)
        {
            self.counters.marks = self.counters.marks.saturating_add(1);
            self.push_mark(ki, bar.key.security_id, segment, bar.close);
        }
    }

    /// Queues a mark (LF-6b review round 3, finding 1). An instrument's
    /// earlier mark of this pass is overwritten with the newer close, so a
    /// pass queues at most one mark per instrument, and a mark never takes the
    /// slots kept for decision rows and finished orders: a pass that releases
    /// several seconds of many traded instruments cannot crowd out an
    /// Execute. O(1): one array read and one compare.
    fn push_mark(&mut self, ki: u32, security_id: u64, segment: ExchangeSegment, price: f64) {
        let pos = self.mark_slot[ki as usize] as usize;
        if pos >= self.pass_floor
            && let Some(ChainOutput::Cmd(PaperCmd::Mark {
                security_id: s,
                segment: g,
                price: p,
            })) = self.outbox.get_mut(pos)
            && *s == security_id
            && *g == segment
        {
            *p = price;
            self.counters.marks_coalesced = self.counters.marks_coalesced.saturating_add(1);
            return;
        }
        if self.outbox.len().saturating_add(OUTBOX_ORDER_ROOM) >= self.outbox_cap {
            // Only reachable when the outbox was not drained between passes;
            // the next second's mark supersedes this one.
            self.counters.outbox_overflow = self.counters.outbox_overflow.saturating_add(1);
            return;
        }
        self.mark_slot[ki as usize] = u32::try_from(self.outbox.len()).unwrap_or(u32::MAX);
        self.outbox.push(ChainOutput::Cmd(PaperCmd::Mark {
            security_id,
            segment,
            price,
        }));
    }

    /// Queues a decision row, keeping [`MAX_PENDING_ORDERS`] slots for the
    /// finished orders of the same pass. O(1).
    fn push_out(&mut self, o: ChainOutput) {
        if self.outbox.len().saturating_add(MAX_PENDING_ORDERS) >= self.outbox_cap {
            self.counters.outbox_overflow = self.counters.outbox_overflow.saturating_add(1);
            return;
        }
        self.outbox.push(o);
    }

    /// Queues a finished order's output (an Execute or its Reject row); it may
    /// use every slot. `false` when the outbox is full. O(1).
    fn push_finished(&mut self, o: ChainOutput) -> bool {
        if self.outbox.len() >= self.outbox_cap {
            self.counters.outbox_overflow = self.counters.outbox_overflow.saturating_add(1);
            return false;
        }
        self.outbox.push(o);
        true
    }

    fn next_signal_id(&mut self) -> i64 {
        let id = self.signal_base.saturating_add(self.signal_next);
        self.signal_next = self.signal_next.saturating_add(1);
        id
    }

    /// The row template for one intent on one candle (or on a tick, `None`).
    fn base_event(
        &mut self,
        intent: &OrderIntent,
        bar: Option<&ClosedBar>,
        qty: i64,
    ) -> Option<LiveOrderEvent> {
        let segment = ExchangeSegment::from_byte(intent.key.exchange_segment)?;
        let signal_id = self.next_signal_id();
        let symbol = self.setup.contracts.get(&intent.key).map_or_else(
            || LIVE_ORDER_EVENTS_SYMBOL_NA.to_owned(),
            |c| c.symbol.clone(),
        );
        let quote = self
            .key_index(&intent.key)
            .and_then(|ki| self.quotes.get(ki as usize).copied())
            .unwrap_or_default();
        let paise = |v: f32| {
            if v > 0.0 {
                rupees_to_paise(f32_to_f64_clean(v)).ok()
            } else {
                None
            }
        };
        let qty_of = |v: u32| if v > 0 { Some(i64::from(v)) } else { None };
        Some(LiveOrderEvent {
            ts_ist_nanos: self.now_ns,
            event: LiveOrderEventKind::Decision,
            signal_id,
            seq: 0,
            feed: LIVE_ORDER_EVENTS_FEED_DHAN,
            mode: LiveOrderMode::Paper,
            recipe: self.setup.recipe.name.clone(),
            recipe_version: i32::try_from(self.setup.recipe.version).unwrap_or(i32::MAX),
            security_id: i64::try_from(intent.key.security_id).unwrap_or(i64::MAX),
            segment,
            symbol,
            side: match intent.side {
                Side::Buy => LiveOrderSide::Buy,
                Side::Sell => LiveOrderSide::Sell,
            },
            qty: qty.max(0),
            slot_paise: 0,
            candle_start_ist_nanos: bar.map(|b| b.bucket_open_secs.saturating_mul(NANOS_PER_SEC)),
            candle_end_ist_nanos: bar.map(|b| b.bucket_end_secs.saturating_mul(NANOS_PER_SEC)),
            last_tick_exchange_ist_nanos: bar
                .map(|b| b.last_tick_exchange_secs.saturating_mul(NANOS_PER_SEC)),
            last_tick_receipt_ist_nanos: bar.map(|b| b.last_tick_receipt_ns.max(0)),
            close_by: bar.map(|b| match b.close_by {
                CloseBy::Clock => LiveCandleCloseBy::Clock,
                CloseBy::Tick => LiveCandleCloseBy::Tick,
            }),
            decision_ist_nanos: Some(self.now_ns),
            best_bid_paise: paise(quote.bid),
            best_bid_qty: qty_of(quote.bid_qty),
            best_ask_paise: paise(quote.ask),
            best_ask_qty: qty_of(quote.ask_qty),
            ltp_paise: bar.and_then(|b| rupees_to_paise(b.close).ok()),
            second_volume: None,
            limit_paise: None,
            limit_bound: None,
            queue_wait_us: None,
            feed_lag_us: bar.map(|b| {
                b.last_tick_receipt_ns
                    .saturating_sub(b.last_tick_exchange_secs.saturating_mul(NANOS_PER_SEC))
                    / 1_000
            }),
            api_rtt_us: None,
            order_id: None,
            exchange_order_ist_nanos: None,
            exchange_trade_ist_nanos: None,
            fill_qty: None,
            fill_price_paise: None,
            avg_price_paise: None,
            remainder_qty: None,
            model_price_paise: None,
            worse_than_model: None,
            reason: None,
            charges_paise: None,
        })
    }

    /// Decision row, then a Reject row with `reason`. No paper order exists.
    fn refuse_unplaced(&mut self, intent: &OrderIntent, bar: Option<&ClosedBar>, reason: &str) {
        self.counters.refused = self.counters.refused.saturating_add(1);
        let Some(base) = self.base_event(intent, bar, 0) else {
            return;
        };
        let mut reject = base.clone();
        reject.event = LiveOrderEventKind::Reject;
        reject.seq = 1;
        reject.reason = Some(reason.to_owned());
        self.push_out(ChainOutput::Event(Box::new(base)));
        self.push_out(ChainOutput::Event(Box::new(reject)));
    }

    /// One intent on one released candle: refused with a reason, or held
    /// while its fill is worked out.
    fn decide(&mut self, intent: &OrderIntent, bar: &ClosedBar) {
        if intent.key != bar.key {
            self.refuse_unplaced(intent, Some(bar), "intent_for_other_instrument");
            return;
        }
        // The candle AND its whole fill window `[end, end + max_fill_seconds]`
        // (LF-6b review round 2, finding 3): a stretch tainted and handed out
        // to the held orders BEFORE this order existed (a feed gap starting
        // exactly at the candle's end, a lost 1 s bar of the first fill
        // second) is never re-offered to it, so it is checked here, the same
        // span `PendingBook::on_taint` checks later. O(256), once per order.
        let horizon_end = bar
            .bucket_end_secs
            .saturating_add(i64::from(self.params.max_fill_seconds.max(1)));
        if let Some(t) = self
            .taints
            .taint_of(bar.bucket_open_secs, horizon_end.saturating_add(1))
        {
            self.refuse_unplaced(intent, Some(bar), t.as_str());
            return;
        }
        let Some(ki) = self.key_index(&bar.key) else {
            self.refuse_unplaced(intent, Some(bar), "instrument_not_in_universe");
            return;
        };
        // The instrument's first packet came after this bar opened (a late
        // subscribe, a widen or a resubscribe after the open): the bar holds
        // only the trades from then on (round 2, finding 2). One array read.
        if bar.bucket_open_secs < self.first_seen_secs[ki as usize] {
            self.refuse_unplaced(intent, Some(bar), REASON_FIRST_BAR_PARTIAL);
            return;
        }
        let nf = self.frames_secs.len();
        let fi = bar.frame_index as usize;
        if self.late_bucket[ki as usize * nf + fi] == bar.bucket_open_secs {
            self.counters.late_price_skipped = self.counters.late_price_skipped.saturating_add(1);
            self.refuse_unplaced(intent, Some(bar), REASON_LATE_PRICE);
            return;
        }
        let Some(spec) = self.setup.contracts.get(&bar.key) else {
            self.refuse_unplaced(intent, Some(bar), "no_contract_spec");
            return;
        };
        let (tick_paise, lot_size) = (spec.tick_paise, spec.lot_size);
        let mut qty = u64::from(intent.qty_lots).saturating_mul(u64::from(lot_size));
        if qty == 0 {
            self.refuse_unplaced(intent, Some(bar), "zero_quantity");
            return;
        }
        // An exit is reduce-only against the paper position this chain sent
        // (LF-6b review finding 6); the runtime clamps again against what it
        // actually booked.
        let exit = intent.kind == IntentKind::Exit;
        if exit {
            let held = self.net_shares[ki as usize];
            let can = match intent.side {
                Side::Sell if held > 0 => held.unsigned_abs(),
                Side::Buy if held < 0 => held.unsigned_abs(),
                _ => 0,
            };
            if can == 0 {
                self.refuse_unplaced(intent, Some(bar), "exit_without_position");
                return;
            }
            qty = qty.min(can);
        }
        let Ok(ltp_paise) = rupees_to_paise(bar.close) else {
            self.refuse_unplaced(intent, Some(bar), "bad_last_price");
            return;
        };
        let limit = match marketable_limit(
            intent.side,
            ltp_paise,
            self.params.band_bps,
            tick_paise,
            qty,
            None,
            None,
        ) {
            Ok(l) => l,
            Err(_) => {
                self.refuse_unplaced(intent, Some(bar), "limit_price_refused");
                return;
            }
        };
        let decision_sec = bar.bucket_end_secs.saturating_sub(1);
        let decision_bar = match self.last_second[ki as usize] {
            Some(ls) if ls.bar.sec == decision_sec => ls.bar,
            // Nothing traded in the candle's last second: its worst price is
            // the candle's close.
            _ => SecondBar {
                sec: decision_sec,
                high_paise: ltp_paise,
                low_paise: ltp_paise,
                volume: 0,
            },
        };
        let quote = self.quotes[ki as usize];
        let book = match intent.side {
            Side::Buy if quote.ask > 0.0 && quote.ask_qty > 0 => {
                rupees_to_paise(f32_to_f64_clean(quote.ask))
                    .ok()
                    .map(|p| (p, u64::from(quote.ask_qty)))
            }
            Side::Sell if quote.bid > 0.0 && quote.bid_qty > 0 => {
                rupees_to_paise(f32_to_f64_clean(quote.bid))
                    .ok()
                    .map(|p| (p, u64::from(quote.bid_qty)))
            }
            _ => None,
        };
        let Some(mut base) =
            self.base_event(intent, Some(bar), i64::try_from(qty).unwrap_or(i64::MAX))
        else {
            return;
        };
        base.second_volume = i64::try_from(decision_bar.volume).ok();
        base.limit_paise = Some(limit.price_paise);
        base.limit_bound = Some(match limit.bound {
            LimitBound::Band => LiveLimitBound::Band,
            LimitBound::SlotCap => LiveLimitBound::SlotCap,
            LimitBound::Circuit => LiveLimitBound::Circuit,
        });
        let mut built = base.clone();
        built.event = LiveOrderEventKind::Built;
        built.seq = 1;
        let order = HeldOrder {
            key_index: ki,
            key: bar.key,
            frame_index: bar.frame_index,
            candle_open_secs: bar.bucket_open_secs,
            candle_end_secs: bar.bucket_end_secs,
            side: intent.side,
            qty,
            tick_paise,
            participation_bps: self.params.participation_bps,
            decision_bar,
            book,
            limit_paise: limit.price_paise,
            meta: OrderMeta {
                base: base.clone(),
                lot_size,
                limit_paise: limit.price_paise,
                exit,
            },
        };
        match self.pending.insert(order) {
            Ok(()) => {
                self.counters.held = self.counters.held.saturating_add(1);
                self.push_out(ChainOutput::Event(Box::new(base)));
                self.push_out(ChainOutput::Event(Box::new(built)));
            }
            Err(_) => {
                self.counters.refused = self.counters.refused.saturating_add(1);
                let mut reject = base.clone();
                reject.event = LiveOrderEventKind::Reject;
                reject.seq = 1;
                reject.reason = Some("held_orders_full".to_owned());
                self.push_out(ChainOutput::Event(Box::new(base)));
                self.push_out(ChainOutput::Event(Box::new(reject)));
            }
        }
    }

    /// A held order whose fill is worked out (or whose candle took a late
    /// price): the paper order goes to the runtime, or a Reject row.
    fn on_finished(&mut self, done: Finished<OrderMeta>) {
        match done {
            Finished::LatePrice { order } => {
                self.counters.late_price_skipped =
                    self.counters.late_price_skipped.saturating_add(1);
                let mut reject = order.meta.base;
                reject.ts_ist_nanos = self.now_ns;
                reject.event = LiveOrderEventKind::Reject;
                reject.seq = 2;
                reject.reason = Some(REASON_LATE_PRICE.to_owned());
                let _queued = self.push_finished(ChainOutput::Event(Box::new(reject)));
            }
            Finished::Tainted { order, reason } => {
                self.counters.refused = self.counters.refused.saturating_add(1);
                let mut reject = order.meta.base;
                reject.ts_ist_nanos = self.now_ns;
                reject.event = LiveOrderEventKind::Reject;
                reject.seq = 2;
                reject.remainder_qty = i64::try_from(order.qty).ok();
                reject.reason = Some(reason.as_str().to_owned());
                let _queued = self.push_finished(ChainOutput::Event(Box::new(reject)));
            }
            Finished::Refused { order, refusal } => {
                self.counters.refused = self.counters.refused.saturating_add(1);
                let mut reject = order.meta.base;
                reject.ts_ist_nanos = self.now_ns;
                reject.event = LiveOrderEventKind::Reject;
                reject.seq = 2;
                reject.remainder_qty = i64::try_from(order.qty).ok();
                reject.reason = Some(format!("fill_model_refused:{refusal}"));
                let _queued = self.push_finished(ChainOutput::Event(Box::new(reject)));
            }
            Finished::Filled {
                order,
                summary,
                late_fill_ticks,
                beyond_limit_qty,
            } => {
                let Some(segment) = ExchangeSegment::from_byte(order.key.exchange_segment) else {
                    return;
                };
                // No room for the Execute (only when the outbox was not drained
                // between passes): nothing is counted as held, so a later exit
                // is never sized against a position the runtime never booked
                // (LF-6b review round 3, finding 1).
                if self.outbox.len() >= self.outbox_cap {
                    self.counters.outbox_overflow = self.counters.outbox_overflow.saturating_add(1);
                    self.counters.executions_lost = self.counters.executions_lost.saturating_add(1);
                    return;
                }
                let lot = u64::from(order.meta.lot_size.max(1));
                // The OMS books whole lots; the model's odd shares are part of
                // the cancelled remainder.
                let filled = (summary.filled_qty / lot) * lot;
                let filled_i = i64::try_from(filled).unwrap_or(i64::MAX);
                let avg_paise = if filled > 0 { summary.vwap_paise } else { None };
                let mut base = order.meta.base;
                base.ts_ist_nanos = self.now_ns;
                base.model_price_paise = summary.vwap_paise;
                base.reason = match (late_fill_ticks, beyond_limit_qty) {
                    (0, 0) => None,
                    (l, 0) => Some(format!("late_fill_ticks={l}")),
                    (0, b) => Some(format!("beyond_limit_qty={b}")),
                    (l, b) => Some(format!("late_fill_ticks={l};beyond_limit_qty={b}")),
                };
                let signed = match order.side {
                    Side::Buy => filled_i,
                    Side::Sell => filled_i.saturating_neg(),
                };
                let ki = order.key_index as usize;
                if let Some(n) = self.net_shares.get_mut(ki) {
                    *n = n.saturating_add(signed);
                }
                if filled_i != 0
                    && let Some(t) = self.traded_today.get_mut(ki)
                {
                    *t = true;
                }
                self.counters.sent = self.counters.sent.saturating_add(1);
                let limit_price = paise_to_rupees(order.meta.limit_paise);
                let _queued = self.push_finished(ChainOutput::Cmd(PaperCmd::Execute(Box::new(
                    PaperExecution {
                        base,
                        next_seq: 2,
                        security_id: order.key.security_id,
                        segment,
                        side: order.side,
                        qty: i64::try_from(order.qty).unwrap_or(i64::MAX),
                        lot_size: order.meta.lot_size,
                        limit_price,
                        filled_qty: filled_i,
                        avg_price: avg_paise.map_or(0.0, paise_to_rupees),
                        avg_price_paise: avg_paise,
                        exit: order.meta.exit,
                    },
                ))));
            }
        }
    }

    /// Hands every queued output to `f`, in order, and empties the outbox.
    pub fn drain_outputs(&mut self, mut f: impl FnMut(ChainOutput)) {
        for o in self.outbox.drain(..) {
            f(o);
        }
    }

    /// Outputs waiting.
    #[must_use]
    pub fn outputs_waiting(&self) -> usize {
        self.outbox.len()
    }
}

/// Bars one instrument can have waiting for release at once, over all
/// frames: per frame, the bars that close within the longest wait
/// (`ceil(open_wait / frame)`) plus two (the one being decided and the one
/// a tick just closed). Sizes the release queue. O(frames).
#[must_use]
pub fn bars_waiting_per_key(frames_secs: &[u32], open_wait_ms: u64) -> usize {
    frames_secs
        .iter()
        .map(|&f| {
            let f_ms = u64::from(f.max(1)).saturating_mul(1_000);
            usize::try_from(open_wait_ms.div_ceil(f_ms))
                .unwrap_or(usize::MAX)
                .saturating_add(2)
        })
        .fold(0usize, usize::saturating_add)
}

/// Bucket open (IST epoch seconds) of a second in a frame of `size` seconds,
/// on the candle book's midnight-aligned grid. O(1).
#[must_use]
pub fn bucket_open(secs: i64, size: u32) -> i64 {
    let size = i64::from(size.max(1));
    let day = secs.div_euclid(SECS_PER_DAY);
    let sod = secs.rem_euclid(SECS_PER_DAY);
    day * SECS_PER_DAY + (sod / size) * size
}

/// Paise to rupees for the OMS's `f64` price. O(1).
#[must_use]
pub fn paise_to_rupees(paise: i64) -> f64 {
    // Exact for every price below 2^53 paise; MAX_PRICE_PAISE is 10^15.
    // APPROVED: exact below 2^53 paise; MAX_PRICE_PAISE is 10^15.
    #[allow(clippy::cast_precision_loss)]
    let p = paise as f64;
    p / 100.0
}

#[cfg(test)]
#[path = "chain_tests.rs"]
mod tests;
