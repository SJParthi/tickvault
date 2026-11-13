//! The live PAPER fill chain (plan item LF-6b; owner rules of 2026-11-12 in
//! the Quote 57 addendum).
//!
//! ```text
//! frame drain ──tap (try_send, never waits)──▶ tv-live-fill thread
//!   live ticks the fold accepted                 strategy candles (LF-1)
//!                                                approved brutex engine
//!                                                held order + worst fill (LF-2)
//!                     ◀──PaperCmd (bounded)──────┘
//! order runtime (dry-run OMS): place PAPER-n, fill, cancel the rest
//! every step ──LiveOrderEvent (bounded)──▶ live_order_events table (LF-5)
//! ```
//!
//! # Safety
//!
//! PAPER ONLY. The chain's single way out is [`chain::PaperCmd`], and the
//! order runtime refuses it unless its OMS is dry-run (whose `dry_run` is
//! hard-wired true). Nothing here calls a broker, builds an HTTP request or
//! flips a live switch.
//!
//! tickvault has no strategy of its own (`brutex-strategy-host-2026-10-29.md`).
//! [`select_engine`] returns `None` until brutex's approved strategy crate is
//! a dependency of this build and its recipe verified, so in production the
//! chain idles: with `[live_fill] enabled = true` only the event writer runs
//! (it drains any leftover spill files) and one coded `info!` says why.
//!
//! # Threads and complexity
//!
//! One OS thread, `tv-live-fill`, owns the chain. It busy-polls the tap's ring
//! with `try_recv` (at most 4,096 ticks per pass), reads the tap's loss total
//! out of band (one relaxed load), then runs one pass (bounded by the newest
//! receipt read when the cap left ticks queued), then
//! sends what the pass produced with `try_send` (a full channel is counted and
//! logged, never waited on). When no tick came it parks until the tap queues
//! one (the tap unparks it on that idle-to-busy edge, §28.6, owner card
//! 2026-11-13) or for at most 1 ms (5 ms after 50 idle passes), the nap it
//! slept before; position reports and clock-driven releases do not wake it,
//! so they wait at most that bound, as before. Per-tick and per-pass costs
//! are in [`chain`].

