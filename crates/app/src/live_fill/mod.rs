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
//! logged, never waited on), and sleeps 1 ms (5 ms after 50 idle passes) when
//! no tick came. Per-tick and per-pass costs are in [`chain`].

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
use tickvault_core::websocket::frame_ring::FrameRingReceiver;
use tickvault_storage::live_order_events_persistence::{LiveOrderEvent, LiveOrderEventKind};
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::Refusal;
use tickvault_trading::brutex_host::engine::{BarInput, IntentSink, StrategyEngine, TickInput};
use tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS;
use tickvault_trading::brutex_host::recipe::{ParamSnapshot, Recipe};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::dhan_feed_stack::live_fill_tap::{TapLink, TapTick, live_fill_tap_ring};
use chain::{ChainOutput, ChainParams, ChainSetup, LiveFillChain, PaperCmd};
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

/// Ticks one pass takes from the ring at most.
pub const MAX_TICKS_PER_PASS: usize = 4_096;
const IDLE_PASSES_BEFORE_SLOW_SLEEP: u32 = 50;
/// Idle sleep while ticks are recent, and after 50 idle passes, ms.
const IDLE_SLEEP_MS: u64 = 1;
const SLOW_IDLE_SLEEP_MS: u64 = 5;
/// How often the thread publishes its counters, seconds.
const COUNTER_PUBLISH_INTERVAL_SECS: u64 = 1;

/// The order runtime's half of the chain: its commands and the event channel
/// it writes Sent / Ack / Fill / Cancel / Reject rows on.
#[derive(Debug)]
pub struct PaperLink {
    pub cmds: mpsc::Receiver<PaperCmd>,
    pub events: mpsc::Sender<LiveOrderEvent>,
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
    let (cmd_tx, cmd_rx) = mpsc::channel::<PaperCmd>(config.event_channel_capacity);
    *TAP_SLOT.lock().unwrap_or_else(PoisonError::into_inner) = Some(TapLink {
        tx: tap_tx,
        loss: Arc::clone(&tap_loss),
    });
    *PAPER_SLOT.lock().unwrap_or_else(PoisonError::into_inner) = Some(PaperLink {
        cmds: cmd_rx,
        events: events_tx.clone(),
    });
    std::thread::Builder::new()
        .name("tv-live-fill".to_owned())
        .spawn(move || run_live_fill_thread(chain, tap_rx, &tap_loss, cmd_tx, events_tx))?;
    Ok(())
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
pub fn send_output(
    o: ChainOutput,
    cmd_tx: &mpsc::Sender<PaperCmd>,
    events_tx: &mpsc::Sender<LiveOrderEvent>,
    drops: &mut SendDrops,
) {
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
                PaperCmd::Execute(x) => Some(x.base.clone()),
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
                if let Some(mut row) = execution {
                    row.event = LiveOrderEventKind::Reject;
                    row.seq = 2;
                    row.ts_ist_nanos = wall_ist_ns();
                    row.reason = Some("paper_queue_full".to_owned());
                    if events_tx.try_send(row).is_err() {
                        drops.events = drops.events.saturating_add(1);
                        metrics::counter!(EVENTS_DROPPED_COUNTER).increment(1);
                    }
                }
            } else if execution.is_some() {
                metrics::counter!(ORDERS_SENT_COUNTER).increment(1);
            }
        }
    }
}

/// Drop tallies of [`send_output`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendDrops {
    pub cmds: u64,
    pub events: u64,
}

/// The `tv-live-fill` loop (see the module docs). Runs until the process
/// exits or a day roll cannot reconfigure the chain.
fn run_live_fill_thread<E: StrategyEngine>(
    mut chain: LiveFillChain<E>,
    mut tap_rx: FrameRingReceiver<TapTick>,
    tap_loss: &AtomicU64,
    cmd_tx: mpsc::Sender<PaperCmd>,
    events_tx: mpsc::Sender<LiveOrderEvent>,
) {
    // Busy-polled: producers never take the wake path.
    tap_rx.set_busy_polling(true);
    let start = Instant::now();
    let mut idle: u32 = 0;
    let mut drops = SendDrops::default();
    let mut published = chain.counters();
    let mut taints_published = chain.taints_added();
    let mut last_publish = Instant::now();
    info!("live paper fill chain: thread running (paper only)");
    loop {
        let mut n = 0usize;
        while n < MAX_TICKS_PER_PASS {
            match tap_rx.try_recv() {
                Some(t) => {
                    chain.on_tap(&t);
                    n += 1;
                }
                None => break,
            }
        }
        let mono = i64::try_from(start.elapsed().as_nanos()).unwrap_or(i64::MAX);
        // Losses the tap counted, read out of band: a tick dropped behind the
        // ones still queued taints before this pass releases anything.
        chain.note_tap_loss(tap_loss.load(Ordering::Relaxed));
        // A pass that stopped at the per-pass cap left ticks queued: the book
        // clock must not run past the stream it has read.
        let backlogged = n >= MAX_TICKS_PER_PASS;
        if let Err(e) = chain.on_pass_bounded(wall_ist_ns(), mono, backlogged) {
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
        chain.drain_outputs(|o| send_output(o, &cmd_tx, &events_tx, &mut drops));
        if last_publish.elapsed() >= Duration::from_secs(COUNTER_PUBLISH_INTERVAL_SECS) {
            last_publish = Instant::now();
            let c = chain.counters();
            metrics::counter!(ORDERS_REFUSED_COUNTER)
                .increment(c.refused.saturating_sub(published.refused));
            metrics::counter!(LATE_TICKS_COUNTER)
                .increment(c.late_ticks.saturating_sub(published.late_ticks));
            let t = chain.taints_added();
            for (i, r) in TaintReason::ALL.iter().enumerate() {
                metrics::counter!(TAINT_COUNTER, "reason" => r.as_str())
                    .increment(t[i].saturating_sub(taints_published[i]));
            }
            if c.outbox_overflow > published.outbox_overflow {
                warn!(
                    lost = c.outbox_overflow - published.outbox_overflow,
                    "live paper fill chain: outputs over the per-pass outbox were lost"
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
            published = c;
            taints_published = t;
        }
        if n == 0 {
            idle = idle.saturating_add(1);
            let nap = if idle > IDLE_PASSES_BEFORE_SLOW_SLEEP {
                SLOW_IDLE_SLEEP_MS
            } else {
                IDLE_SLEEP_MS
            };
            std::thread::sleep(Duration::from_millis(nap));
        } else {
            idle = 0;
        }
    }
}

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
    async fn test_send_output_counts_full_channels_and_writes_a_reject_row() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<PaperCmd>(1);
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
        assert!(cmd_rx.try_recv().is_ok());
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
