//! §28.6 (owner card 2026-11-13, "Yes, instant wake (Recommended)"): the
//! `tv-live-fill` loop gives the same chain outputs whether it waits with the
//! old nap ([`SleepWait`]) or the new park ([`ParkWait`]). The engine here is
//! a TEST DOUBLE with a fixed toy rule; tickvault has no strategy of its own.

use super::*;
use crate::dhan_feed_stack::live_fill_tap::{LiveFillTap, TapVolume};
use chain::ContractSpec;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use tickvault_common::tick_types::{MarketDepthLevel, ParsedTick};
use tickvault_trading::brutex_host::engine::{IntentKind, OrderIntent, Side};

/// IST midnight of a trading day (a multiple of 86,400).
const DAY: i64 = 1_779_321_600;
const NS: i64 = 1_000_000_000;
const OPEN: i64 = DAY + 33_300;
const STEP_NS: i64 = 100_000_000;
const LAST_TICK_SECS: i64 = OPEN + 125;
const END_NS: i64 = (OPEN + 128) * NS;

fn key() -> InstrumentKey {
    InstrumentKey {
        security_id: 2885,
        exchange_segment: 1,
    }
}

/// Asks for 10 lots on every bar after the first. TEST DOUBLE.
#[derive(Default)]
struct EveryBar {
    bars: u32,
    cols: Vec<String>,
}

impl StrategyEngine for EveryBar {
    fn configure(
        &mut self,
        _recipe: &Recipe,
        _params: Option<&ParamSnapshot>,
        _universe: &[InstrumentKey],
    ) -> Result<(), Refusal> {
        self.bars = 0;
        Ok(())
    }
    fn on_tick(&mut self, _tick: &TickInput, _out: &mut IntentSink) {}
    fn on_bar(&mut self, bar: &BarInput, out: &mut IntentSink) {
        self.bars += 1;
        if self.bars > 1 {
            out.push(OrderIntent {
                key: bar.key,
                timeframe: 0,
                kind: IntentKind::Entry,
                side: Side::Buy,
                qty_lots: 10,
                entry_price: 0.0,
                stop_loss: 0.0,
                target: 0.0,
                intent_id: u64::from(self.bars),
                at_ist_secs: 0,
            });
        }
    }
    fn value_columns(&self) -> &[String] {
        &self.cols
    }
    fn bar_values(&self, _key: InstrumentKey, _tf: u8, _out: &mut [f64]) -> bool {
        false
    }
}

fn chain_for_test() -> LiveFillChain<EveryBar> {
    let recipe = serde_json::from_str(
        r#"{"schema":"tickvault-strategy-recipe/1","name":"toy","version":3,"status":"approved","approved_by":"test","approved_at_ist":"2026-11-12T08:00:00+05:30","brutex_commit":"0000000000000000000000000000000000000000","instruments":[{"security_id":2885,"exchange_segment":1}],"timeframes":["1m"],"settings":{},"backtest":{}}"#,
    )
    .expect("recipe json");
    let mut contracts = HashMap::new();
    contracts.insert(
        key(),
        ContractSpec {
            tick_paise: 5,
            lot_size: 1,
            symbol: "RELIANCE".to_owned(),
        },
    );
    let setup = ChainSetup {
        recipe,
        snapshot: None,
        engine_frames_secs: vec![60],
        universe: vec![key()],
        contracts,
        trading_day: DAY / 86_400,
    };
    let params = ChainParams {
        wait_ms: 1_000,
        open_wait_ms: 2_500,
        band_bps: 1_000,
        participation_bps: 2_500,
        max_fill_seconds: 60,
    };
    LiveFillChain::new(EveryBar::default(), setup, params, (OPEN - 600) * NS).expect("chain")
}

fn price(s: i64) -> f32 {
    if s < OPEN + 60 {
        100.0
    } else if s < OPEN + 120 {
        101.0
    } else {
        102.0
    }
}