pub mod chain;
pub mod fill_window;
pub mod taint;
pub mod timing;
// Plan item LF-8: the paper order rows, shared by the order runtime and the
// offline replay; and the replay itself (never on the live path: a source
// scan, `live_fill_replay_isolation_guard.rs`, keeps it there).
pub mod paper_rows;
pub mod replay;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tickvault_common::config::{ApplicationConfig, LiveFillConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_core::websocket::frame_ring::{FrameRingReceiver, FrameRingSender, frame_ring};
use tickvault_storage::live_order_events_persistence::{LiveOrderEvent, LiveOrderEventKind};
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::Refusal;
use tickvault_trading::brutex_host::engine::{BarInput, IntentSink, StrategyEngine, TickInput};
use tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS;
use tickvault_trading::brutex_host::recipe::{ParamSnapshot, Recipe};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::dhan_feed_stack::live_fill_tap::{ChainWake, TapLink, TapTick, live_fill_tap_ring};
use chain::{ChainOutput, ChainParams, ChainSetup, LiveFillChain, PaperCmd, PositionReport};
use taint::TaintReason;

/// Gauge: 1 while a strategy engine runs the chain, 0 otherwise.
pub const ENGINE_CONFIGURED_GAUGE: &str = "tv_live_fill_engine_configured";
/// Counter: paper commands the runtime channel had no room for.
pub const CMD_DROPPED_COUNTER: &str = "tv_live_fill_cmd_dropped_total";
/// Counter: event rows the event channel had no room for.
pub const EVENTS_DROPPED_COUNTER: &str = "tv_live_fill_events_dropped_total";
/// Counter: refused or skipped orders.
pub const ORDERS_REFUSED_COUNTER: &str = "tv_live_fill_orders_refused_total";
/// Counter: paper orders sent to the runtime.
pub const ORDERS_SENT_COUNTER: &str = "tv_live_fill_orders_sent_total";
/// Counter: ticks the candle book counted late.
pub const LATE_TICKS_COUNTER: &str = "tv_live_fill_late_ticks_total";
/// Counter: untrusted stretches, by reason.
pub const TAINT_COUNTER: &str = "tv_live_fill_taint_total";

/// The Reject reason when the order runtime's channel had no room for a
/// priced order: the chain had worked its fill out (the daily re-pricing
/// reads its share of the seconds as known, LF-9).
pub const PAPER_QUEUE_FULL_REASON: &str = "paper_queue_full";

/// The key of the count of late 1 s prints the chain saw inside a priced
/// order's fill window, on that order's rows (`late_fill_ticks=N` in the
/// reason). The daily re-pricing reads an order carrying it as one whose
/// share of the seconds is not known (LF-9 review, third pass): the late
/// print can have changed the stored volume of a second the chain had
/// already settled.
pub const LATE_FILL_TICKS_KEY: &str = "late_fill_ticks";

/// Ticks one pass takes from the ring at most.
pub const MAX_TICKS_PER_PASS: usize = 4_096;
const IDLE_PASSES_BEFORE_SLOW_SLEEP: u32 = 50;
/// Upper bound of the idle park while ticks are recent, and after 50 idle
/// passes, ms (the old nap, §28.6).
const IDLE_SLEEP_MS: u64 = 1;
const SLOW_IDLE_SLEEP_MS: u64 = 5;
/// How often the thread publishes its counters, seconds.
const COUNTER_PUBLISH_INTERVAL_SECS: u64 = 1;

/// The order runtime's half of the chain: its commands and the event channel
/// it writes Sent / Ack / Fill / Cancel / Reject rows on.
#[derive(Debug)]
pub struct PaperLink {
    /// A pre-allocated ring (LF-9 finding 5): a tokio channel grew 32-slot
    /// blocks while the runtime lagged, an allocation per pass on this
    /// thread.
    pub cmds: FrameRingReceiver<PaperCmd>,
    pub events: mpsc::Sender<LiveOrderEvent>,
    /// What the runtime booked for each Execute, back to the chain (LF-9
    /// finding 2): a pre-allocated ring the chain thread busy-polls.
    pub positions: FrameRingSender<PositionReport>,
}

static TAP_SLOT: Mutex<Option<TapLink>> = Mutex::new(None);
static PAPER_SLOT: Mutex<Option<PaperLink>> = Mutex::new(None);

/// The tap's sender and shared loss total, once, for the frame drain. `None`
/// unless the boot built the chain (never today). O(1), cold.
#[must_use]
pub fn take_tap_sender() -> Option<TapLink> {
    TAP_SLOT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

/// The runtime's link, once, for the order runtime spawn. `None` unless the
/// boot built the chain (never today). O(1), cold.
#[must_use]
pub fn take_paper_link() -> Option<PaperLink> {
    PAPER_SLOT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

/// There is no approved strategy engine in this build. Uninhabited: it can
/// never be constructed, so [`select_engine`] can only answer `None`. When
/// brutex's strategy crate becomes a pinned dependency, its adapter replaces
/// this type here and nowhere else.
#[derive(Debug)]
pub enum NoApprovedEngine {}

impl StrategyEngine for NoApprovedEngine {
    fn configure(
        &mut self,
        _recipe: &Recipe,
        _params: Option<&ParamSnapshot>,
        _universe: &[InstrumentKey],
    ) -> Result<(), Refusal> {
        match *self {}
    }
    fn on_tick(&mut self, _tick: &TickInput, _out: &mut IntentSink) {
        match *self {}
    }
    fn on_bar(&mut self, _bar: &BarInput, _out: &mut IntentSink) {
        match *self {}
    }
    fn value_columns(&self) -> &[String] {
        match *self {}
    }
    fn bar_values(&self, _key: InstrumentKey, _timeframe: u8, _out: &mut [f64]) -> bool {
        match *self {}
    }
}

/// The approved strategy engine and its day setup, or `None`.
///
/// Rule `brutex-strategy-host-2026-10-29.md`: tickvault runs only brutex's own
/// strategy crate, pinned to the approved commit, after its recipe, gates and
/// replay check pass, swapped only before 09:00 IST. That crate does not exist
/// yet, so this answers `None` and the chain idles. O(1).
#[must_use]
pub fn select_engine() -> Option<(NoApprovedEngine, ChainSetup)> {
    None
}

/// Registers every live-fill metric at 0 so an absent series never reads as
/// healthy. Cold.
pub fn register_metrics() {
    metrics::gauge!(ENGINE_CONFIGURED_GAUGE).set(0.0);
    metrics::counter!(CMD_DROPPED_COUNTER).increment(0);
    metrics::counter!(EVENTS_DROPPED_COUNTER).increment(0);
    metrics::counter!(ORDERS_REFUSED_COUNTER).increment(0);
    metrics::counter!(ORDERS_SENT_COUNTER).increment(0);
    metrics::counter!(LATE_TICKS_COUNTER).increment(0);
    for r in TaintReason::ALL {
        metrics::counter!(TAINT_COUNTER, "reason" => r.as_str()).increment(0);
    }
}

/// What the boot decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootOutcome {
    /// `[live_fill] enabled = false`.
    Off,
    /// Enabled, but `[order_runtime]` is off: nothing could take the orders.
    RefusedNoOrderRuntime,
    /// Enabled; the event writer runs; no approved engine, so no chain.
    IdleNoEngine,
    /// The chain runs.
    Running,
}

/// The boot decision, pure. O(1).
#[must_use]
pub fn boot_decision(
    live_fill_enabled: bool,
    order_runtime_enabled: bool,
    engine: bool,
) -> BootOutcome {
    if !live_fill_enabled {
        BootOutcome::Off
    } else if !order_runtime_enabled {
        BootOutcome::RefusedNoOrderRuntime
    } else if engine {
        BootOutcome::Running
    } else {
        BootOutcome::IdleNoEngine
    }
}

/// Boots the chain, once, from `dhan_rest_stack::spawn_dhan_rest_stack`, which
/// `main` calls before it spawns the feed stack (needs a tokio runtime for the
/// event writer).
/// Off by default; see [`BootOutcome`]. Cold, once per process.
// TEST-EXEMPT: boot glue — every decision is the pure `boot_decision` (tested); the chain start is `start_chain` (tested with test-double engines); the writer has its own source-scan ratchet.
pub fn boot(config: &ApplicationConfig) -> BootOutcome {
    register_metrics();
    let engine = select_engine();
    let outcome = boot_decision(
        config.live_fill.enabled,
        config.order_runtime.enabled,
        engine.is_some(),
    );
    match outcome {
        BootOutcome::Off => {
            info!("live paper fill chain: off ([live_fill] enabled = false)");
        }
        BootOutcome::RefusedNoOrderRuntime => {
            error!(
                code = ErrorCode::OrderReady01GateRefused.code_str(),
                source = "live_fill_requires_order_runtime",
                "live paper fill chain: [live_fill] is enabled but [order_runtime] is not, so \
                 nothing could take its paper orders; the chain stays OFF this process"
            );
        }
        BootOutcome::IdleNoEngine | BootOutcome::Running => {
            let cap = config.live_fill.event_channel_capacity;
            let (events_tx, events_rx) = mpsc::channel::<LiveOrderEvent>(cap);
            let _writer = crate::live_order_events_boot::spawn_live_order_events_writer(
                config.questdb.clone(),
                events_rx,
            );
            match engine {
                None => info!(
                    "live fill chain idle: no approved strategy engine (no brutex strategy \
                     crate in this build); the event writer runs, no tap, no thread"
                ),
                Some((engine, setup)) => {
                    let wall = wall_ist_ns();
                    match start_chain(engine, setup, &config.live_fill, events_tx, wall) {
                        Ok(()) => metrics::gauge!(ENGINE_CONFIGURED_GAUGE).set(1.0),
                        Err(e) => error!(
                            code = ErrorCode::OrderReady01GateRefused.code_str(),
                            source = "live_fill_chain_refused",
                            error = %e,
                            "live paper fill chain: could not start; it stays OFF this process"
                        ),
                    }
                }
            }
        }
    }
    record_boot_outcome(outcome);
    outcome
}

/// The boot outcome of this process, for the daily calibration report (plan
/// item LF-7): 0 = not booted yet, else `1 + BootOutcome as u8`. One store,
/// once per process.
static LIVE_FILL_BOOT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Records `outcome` for [`last_boot_outcome`]. One relaxed store.
fn record_boot_outcome(outcome: BootOutcome) {
    LIVE_FILL_BOOT.store(
        boot_outcome_code(outcome),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The stored code of an outcome (never 0). O(1).
const fn boot_outcome_code(outcome: BootOutcome) -> u8 {
    match outcome {
        BootOutcome::Off => 1,
        BootOutcome::RefusedNoOrderRuntime => 2,
        BootOutcome::IdleNoEngine => 3,
        BootOutcome::Running => 4,
    }
}

/// The outcome of a stored code; `None` for 0 (not booted) or an unknown code. O(1).
const fn boot_outcome_of_code(code: u8) -> Option<BootOutcome> {
    match code {
        1 => Some(BootOutcome::Off),
        2 => Some(BootOutcome::RefusedNoOrderRuntime),
        3 => Some(BootOutcome::IdleNoEngine),
        4 => Some(BootOutcome::Running),
        _ => None,
    }
}

/// What [`boot`] decided in this process, or `None` before it ran. Read by
/// the daily calibration report to say why a day had no orders. One relaxed
/// load.
#[must_use]
pub fn last_boot_outcome() -> Option<BootOutcome> {
    boot_outcome_of_code(LIVE_FILL_BOOT.load(std::sync::atomic::Ordering::Relaxed))
}

/// Builds the chain, puts the tap sender and the runtime link in their slots,
/// and spawns `tv-live-fill`. Cold.
///
/// # Errors
/// The chain refused its setup, or the thread could not be spawned.
pub fn start_chain<E: StrategyEngine + Send + 'static>(
    engine: E,
    setup: ChainSetup,
    config: &LiveFillConfig,
    events_tx: mpsc::Sender<LiveOrderEvent>,
    now_ist_ns: i64,
) -> anyhow::Result<()> {
    let params = ChainParams::from(config);
    let chain = LiveFillChain::new(engine, setup, params, now_ist_ns)?;
    let (tap_tx, tap_rx) = live_fill_tap_ring(config.tap_capacity);
    let tap_loss = Arc::new(AtomicU64::new(0));
    let wake = Arc::new(ChainWake::new());
    let (cmd_tx, cmd_rx) = frame_ring::<PaperCmd>(config.event_channel_capacity.max(1));
    // Every Execute in flight gets one report: the command channel, the
    // runtime's exit retry queue and one pass of finished orders bound them.
    let (pos_tx, pos_rx) =
        frame_ring::<PositionReport>(position_ring_capacity(config.event_channel_capacity));
    pos_rx.set_busy_polling(true);
    *TAP_SLOT.lock().unwrap_or_else(PoisonError::into_inner) = Some(TapLink {
        tx: tap_tx,
        loss: Arc::clone(&tap_loss),
        wake: Arc::clone(&wake),
    });
    *PAPER_SLOT.lock().unwrap_or_else(PoisonError::into_inner) = Some(PaperLink {
        cmds: cmd_rx,
        events: events_tx.clone(),
        positions: pos_tx,
    });
    std::thread::Builder::new()
        .name("tv-live-fill".to_owned())
        .spawn(move || {
            let ends = LoopEnds {
                tap_rx,
                pos_rx,
                cmd_tx,
                events_tx,
            };
            run_live_fill_thread(chain, ends, &tap_loss, wake);
        })?;
    Ok(())
}

/// Slots of the position-report ring (LF-9 finding 2): every Execute that can
/// be in flight at once (the command channel, the runtime's exit retry queue,
/// one pass of finished orders), so a report is never dropped while the chain
/// thread keeps up. O(1).
#[must_use]
pub fn position_ring_capacity(cmd_capacity: usize) -> usize {
    cmd_capacity
        .saturating_add(fill_window::MAX_PENDING_ORDERS * 4)
        .saturating_add(fill_window::MAX_PENDING_ORDERS)
}

/// The wall clock, IST epoch nanoseconds. O(1).
#[must_use]
pub fn wall_ist_ns() -> i64 {
    let utc = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    utc.saturating_add(IST_OFFSET_NS)
}

/// Hands one chain output to its channel. Never waits: a full channel is
/// counted and logged (at powers of two, so a stall cannot flood the log).
/// O(1) apart from the row itself.
/// Returns the report of an Execute the command channel refused (`expected`
/// shares, nothing booked), so the chain can take its shares back (LF-9
/// finding 2).
pub fn send_output(
    o: ChainOutput,
    cmd_tx: &FrameRingSender<PaperCmd>,
    events_tx: &mpsc::Sender<LiveOrderEvent>,
    drops: &mut SendDrops,
) -> Option<PositionReport> {
    match o {
        ChainOutput::Event(e) => {
            if events_tx.try_send(*e).is_err() {
                drops.events = drops.events.saturating_add(1);
                metrics::counter!(EVENTS_DROPPED_COUNTER).increment(1);
                if drops.events.is_power_of_two() {
                    error!(
                        code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                        stage = "live_fill_event_channel",
                        dropped = drops.events,
                        "AUDIT-06: live_order_events row dropped — the event channel was full \
                         or closed (paper chain only; no order is affected)"
                    );
                }
            }
        }
        ChainOutput::Cmd(cmd) => {
            let execution = match &cmd {
                PaperCmd::Execute(x) => Some((x.base.clone(), paper_rows::position_report(x, 0))),
                PaperCmd::Mark { .. } => None,
            };
            if cmd_tx.try_send(cmd).is_err() {
                drops.cmds = drops.cmds.saturating_add(1);
                metrics::counter!(CMD_DROPPED_COUNTER).increment(1);
                if drops.cmds.is_power_of_two() {
                    error!(
                        code = ErrorCode::OrderReady01GateRefused.code_str(),
                        source = "live_fill_cmd_queue_full",
                        dropped = drops.cmds,
                        "live paper fill chain: a paper command was dropped — the order \
                         runtime's channel was full or closed"
                    );
                }
                if let Some((mut row, unsent)) = execution {
                    row.event = LiveOrderEventKind::Reject;
                    row.seq = 2;
                    row.ts_ist_nanos = wall_ist_ns();
                    row.reason = Some(PAPER_QUEUE_FULL_REASON.to_owned());
                    if events_tx.try_send(row).is_err() {
                        drops.events = drops.events.saturating_add(1);
                        metrics::counter!(EVENTS_DROPPED_COUNTER).increment(1);
                    }
                    return Some(unsent);
                }
            } else if execution.is_some() {
                metrics::counter!(ORDERS_SENT_COUNTER).increment(1);
            }
        }
    }
    None
}

/// The thread's once-a-second counter publish (LF-9 finding 5). Every metric
/// handle is resolved once, at construction: a `counter!` with a non-literal
/// label value builds its label list on the heap at EVERY call (the
/// allocating arm of the `metrics` macro), so the taint counters used to
/// allocate four times a second on this thread. A publish now allocates
/// nothing unless it logs a loss line (rare). O(`TaintReason::COUNT`).
pub struct CounterPublisher {
    refused: metrics::Counter,
    late: metrics::Counter,
    taints: [metrics::Counter; TaintReason::COUNT],
    published: chain::ChainCounters,
    taints_published: [u64; TaintReason::COUNT],
    /// `tv_live_fill_engine_configured`, resolved once (LF-9 review, finding
    /// 5).
    configured: metrics::Gauge,
    /// Whether the chain was inert (`awaiting_day_setup`) at the last pass.
    inert: bool,
}

impl CounterPublisher {
    /// Resolves the handles and takes `c` and `t` as published. Cold.
    #[must_use]
    pub fn new(c: chain::ChainCounters, t: [u64; TaintReason::COUNT]) -> Self {
        Self {
            refused: metrics::counter!(ORDERS_REFUSED_COUNTER),
            late: metrics::counter!(LATE_TICKS_COUNTER),
            taints: std::array::from_fn(
                |i| metrics::counter!(TAINT_COUNTER, "reason" => TaintReason::ALL[i].as_str()),
            ),
            published: c,
            taints_published: t,
            configured: metrics::gauge!(ENGINE_CONFIGURED_GAUGE),
            inert: false,
        }
    }

    /// The chain's inert state after a pass (LF-9 review, finding 5): when a
    /// day roll leaves it on yesterday's setup it decides nothing, so the
    /// engine-configured gauge goes to 0 at that edge (it read 1 all day
    /// before, a false OK). Local `/metrics` only, no page or alarm. O(1):
    /// one compare, one gauge store on the edge only.
    pub fn note_inert(&mut self, inert: bool) {
        if inert != self.inert {
            self.inert = inert;
            self.configured.set(if inert { 0.0 } else { 1.0 });
        }
    }

    /// Adds what moved since the last publish, and logs once per moved loss
    /// or stale-setup count. O(`TaintReason::COUNT`).
    pub fn publish(&mut self, c: chain::ChainCounters, t: [u64; TaintReason::COUNT]) {
        let published = self.published;
        self.refused
            .increment(c.refused.saturating_sub(published.refused));
        self.late
            .increment(c.late_ticks.saturating_sub(published.late_ticks));
        for (i, counter) in self.taints.iter().enumerate() {
            counter.increment(t[i].saturating_sub(self.taints_published[i]));
        }
        if c.outbox_overflow > published.outbox_overflow {
            warn!(
                lost = c.outbox_overflow - published.outbox_overflow,
                "live paper fill chain: outputs over the per-pass outbox were lost"
            );
        }
        if c.day_rolls_without_setup > published.day_rolls_without_setup {
            // Once per day roll (LF-9 finding 12), never per bar.
            warn!(
                code = ErrorCode::OrderReady01GateRefused.code_str(),
                source = "live_fill_setup_stale",
                "live paper fill chain: a new trading day began while the loaded setup \
                 is the previous day's; the chain decides nothing until the process \
                 restarts and loads a setup checked for today (paper only, no order is \
                 placed)"
            );
        }
        if c.executions_lost > published.executions_lost {
            error!(
                code = ErrorCode::OrderReady01GateRefused.code_str(),
                source = "live_fill_execution_not_queued",
                lost = c.executions_lost - published.executions_lost,
                "live paper fill chain: a filled paper order could not be queued (the \
                 outbox was full); the chain did not count it as held (paper only)"
            );
        }
        self.published = c;
        self.taints_published = t;
    }
}

/// Drop tallies of [`send_output`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendDrops {
    pub cmds: u64,
    pub events: u64,
}

/// Hands every position report the runtime sent since the last pass to the
/// chain (LF-9 finding 2). The thread's own step, split out so a test drives
/// the exact code the thread runs (LF-9 review). O(reports), no allocation.
fn apply_position_reports<E: StrategyEngine>(
    chain: &mut LiveFillChain<E>,
    pos_rx: &mut FrameRingReceiver<PositionReport>,
) {
    while let Some(r) = pos_rx.try_recv() {
        chain.on_position_report(&r);
    }
}

/// Sends the chain's outputs and takes back the shares of every Execute the
/// command channel refused (`forget_unsent`, LF-9 finding 2). `unsent` is the
/// thread's pre-sized scratch, left empty. The thread's own step, split out
/// for the same reason as [`apply_position_reports`]. O(outputs), no
/// allocation within the scratch's capacity.
fn forward_outputs<E: StrategyEngine>(
    chain: &mut LiveFillChain<E>,
    cmd_tx: &FrameRingSender<PaperCmd>,
    events_tx: &mpsc::Sender<LiveOrderEvent>,
    drops: &mut SendDrops,
    unsent: &mut Vec<PositionReport>,
) {
    chain.drain_outputs(|o| {
        if let Some(r) = send_output(o, cmd_tx, events_tx, drops)
            && unsent.len() < unsent.capacity()
        {
            unsent.push(r);
        }
    });
    for r in unsent.drain(..) {
        chain.forget_unsent(r.security_id, r.segment, r.expected_shares);
    }
}

/// One pass of the thread and its inert-state report (LF-9 review, finding
/// 5): the thread's own step, split out so a test drives the exact code it
/// runs. O(1) more than the pass.
///
/// # Errors
/// As [`LiveFillChain::on_pass_bounded`].
fn pass_step<E: StrategyEngine>(
    chain: &mut LiveFillChain<E>,
    publisher: &mut CounterPublisher,
    wall_ist_ns: i64,
    mono_ns: i64,
    backlogged: bool,
) -> Result<(), chain::ChainError> {
    chain.on_pass_bounded(wall_ist_ns, mono_ns, backlogged)?;
    publisher.note_inert(chain.awaiting_day_setup());
    Ok(())
}

/// How the thread waits after a pass that took no tick (§28.6). The bound is
/// the old nap: a pass driven by the clock or a position report (neither
/// wakes the thread) is never later than before.
trait IdleWait {
    fn idle(
        &self,
        tap_rx: &FrameRingReceiver<TapTick>,
        pos_rx: &FrameRingReceiver<PositionReport>,
        bound: Duration,
    );
}

/// The production wait (owner card 2026-11-13, "Yes, instant wake
/// (Recommended)"): park until the tap queues a tick, or the bound. O(1).
struct ParkWait(Arc<ChainWake>);

impl IdleWait for ParkWait {
    fn idle(
        &self,
        tap_rx: &FrameRingReceiver<TapTick>,
        pos_rx: &FrameRingReceiver<PositionReport>,
        bound: Duration,
    ) {
        self.0
            .park_unless(|| tap_rx.is_empty() && pos_rx.is_empty(), bound);
    }
}

/// The wait before 2026-11-13: a plain nap a tick cannot end. TEST ONLY, the
/// old side of the old-vs-new identical-output test.
#[cfg(test)]
struct SleepWait;

#[cfg(test)]
impl IdleWait for SleepWait {
    fn idle(
        &self,
        _tap_rx: &FrameRingReceiver<TapTick>,
        _pos_rx: &FrameRingReceiver<PositionReport>,
        bound: Duration,
    ) {
        std::thread::sleep(bound);
    }
}

/// The thread's channel ends.
struct LoopEnds {
    tap_rx: FrameRingReceiver<TapTick>,
    pos_rx: FrameRingReceiver<PositionReport>,
    cmd_tx: FrameRingSender<PaperCmd>,
    events_tx: mpsc::Sender<LiveOrderEvent>,
}

/// The `tv-live-fill` thread (see the module docs). Runs until the process
/// exits or a day roll cannot reconfigure the chain.
fn run_live_fill_thread<E: StrategyEngine>(
    chain: LiveFillChain<E>,
    ends: LoopEnds,
    tap_loss: &AtomicU64,
    wake: Arc<ChainWake>,
) {
    wake.register_current_thread();
    let start = Instant::now();
    info!("live paper fill chain: thread running (paper only)");
    run_chain_loop(chain, ends, tap_loss, &ParkWait(wake), || {
        let mono = i64::try_from(start.elapsed().as_nanos()).unwrap_or(i64::MAX);
        Some((wall_ist_ns(), mono))
    });
}

/// The thread's loop. `clock` gives each pass its (wall IST, monotonic)
/// nanoseconds; `None` ends the loop (tests only: production never returns
/// `None`). Returns when `clock` ends it or a day roll is refused.
fn run_chain_loop<E: StrategyEngine, W: IdleWait>(
    mut chain: LiveFillChain<E>,
    mut ends: LoopEnds,
    tap_loss: &AtomicU64,
    wait: &W,
    mut clock: impl FnMut() -> Option<(i64, i64)>,
) {
    // Busy-polled: producers never take the ring's own wake path; the tap
    // wakes this thread through `ChainWake` instead (§28.6).
    ends.tap_rx.set_busy_polling(true);
    ends.pos_rx.set_busy_polling(true);
    // Executes the command channel refused in one pass: at most one pass of
    // finished orders, sized once (LF-9 finding 2).
    let mut unsent: Vec<PositionReport> = Vec::with_capacity(fill_window::MAX_PENDING_ORDERS * 4);
    let mut idle: u32 = 0;
    let mut drops = SendDrops::default();
    let mut publisher = CounterPublisher::new(chain.counters(), chain.taints_added());
    let mut last_publish = Instant::now();
    loop {
        let mut n = 0usize;
        while n < MAX_TICKS_PER_PASS {
            match ends.tap_rx.try_recv() {
                Some(t) => {
                    chain.on_tap(&t);
                    n += 1;
                }
                None => break,
            }
        }
        // What the runtime booked since the last pass (LF-9 finding 2).
        apply_position_reports(&mut chain, &mut ends.pos_rx);
        let Some((wall, mono)) = clock() else {
            return;
        };
        // Losses the tap counted, read out of band: a tick dropped behind the
        // ones still queued taints before this pass releases anything.
        chain.note_tap_loss(tap_loss.load(Ordering::Relaxed));
        // A pass that stopped at the per-pass cap left ticks queued: the book
        // clock must not run past the stream it has read.
        let backlogged = n >= MAX_TICKS_PER_PASS;
        if let Err(e) = pass_step(&mut chain, &mut publisher, wall, mono, backlogged) {
            error!(
                code = ErrorCode::OrderReady01GateRefused.code_str(),
                source = "live_fill_day_roll_refused",
                error = %e,
                "live paper fill chain: the new trading day could not be configured; the \
                 chain stops this process"
            );
            metrics::gauge!(ENGINE_CONFIGURED_GAUGE).set(0.0);
            return;
        }
        forward_outputs(
            &mut chain,
            &ends.cmd_tx,
            &ends.events_tx,
            &mut drops,
            &mut unsent,
        );
        if last_publish.elapsed() >= Duration::from_secs(COUNTER_PUBLISH_INTERVAL_SECS) {
            last_publish = Instant::now();
            publisher.publish(chain.counters(), chain.taints_added());
        }
        if n == 0 {
            idle = idle.saturating_add(1);
            let nap = if idle > IDLE_PASSES_BEFORE_SLOW_SLEEP {
                SLOW_IDLE_SLEEP_MS
            } else {
                IDLE_SLEEP_MS
            };
            wait.idle(&ends.tap_rx, &ends.pos_rx, Duration::from_millis(nap));
        } else {
            idle = 0;
        }
    }
}

#[cfg(test)]
#[path = "wake_tests.rs"]
mod wake_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_engine_returns_none_until_an_approved_crate_exists() {
        assert!(select_engine().is_none());
    }

    #[test]
    fn test_boot_decision_needs_both_switches_and_an_engine() {
        assert_eq!(boot_decision(false, true, true), BootOutcome::Off);
        assert_eq!(boot_decision(false, false, false), BootOutcome::Off);
        assert_eq!(
            boot_decision(true, false, true),
            BootOutcome::RefusedNoOrderRuntime
        );
        assert_eq!(boot_decision(true, true, false), BootOutcome::IdleNoEngine);
        assert_eq!(boot_decision(true, true, true), BootOutcome::Running);
    }

    /// LF-7: the boot outcome round-trips through its stored code; 0 and an
    /// unknown code read as not booted.
    #[test]
    fn test_last_boot_outcome_round_trips_every_outcome() {
        for o in [
            BootOutcome::Off,
            BootOutcome::RefusedNoOrderRuntime,
            BootOutcome::IdleNoEngine,
            BootOutcome::Running,
        ] {
            assert_ne!(boot_outcome_code(o), 0);
            assert_eq!(boot_outcome_of_code(boot_outcome_code(o)), Some(o));
        }
        assert_eq!(boot_outcome_of_code(0), None);
        assert_eq!(boot_outcome_of_code(9), None);
        record_boot_outcome(BootOutcome::IdleNoEngine);
        assert_eq!(last_boot_outcome(), Some(BootOutcome::IdleNoEngine));
    }

    #[test]
    fn test_register_metrics_and_wall_ist_ns() {
        register_metrics();
        let w = wall_ist_ns();
        assert!(w > IST_OFFSET_NS);
    }

    #[tokio::test]
    async fn test_send_output_counts_full_channels_writes_a_reject_row_and_position_ring_capacity()
    {
        let (cmd_tx, mut cmd_rx) = frame_ring::<PaperCmd>(1);
        let (ev_tx, mut ev_rx) = mpsc::channel::<LiveOrderEvent>(4);
        let mut drops = SendDrops::default();
        let mark = || {
            ChainOutput::Cmd(PaperCmd::Mark {
                security_id: 1,
                segment: tickvault_common::types::ExchangeSegment::NseEquity,
                price: 10.0,
            })
        };
        send_output(mark(), &cmd_tx, &ev_tx, &mut drops);
        send_output(mark(), &cmd_tx, &ev_tx, &mut drops);
        assert_eq!(drops.cmds, 1);
        assert!(cmd_rx.try_recv().is_some());
        assert!(ev_rx.try_recv().is_err(), "a dropped mark writes no row");
        drop(ev_rx);
        let ev = test_support::event();
        send_output(
            ChainOutput::Event(Box::new(ev)),
            &cmd_tx,
            &ev_tx,
            &mut drops,
        );
        assert_eq!(drops.events, 1);
        // LF-9 finding 2: an Execute the command channel refuses comes back
        // as the shares the chain must take off its position.
        let (cmd_tx, _cmd_rx) = frame_ring::<PaperCmd>(1);
        let (ev_tx, _ev_rx) = mpsc::channel::<LiveOrderEvent>(4);
        assert!(send_output(mark(), &cmd_tx, &ev_tx, &mut drops).is_none());
        let x = chain::PaperExecution {
            base: test_support::event(),
            next_seq: 2,
            security_id: 9,
            segment: tickvault_common::types::ExchangeSegment::NseEquity,
            side: tickvault_trading::brutex_host::engine::Side::Sell,
            qty: 20,
            lot_size: 1,
            limit_price: 10.0,
            filled_qty: 20,
            avg_price: 10.0,
            avg_price_paise: Some(1_000),
            exit: true,
        };
        let unsent = send_output(
            ChainOutput::Cmd(PaperCmd::Execute(Box::new(x))),
            &cmd_tx,
            &ev_tx,
            &mut drops,
        )
        .expect("refused");
        assert_eq!((unsent.security_id, unsent.expected_shares), (9, -20));
        assert!(position_ring_capacity(8) > fill_window::MAX_PENDING_ORDERS * 4);
    }

    /// One test owns the process-wide slots (tests run in parallel), so the
    /// empty-slot check and the start share it.
    #[tokio::test]
    async fn test_start_chain_spawns_the_thread_and_take_tap_sender_take_paper_link() {
        use tickvault_trading::brutex_host::engine::ReferenceDouble;
        assert!(take_tap_sender().is_none(), "empty without a chain");
        assert!(take_paper_link().is_none(), "empty without a chain");
        let config = LiveFillConfig {
            enabled: true,
            ..LiveFillConfig::default()
        };
        let (ev_tx, _ev_rx) = mpsc::channel::<LiveOrderEvent>(64);
        let setup = test_support::setup();
        start_chain(ReferenceDouble::new(), setup, &config, ev_tx, wall_ist_ns()).expect("start");
        let tap = take_tap_sender().expect("tap slot filled");
        let link = take_paper_link().expect("paper slot filled");
        assert!(take_tap_sender().is_none(), "taken once");
        // §28.6 wiring: the tap's wake is the one the chain thread parks on.
        // Only the spawned `tv-live-fill` thread ever declares itself parked,
        // so a fresh `ChainWake` handed to the thread (not the link's) leaves
        // this one unparked forever and fails here.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !tap.wake.is_parked() {
            assert!(
                Instant::now() < deadline,
                "the chain thread never parked on the tap link's wake: start_chain \
                 must hand the SAME Arc<ChainWake> to TapLink and to the thread"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            Arc::strong_count(&tap.wake),
            2,
            "the link and the running thread share one wake"
        );
        drop(tap);
        drop(link);
    }
}

/// Fixtures shared by the live-fill tests (this module and the order
/// runtime's arm). TEST ONLY.
#[cfg(test)]
pub(crate) mod test_support {
    use super::chain::{ChainSetup, ContractSpec};
    use std::collections::HashMap;
    use tickvault_common::types::ExchangeSegment;
    use tickvault_storage::live_order_events_persistence::{
        LiveOrderEvent, LiveOrderEventKind, LiveOrderMode, LiveOrderSide,
    };
    use tickvault_trading::brutex_host::InstrumentKey;