/// How the park run gates the producer (the old-nap run passes `None`).
#[derive(Clone, Copy)]
struct ParkGate {
    /// The bound a wait that WILL have a tick queued into it is given, long
    /// enough that only the tap's unpark can end it. Waits with no tick keep
    /// the loop's own bound. Test-only: the loop's real bound is the old nap
    /// (1 ms here), which a producer descheduled for more than 1 ms on a busy
    /// runner would miss, ending the park by timeout and making the unpark
    /// count depend on the scheduler.
    tick_bound: Duration,
    /// Extra time the producer waits after it saw the thread parked and
    /// before it queues: stands in for a producer the scheduler took off its
    /// core at the worst moment.
    producer_delay: Duration,
    /// Extra time the producer waits between queuing the tick and the
    /// frame's `flush_wake`.
    flush_delay: Duration,
    /// Stall the consumer takes between declaring itself parked and checking
    /// its rings (`ChainWake::set_check_stall`).
    check_stall: Duration,
}

/// Wraps the wait under test: signals "pass `p` finished, now idle" to the
/// producer thread, runs the REAL wait, then holds until the producer has
/// queued everything pass `p + 1` must see. The hold comes AFTER the real
/// wait, so the real wait still has to end (by the tap's wake, or its bound);
/// it only makes the tick-to-pass assignment deterministic. With a
/// [`ParkGate`], it first waits for the producer to say whether a tick will be
/// queued into this wait, and gives such a wait the gate's long bound.
struct ScriptedWait<'a, W: IdleWait> {
    inner: &'a W,
    passes: &'a AtomicU64,
    idle_seq: &'a AtomicU64,
    idle_pass: &'a AtomicU64,
    ack: &'a AtomicU64,
    decided: &'a AtomicU64,
    will_queue: &'a AtomicBool,
    gate: Option<ParkGate>,
}

impl<W: IdleWait> IdleWait for ScriptedWait<'_, W> {
    fn idle(
        &self,
        tap_rx: &FrameRingReceiver<TapTick>,
        pos_rx: &FrameRingReceiver<PositionReport>,
        bound: Duration,
    ) {
        self.idle_pass
            .store(self.passes.load(Ordering::SeqCst), Ordering::SeqCst);
        let seq = self.idle_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut wait_bound = bound;
        if let Some(gate) = self.gate {
            while self.decided.load(Ordering::SeqCst) < seq {
                std::thread::yield_now();
            }
            if self.will_queue.load(Ordering::SeqCst) {
                wait_bound = gate.tick_bound;
            }
        }
        self.inner.idle(tap_rx, pos_rx, wait_bound);
        while self.ack.load(Ordering::SeqCst) < seq {
            std::thread::yield_now();
        }
    }
}

/// The wall IST nanoseconds of pass `p` (1-based).
fn pass_ns(p: u64) -> i64 {
    OPEN * NS - STEP_NS + i64::try_from(p).expect("fits") * STEP_NS
}

/// Runs the real loop over a scripted day: a pass every 100 ms, one tick a
/// second received 300 ms after its trade. Ticks are queued by a SECOND
/// thread through the real tap (wake armed) while the loop thread waits, so
/// with [`ParkWait`] it is the tap's cross-thread unpark that ends the park.
/// With a [`ParkGate`] the producer queues a tick only once the loop thread
/// is inside `park_timeout` under the gate's long bound (it found the rings
/// empty), so every queued tick meets the idle-to-busy edge whatever the
/// scheduler does.
/// Returns the paper commands and event rows the loop sent, as Debug text,
/// and how many idle waits had a tick queued into them.
fn run_scripted<W: IdleWait>(
    wait: &W,
    wake: &Arc<ChainWake>,
    gate: Option<ParkGate>,
) -> (Vec<String>, Vec<String>, u64) {
    wake.register_current_thread();
    let (tap_tx, tap_rx) = live_fill_tap_ring(4_096);
    let mut tap = LiveFillTap::with_shed_source(tap_tx, || 0).with_wake(Arc::clone(wake));
    let (cmd_tx, mut cmd_rx) = frame_ring::<PaperCmd>(4_096);
    let (events_tx, mut events_rx) = mpsc::channel::<LiveOrderEvent>(4_096);
    let (_pos_tx, pos_rx) = frame_ring::<PositionReport>(64);
    let ends = LoopEnds {
        tap_rx,
        pos_rx,
        cmd_tx,
        events_tx,
    };
    let (passes, idle_seq, idle_pass, ack) = (
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    );
    let done = AtomicBool::new(false);
    let decided = AtomicU64::new(0);
    let will_queue = AtomicBool::new(false);
    let scripted = ScriptedWait {
        inner: wait,
        passes: &passes,
        idle_seq: &idle_seq,
        idle_pass: &idle_pass,
        ack: &ack,
        decided: &decided,
        will_queue: &will_queue,
        gate,
    };
    let mut tick_waits = 0u64;
    std::thread::scope(|scope| {
        let producer = scope.spawn(|| {
            let mut next_secs = OPEN;
            let mut cum: u32 = 0;
            let mut seen = 0u64;
            let mut with_tick = 0u64;
            loop {
                let seq = idle_seq.load(Ordering::SeqCst);
                if seq == seen {
                    if done.load(Ordering::SeqCst) {
                        return with_tick;
                    }
                    std::thread::yield_now();
                    continue;
                }
                seen = seq;
                // Every tick the NEXT pass will have received (at most one).
                let next_pass_ns = pass_ns(idle_pass.load(Ordering::SeqCst) + 1);
                let due = |s: i64| s < LAST_TICK_SECS && s * NS + 300_000_000 <= next_pass_ns;
                will_queue.store(due(next_secs), Ordering::SeqCst);
                // The loop thread cannot start this wait's park before
                // `decided` moves, so a park counted after this baseline is
                // THIS wait's park.
                let parks_before = wake.parks_entered();
                decided.store(seq, Ordering::SeqCst);
                let mut queued = 0u32;
                while due(next_secs) {
                    if let (0, Some(g)) = (queued, gate) {
                        // Queue only once the thread is INSIDE `park_timeout`
                        // (it found the rings empty), never merely flagged
                        // parked: a tick pushed between its flag store and its
                        // ring check is taken without a park, and then whether
                        // the tap unparks it depends on the scheduler (PR C
                        // review, 2026-11-13).
                        let t0 = Instant::now();
                        while wake.parks_entered() == parks_before && t0.elapsed() < g.tick_bound {
                            std::thread::yield_now();
                        }
                        std::thread::sleep(g.producer_delay);
                    }
                    cum += 1_000;
                    let ltp = price(next_secs);
                    let p = ParsedTick {
                        security_id: 2885,
                        exchange_segment_code: 1,
                        last_traded_price: ltp,
                        exchange_timestamp: u32::try_from(next_secs).expect("fits"),
                        received_at_nanos: next_secs * NS + 300_000_000 - IST_OFFSET_NS,
                        volume: cum,
                        ..ParsedTick::default()
                    };
                    let level = MarketDepthLevel {
                        bid_quantity: 100,
                        ask_quantity: 100,
                        bid_orders: 1,
                        ask_orders: 1,
                        bid_price: ltp - 0.05,
                        ask_price: ltp + 0.05,
                    };
                    let vol = TapVolume {
                        delta: 1_000,
                        first_bucket_delta: 1_000,
                        ..TapVolume::default()
                    };
                    assert!(tap.offer(&p, Some(&level), true, false, vol));
                    if let Some(g) = gate {
                        std::thread::sleep(g.flush_delay);
                    }
                    // One tick per frame here: the frame's end wakes it.
                    tap.flush_wake();
                    next_secs += 1;
                    queued += 1;
                }
                assert!(queued <= 1, "the script queues at most one tick per pass");
                with_tick += u64::from(queued);
                ack.store(seq, Ordering::SeqCst);
            }
        });
        let clock = || {
            let p = passes.fetch_add(1, Ordering::SeqCst) + 1;
            let now = pass_ns(p);
            if now > END_NS {
                return None;
            }
            Some((now, now - DAY * NS))
        };
        run_chain_loop(chain_for_test(), ends, &AtomicU64::new(0), &scripted, clock);
        done.store(true, Ordering::SeqCst);
        tick_waits = producer.join().expect("producer");
    });
    let mut cmds = Vec::new();
    while let Some(c) = cmd_rx.try_recv() {
        cmds.push(format!("{c:?}"));
    }
    let mut events = Vec::new();
    while let Ok(e) = events_rx.try_recv() {
        events.push(format!("{e:?}"));
    }
    (cmds, events, tick_waits)
}

/// Old-vs-new: the same tick stream, queued by a second thread into the
/// waits, gives byte-identical paper commands and event rows with the old
/// nap and the new park for the same pass times, the run is not vacuous (the
/// toy double's order is decided and executed), and the park is really
/// ended by the tap's cross-thread unpark: one unpark per queued tick. Fails
/// if `ParkWait::idle` does nothing, spins or sleeps (no unpark is ever
/// issued), or if the wake is lost (the park would sit out the gate's long
/// bound and the unpark count would fall short).
fn assert_old_and_new_agree(producer_delay: Duration) {
    assert_old_and_new_agree_with(ParkGate {
        tick_bound: Duration::from_secs(30),
        producer_delay,
        flush_delay: Duration::ZERO,
        check_stall: Duration::ZERO,
    });
}