    pub fn setup() -> ChainSetup {
        let key = InstrumentKey {
            security_id: 2885,
            exchange_segment: 1,
        };
        let recipe = serde_json::from_str(
            r#"{"schema":"tickvault-strategy-recipe/1","name":"toy","version":1,"status":"approved","approved_by":"test","approved_at_ist":"2026-11-12T08:00:00+05:30","brutex_commit":"0000000000000000000000000000000000000000","timeframes":["1m"],"settings":{},"backtest":{}}"#,
        )
        .expect("recipe");
        let mut contracts = HashMap::new();
        contracts.insert(
            key,
            ContractSpec {
                tick_paise: 5,
                lot_size: 1,
                symbol: "X".to_owned(),
            },
        );
        ChainSetup {
            recipe,
            snapshot: None,
            engine_frames_secs: vec![60],
            universe: vec![key],
            contracts,
            trading_day: super::wall_ist_ns()
                .div_euclid(1_000_000_000)
                .div_euclid(86_400),
        }
    }

    pub fn event() -> LiveOrderEvent {
        LiveOrderEvent {
            ts_ist_nanos: 1,
            event: LiveOrderEventKind::Decision,
            signal_id: 1,
            seq: 0,
            feed: "dhan",
            mode: LiveOrderMode::Paper,
            recipe: "toy".to_owned(),
            recipe_version: 1,
            security_id: 1,
            segment: ExchangeSegment::NseEquity,
            symbol: "X".to_owned(),
            side: LiveOrderSide::Buy,
            qty: 1,
            slot_paise: 0,
            candle_start_ist_nanos: None,
            candle_end_ist_nanos: None,
            last_tick_exchange_ist_nanos: None,
            last_tick_receipt_ist_nanos: None,
            close_by: None,
            decision_ist_nanos: None,
            best_bid_paise: None,
            best_bid_qty: None,
            best_ask_paise: None,
            best_ask_qty: None,
            ltp_paise: None,
            second_volume: None,
            limit_paise: None,
            limit_bound: None,
            queue_wait_us: None,
            feed_lag_us: None,
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
            tick_paise: None,
            lot_size: None,
            participation_bps: None,
            max_fill_seconds: None,
        }
    }
}