fn assert_old_and_new_agree_with(gate: ParkGate) {
    let old_wake = Arc::new(ChainWake::new());
    let old = run_scripted(&SleepWait, &old_wake, None);
    let wake = Arc::new(ChainWake::new());
    wake.set_check_stall(gate.check_stall);
    let new = run_scripted(&ParkWait(Arc::clone(&wake)), &wake, Some(gate));
    assert!(
        old.0.iter().any(|c| c.starts_with("Execute")),
        "non-vacuity: a paper Execute was sent: {:?}",
        old.0
    );
    assert!(!old.1.is_empty(), "non-vacuity: event rows were written");
    assert_eq!(old.0, new.0, "paper commands differ between nap and park");
    assert_eq!(old.1, new.1, "event rows differ between nap and park");
    assert!(new.2 > 50, "ticks were queued into the waits: {}", new.2);
    assert_eq!(old_wake.unparks(), 0, "the old nap is never unparked");
    assert_eq!(
        wake.unparks(),
        new.2,
        "the tap's unpark must end every park a tick was queued into"
    );
}

#[test]
fn test_old_and_new_idle_wait_give_identical_chain_outputs() {
    assert_old_and_new_agree(Duration::ZERO);
}

/// Regression (PR C review): the check used to need the producer to see the
/// thread parked inside the loop's 1 ms bound; a producer the scheduler took
/// off its core for longer (normal on a busy CI runner) let the park time out
/// before the tick was queued, so the unpark count fell short and the test
/// failed at random. A 3 ms stall after the thread parked now still meets the
/// park, because a wait with a tick coming gets the gate's long bound.
#[test]
fn test_regression_a_descheduled_producer_still_meets_the_park() {
    assert_old_and_new_agree(Duration::from_millis(3));
}

/// Regression (PR C review, second pass): the producer used to queue as soon
/// as the thread had stored its parked flag. A thread stalled between that
/// store and its ring check (a cache miss, an interrupt, a preempted vCPU)
/// then saw the tick, did not park, and cleared the flag before the tap read
/// it: no unpark was issued and the exact unpark count fell short at random
/// (or the tap's unpark left a stale token that cost a later wait its 30 s
/// bound). The stalls are forced here: 2 ms before the ring check, 5 ms
/// between the push and the frame's wake. The producer now queues only once
/// the thread is inside `park_timeout`, so every tick still meets a real park.
#[test]
fn test_regression_a_consumer_stalled_before_its_ring_check_still_meets_the_park() {
    assert_old_and_new_agree_with(ParkGate {
        tick_bound: Duration::from_secs(30),
        producer_delay: Duration::ZERO,
        flush_delay: Duration::from_millis(5),
        check_stall: Duration::from_millis(2),
    });
}

/// The production wait returns at once when a tick is already queued (it
/// re-checks after declaring itself parked) and otherwise waits at most the
/// old nap.
#[test]
fn test_park_wait_idle_never_waits_on_queued_work_and_is_bounded() {
    let wake = Arc::new(ChainWake::new());
    wake.register_current_thread();
    let wait = ParkWait(Arc::clone(&wake));
    let (tap_tx, tap_rx) = live_fill_tap_ring(8);
    let (_pos_tx, pos_rx) = frame_ring::<PositionReport>(8);
    let t0 = Instant::now();
    wait.idle(&tap_rx, &pos_rx, Duration::from_millis(5));
    let empty_wait = t0.elapsed();
    assert!(
        empty_wait < Duration::from_secs(2),
        "bounded: {empty_wait:?}"
    );
    let mut tap = LiveFillTap::with_shed_source(tap_tx, || 0);
    let p = ParsedTick {
        security_id: 2885,
        exchange_segment_code: 1,
        ..ParsedTick::default()
    };
    assert!(tap.offer(&p, None, true, false, TapVolume::default()));
    let t1 = Instant::now();
    wait.idle(&tap_rx, &pos_rx, Duration::from_secs(10));
    assert!(
        t1.elapsed() < Duration::from_secs(2),
        "a queued tick skips the park"
    );
    assert_eq!(tap_rx.len(), 1, "the wait took nothing off the ring");
}
