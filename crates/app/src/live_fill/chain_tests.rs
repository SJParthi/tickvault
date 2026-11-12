//! Tests for the paper fill chain. The engines here are TEST DOUBLES with
//! fixed toy rules; tickvault has no strategy of its own.

use super::*;
use tickvault_trading::brutex_host::Refusal;
use tickvault_trading::brutex_host::engine::{BarInput, IntentKind, ReferenceDouble};

/// IST midnight of a trading day (a multiple of 86,400).
const DAY: i64 = 1_779_321_600;
const NS: i64 = 1_000_000_000;
const OPEN: i64 = DAY + 33_300;

fn key() -> InstrumentKey {
    InstrumentKey {
        security_id: 2885,
        exchange_segment: 1,
    }
}

fn recipe() -> Recipe {
    serde_json::from_str(
        r#"{"schema":"tickvault-strategy-recipe/1","name":"toy","version":3,"status":"approved","approved_by":"test","approved_at_ist":"2026-11-12T08:00:00+05:30","brutex_commit":"0000000000000000000000000000000000000000","instruments":[{"security_id":2885,"exchange_segment":1}],"timeframes":["1m"],"settings":{},"backtest":{}}"#,
    )
    .expect("recipe json")
}

fn setup(with_spec: bool) -> ChainSetup {
    let mut contracts = HashMap::new();
    if with_spec {
        contracts.insert(
            key(),
            ContractSpec {
                tick_paise: 5,
                lot_size: 1,
                symbol: "RELIANCE".to_owned(),
            },
        );
    }
    ChainSetup {
        recipe: recipe(),
        snapshot: None,
        engine_frames_secs: vec![60],
        universe: vec![key()],
        contracts,
    }
}

fn params() -> ChainParams {
    ChainParams {
        wait_ms: 1_000,
        open_wait_ms: 2_500,
        band_bps: 1_000,
        participation_bps: 2_500,
        max_fill_seconds: 60,
    }
}

fn tap(secs: i64, receipt_ns: i64, ltp: f32, cum: u32, drop_seq: u32) -> TapTick {
    TapTick {
        security_id: 2885,
        receipt_ist_ns: receipt_ns,
        ltp,
        exchange_secs: u32::try_from(secs).expect("fits"),
        cum_volume: cum,
        best_bid: ltp - 0.05,
        best_ask: ltp + 0.05,
        best_bid_qty: 100,
        best_ask_qty: 100,
        drop_seq,
        segment: 1,
        new_trade: true,
    }
}

/// Records every bar handed to it and, when `emit`, asks for one share on
/// every bar after the first. TEST DOUBLE.
#[derive(Debug, Default)]
struct Recorder {
    bars: Vec<BarInput>,
    emit: bool,
    /// Ask on every bar, the first included.
    emit_all: bool,
    side: Option<Side>,
    tick_intent: bool,
    cols: Vec<String>,
}

impl StrategyEngine for Recorder {
    fn configure(
        &mut self,
        _recipe: &Recipe,
        _params: Option<&ParamSnapshot>,
        _universe: &[InstrumentKey],
    ) -> Result<(), Refusal> {
        self.bars.clear();
        Ok(())
    }
    fn on_tick(&mut self, tick: &TickInput, out: &mut IntentSink) {
        if self.tick_intent {
            out.push(intent(tick.key, Side::Buy));
        }
    }
    fn on_bar(&mut self, bar: &BarInput, out: &mut IntentSink) {
        self.bars.push(*bar);
        if (self.emit && self.bars.len() > 1) || self.emit_all {
            out.push(intent(bar.key, self.side.unwrap_or(Side::Buy)));
        }
    }
    fn value_columns(&self) -> &[String] {
        &self.cols
    }
    fn bar_values(&self, _key: InstrumentKey, _tf: u8, _out: &mut [f64]) -> bool {
        false
    }
}

fn intent(key: InstrumentKey, side: Side) -> OrderIntent {
    OrderIntent {
        key,
        timeframe: 0,
        kind: IntentKind::Entry,
        side,
        qty_lots: 10,
        entry_price: 0.0,
        stop_loss: 0.0,
        target: 0.0,
        intent_id: 1,
        at_ist_secs: 0,
    }
}

fn chain(engine: Recorder, with_spec: bool) -> LiveFillChain<Recorder> {
    LiveFillChain::new(engine, setup(with_spec), params(), (OPEN - 600) * NS).expect("chain")
}

/// One pass with a monotonic clock that agrees with the wall (no step).
fn pass<E: StrategyEngine>(c: &mut LiveFillChain<E>, now: i64) {
    c.on_pass(now, now - DAY * NS).expect("pass");
}

/// A synthetic feed: one tick per second, received 300 ms after it, the
/// cumulative volume rising by `vol` each tick.
struct Feed {
    cum: u32,
    drop_seq: u32,
    out: Vec<ChainOutput>,
}

impl Feed {
    fn new() -> Self {
        Self {
            cum: 0,
            drop_seq: 0,
            out: Vec::new(),
        }
    }

    /// Ticks for every second of `[from, to)` at `price(s)`, and a pass every
    /// 100 ms from `from` to `to + tail` seconds.
    fn drive<E: StrategyEngine>(
        &mut self,
        c: &mut LiveFillChain<E>,
        (from, to, tail): (i64, i64, i64),
        price: impl Fn(i64) -> f32,
        vol: u32,
    ) {
        let mut s = from;
        let mut now = from * NS;
        let end = (to + tail) * NS;
        while now <= end {
            while s < to && s * NS + 300_000_000 <= now {
                self.cum += vol;
                c.on_tap(&tap(
                    s,
                    s * NS + 300_000_000,
                    price(s),
                    self.cum,
                    self.drop_seq,
                ));
                s += 1;
            }
            pass(c, now);
            c.drain_outputs(|o| self.out.push(o));
            now += 100_000_000;
        }
    }

    fn passes<E: StrategyEngine>(&mut self, c: &mut LiveFillChain<E>, from_ns: i64, to_ns: i64) {
        let mut now = from_ns;
        while now < to_ns {
            pass(c, now);
            c.drain_outputs(|o| self.out.push(o));
            now += 100_000_000;
        }
    }
}

/// The 09:15 minute at 100, the 09:16 minute at 101.
fn rising(s: i64) -> f32 {
    if s < OPEN + 60 { 100.0 } else { 101.0 }
}

fn events(out: &[ChainOutput]) -> Vec<(LiveOrderEventKind, i32, Option<String>)> {
    out.iter()
        .filter_map(|o| match o {
            ChainOutput::Event(e) => Some((e.event, e.seq, e.reason.clone())),
            ChainOutput::Cmd(_) => None,
        })
        .collect()
}

fn executions(out: &[ChainOutput]) -> Vec<PaperExecution> {
    out.iter()
        .filter_map(|o| match o {
            ChainOutput::Cmd(PaperCmd::Execute(x)) => Some((**x).clone()),
            _ => None,
        })
        .collect()
}

fn emitting() -> Recorder {
    Recorder {
        emit: true,
        ..Recorder::default()
    }
}

#[test]
fn test_bars_waiting_per_key_covers_the_longest_wait() {
    // 1 s frame: 3 bars close inside 2.5 s, plus 2; 60 s frame: 1 plus 2.
    assert_eq!(bars_waiting_per_key(&[1, 60], 2_500), 8);
    assert_eq!(bars_waiting_per_key(&[1], 1_000), 3);
    assert_eq!(bars_waiting_per_key(&[], 1_000), 0);
    assert_eq!(
        bars_waiting_per_key(&[0], 1),
        3,
        "a zero size counts as 1 s"
    );
}

#[test]
fn test_bucket_open_and_paise_to_rupees() {
    assert_eq!(bucket_open(OPEN + 59, 60), OPEN);
    assert_eq!(bucket_open(OPEN + 60, 60), OPEN + 60);
    assert_eq!(bucket_open(OPEN + 4, 7), DAY + 33_299);
    assert!((paise_to_rupees(10_025) - 100.25).abs() < 1e-12);
}

#[test]
fn test_live_fill_chain_new_maps_engine_frames_and_outputs_waiting() {
    let mut s = setup(true);
    s.engine_frames_secs = vec![1, 60];
    let c = LiveFillChain::new(Recorder::default(), s, params(), OPEN * NS).expect("chain");
    assert_eq!(c.frames_secs, vec![1, 60]);
    assert_eq!(c.engine_tf, vec![Some(0), Some(1)]);
    let c = chain(Recorder::default(), true);
    assert_eq!(c.frames_secs, vec![1, 60]);
    assert_eq!(c.engine_tf, vec![None, Some(0)]);
    assert_eq!(c.day(), DAY / 86_400);
    assert_eq!(c.held_orders(), 0);
    assert_eq!(c.outputs_waiting(), 0);
    let mut bad = setup(true);
    bad.universe.clear();
    assert!(matches!(
        LiveFillChain::new(ReferenceDouble::new(), bad, params(), OPEN * NS),
        Err(ChainError::Engine(_))
    ));
}

#[test]
fn test_opening_minute_waits_open_wait_and_later_minutes_wait_one_second() {
    let mut c = chain(Recorder::default(), true);
    let mut f = Feed::new();
    // Ticks through 09:16:00, passes until 09:16:02.4: the opening minute
    // (end 09:16:00) must NOT have reached the engine.
    f.drive(&mut c, (OPEN, OPEN + 61, 1), |_| 100.0, 10);
    f.passes(&mut c, (OPEN + 62) * NS, (OPEN + 62) * NS + 500_000_000);
    assert!(
        c.engine().bars.is_empty(),
        "opening minute held until 09:16:02.5"
    );
    pass(&mut c, (OPEN + 62) * NS + 500_000_000);
    assert_eq!(c.engine().bars.len(), 1);
    assert_eq!(c.engine().bars[0].open_ist_secs, OPEN);
    // The second minute (end 09:17:00) is released at 09:17:01.0, not 09:17:02.5.
    f.drive(&mut c, (OPEN + 63, OPEN + 120, 0), |_| 100.0, 10);
    pass(&mut c, (OPEN + 121) * NS - 1);
    assert_eq!(c.engine().bars.len(), 1);
    pass(&mut c, (OPEN + 121) * NS);
    assert_eq!(c.engine().bars.len(), 2);
    assert_eq!(c.counters().late_ticks, 0);
}

#[test]
fn test_on_tap_on_pass_drain_outputs_full_chain_to_one_paper_execution() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    // Two minutes of ticks, 1,000 shares a second (cap 250 a second).
    f.drive(&mut c, (OPEN, OPEN + 120, 0), rising, 1_000);
    assert!(
        events(&f.out).is_empty(),
        "the 09:16 bar is released at 09:17:01"
    );
    // Keep trading. The intent fires at the 09:16 bar's release (09:17:01).
    // 10 shares wanted; the first fill second (09:17:00) covers them, so the
    // fill is final once 09:17:01 is final (09:17:03).
    f.drive(&mut c, (OPEN + 120, OPEN + 125, 2), |_| 102.0, 1_000);
    let ev = events(&f.out);
    assert_eq!(
        ev.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(),
        vec![
            (LiveOrderEventKind::Decision, 0),
            (LiveOrderEventKind::Built, 1)
        ]
    );
    let x = executions(&f.out);
    assert_eq!(x.len(), 1, "exactly one paper execution");
    let x = &x[0];
    assert_eq!(x.next_seq, 2);
    assert_eq!(x.qty, 10);
    assert_eq!(x.filled_qty, 10);
    assert!(x.avg_price > 0.0);
    assert_eq!(x.avg_price_paise, x.base.model_price_paise);
    assert_eq!(x.base.recipe, "toy");
    assert_eq!(x.base.recipe_version, 3);
    assert_eq!(x.base.symbol, "RELIANCE");
    assert_eq!(x.base.mode, LiveOrderMode::Paper);
    assert!(x.base.limit_paise.is_some());
    assert_eq!(c.held_orders(), 0);
    // Marks follow for the new position, every second.
    let before = c.counters().marks;
    f.drive(&mut c, (OPEN + 125, OPEN + 130, 2), |_| 102.0, 10);
    assert!(c.counters().marks > before);
}

#[test]
fn test_partial_fill_inside_the_window_sends_the_rounded_fill() {
    let mut s = setup(true);
    s.contracts.get_mut(&key()).expect("spec").lot_size = 50;
    let mut c = LiveFillChain::new(emitting(), s, params(), (OPEN - 600) * NS).expect("chain");
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 0), rising, 8);
    // 10 lots x 50 = 500 shares wanted; 8 shares a second traded, 2 of them
    // ours: 120 over the 60-second window, rounded down to 2 lots = 100. The
    // window ends at 09:18:00 (first fill second 09:17:00 + 60), final at
    // 09:18:02.
    f.drive(&mut c, (OPEN + 120, OPEN + 200, 4), |_| 101.0, 8);
    let x = executions(&f.out);
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].qty, 500);
    assert_eq!(x[0].filled_qty, 100);
    assert!(x[0].avg_price > 0.0);
    assert_eq!(x[0].lot_size, 50);
    // No volume at all after the decision: nothing filled, all cancelled.
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 0), rising, 8);
    f.drive(&mut c, (OPEN + 120, OPEN + 200, 4), |_| 101.0, 0);
    let x = executions(&f.out);
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].filled_qty, 0);
    assert_eq!(x[0].avg_price, 0.0);
}

#[test]
fn test_late_price_before_release_skips_the_order() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 0), rising, 100);
    // 09:17:00.3: the instrument's 09:17:00 tick closes the 09:16 bar; a
    // straggler stamped 09:16:59 arrives at 09:17:00.5, before the release.
    f.cum += 100;
    c.on_tap(&tap(
        OPEN + 120,
        (OPEN + 120) * NS + 300_000_000,
        101.0,
        f.cum,
        0,
    ));
    f.cum += 1;
    c.on_tap(&tap(
        OPEN + 119,
        (OPEN + 120) * NS + 500_000_000,
        101.0,
        f.cum,
        0,
    ));
    f.passes(&mut c, (OPEN + 120) * NS + 600_000_000, (OPEN + 122) * NS);
    assert_eq!(c.counters().late_ticks, 1);
    let ev = events(&f.out);
    assert!(
        ev.contains(&(
            LiveOrderEventKind::Reject,
            1,
            Some(REASON_LATE_PRICE.to_owned())
        )),
        "{ev:?}"
    );
    assert_eq!(c.held_orders(), 0);
    assert!(executions(&f.out).is_empty());
}

#[test]
fn test_late_price_after_the_decision_cancels_the_held_order() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 1), rising, 1);
    assert_eq!(c.held_orders(), 1);
    // A price for 09:16:30 arrives at 09:17:01.5: after the decision.
    f.cum += 1;
    c.on_tap(&tap(
        OPEN + 90,
        (OPEN + 121) * NS + 500_000_000,
        101.0,
        f.cum,
        0,
    ));
    f.passes(
        &mut c,
        (OPEN + 121) * NS + 600_000_000,
        (OPEN + 121) * NS + 700_000_000,
    );
    assert_eq!(c.held_orders(), 0);
    let ev = events(&f.out);
    assert!(ev.contains(&(
        LiveOrderEventKind::Reject,
        2,
        Some(REASON_LATE_PRICE.to_owned())
    )));
    assert!(
        executions(&f.out).is_empty(),
        "no paper order is ever created"
    );
    assert!(c.counters().late_price_skipped >= 1);
}

#[test]
fn test_dropped_ticks_refuse_orders_on_the_candles_they_touched() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 90, 0), |_| 100.0, 10);
    // The tap dropped 3 ticks just before 09:16:30.
    f.drop_seq = 3;
    f.drive(&mut c, (OPEN + 90, OPEN + 120, 2), |_| 101.0, 10);
    let ev = events(&f.out);
    assert!(
        ev.contains(&(
            LiveOrderEventKind::Reject,
            1,
            Some("tick_dropped".to_owned())
        )),
        "{ev:?}"
    );
    assert_eq!(c.taints_added()[0], 1);
}

#[test]
fn test_missing_contract_spec_and_tick_intents_are_refused() {
    let mut c = chain(emitting(), false);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 2), rising, 10);
    assert!(events(&f.out).contains(&(
        LiveOrderEventKind::Reject,
        1,
        Some("no_contract_spec".to_owned())
    )));
    let engine = Recorder {
        tick_intent: true,
        ..Recorder::default()
    };
    let mut c = chain(engine, true);
    c.on_tap(&tap(OPEN, OPEN * NS + 1, 100.0, 1, 0));
    let mut out = Vec::new();
    c.drain_outputs(|o| out.push(o));
    assert!(events(&out).contains(&(
        LiveOrderEventKind::Reject,
        1,
        Some("tick_intent_unsupported".to_owned())
    )));
}

#[test]
fn test_day_roll_reconfigures_and_drops_held_orders() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 1), rising, 1);
    assert_eq!(c.held_orders(), 1);
    let next_day = (DAY + 86_400 + 30_000) * NS;
    pass(&mut c, next_day);
    assert_eq!(c.held_orders(), 0);
    assert_eq!(c.day(), DAY / 86_400 + 1);
    assert!(
        c.engine().bars.is_empty(),
        "the engine was configured again"
    );
    assert_eq!(c.new_day(next_day).expect("again"), 0);
}

#[test]
fn test_clock_step_is_counted_in_taints_added() {
    let mut c = chain(Recorder::default(), true);
    c.on_pass(OPEN * NS, 0).expect("pass");
    c.on_pass((OPEN + 5) * NS, NS).expect("pass");
    assert_eq!(c.counters().clock_steps, 1);
    assert_eq!(c.taints_added()[2], 1);
}

/// Finding 2 (LF-6b review): a repeat quote after the bar's release carries
/// the old trade time; it is not a late price and does not skip the order.
#[test]
fn test_repeat_quote_after_release_is_not_a_late_price() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 1), rising, 1);
    assert_eq!(c.held_orders(), 1);
    // 09:17:01.2: the book moved, no trade; the packet still carries the
    // 09:16:59 trade time, price and cumulative.
    let mut repeat = tap(OPEN + 119, (OPEN + 121) * NS + 200_000_000, 101.0, f.cum, 0);
    repeat.new_trade = false;
    c.on_tap(&repeat);
    assert_eq!(c.counters().late_ticks, 0, "a repeat quote is never late");
    f.drive(&mut c, (OPEN + 120, OPEN + 125, 2), |_| 101.0, 1_000);
    let ev = events(&f.out);
    assert!(
        !ev.iter().any(|e| e.2.as_deref() == Some(REASON_LATE_PRICE)),
        "{ev:?}"
    );
    assert_eq!(
        executions(&f.out).len(),
        1,
        "the order went to the paper OMS"
    );
}

/// Finding 2: a first-sight packet seeds the volume baseline without opening
/// a bar, so the first trade's own volume reaches its bar.
#[test]
fn test_first_sight_packet_seeds_the_baseline_and_opens_no_bar() {
    let mut c = chain(Recorder::default(), true);
    let mut first = tap(OPEN - 3_600, OPEN * NS - 1, 99.0, 5_000, 0);
    first.new_trade = false;
    c.on_tap(&first);
    c.on_tap(&tap(OPEN, OPEN * NS + 300_000_000, 100.0, 5_400, 0));
    let mut f = Feed::new();
    f.passes(&mut c, OPEN * NS + 400_000_000, (OPEN + 63) * NS);
    assert_eq!(c.engine().bars.len(), 1);
    assert_eq!(
        c.engine().bars[0].open_ist_secs,
        OPEN,
        "no bar at the old trade time"
    );
    assert_eq!(
        c.engine().bars[0].volume,
        400,
        "the first trade's own volume"
    );
}

/// Finding 3 (LF-6b review): an order on an opening candle is decided on the
/// candle's real last second (not a close-only stand-in) and sees its first
/// fill second, so a fill the first second covers is final at 09:16:03.
#[test]
fn test_opening_candle_decides_on_its_last_second_and_keeps_its_first_fill_second() {
    let engine = Recorder {
        emit_all: true,
        ..Recorder::default()
    };
    let mut c = chain(engine, true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 63, 0), |_| 100.0, 1_000);
    let decision = f
        .out
        .iter()
        .find_map(|o| match o {
            ChainOutput::Event(e) if e.event == LiveOrderEventKind::Decision => Some(e.clone()),
            _ => None,
        })
        .expect("a decision on the opening candle");
    assert_eq!(decision.candle_start_ist_nanos, Some(OPEN * NS));
    assert_eq!(
        decision.second_volume,
        Some(1_000),
        "the 09:15:59 second's own volume, not a close-only stand-in"
    );
    assert_eq!(
        executions(&f.out).len(),
        1,
        "triggered on the first fill second 09:16:00, final at 09:16:03"
    );
}

/// Finding 4 (LF-6b review): a drop learned AFTER the decision, while the
/// order is held, refuses it before any paper order exists.
#[test]
fn test_drop_learned_after_the_decision_refuses_the_held_order() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 1), rising, 1);
    assert_eq!(c.held_orders(), 1);
    f.drop_seq = 1;
    f.drive(&mut c, (OPEN + 120, OPEN + 123, 0), |_| 101.0, 1);
    assert_eq!(c.held_orders(), 0);
    let ev = events(&f.out);
    assert!(
        ev.contains(&(
            LiveOrderEventKind::Reject,
            2,
            Some("tick_dropped".to_owned())
        )),
        "{ev:?}"
    );
    assert!(
        executions(&f.out).is_empty(),
        "no paper order is ever created"
    );
}

/// Finding 4: a feed gap during the fill window refuses the held order while
/// the gap lasts (the wall clock is checked every pass).
#[test]
fn test_feed_gap_during_the_fill_window_refuses_the_held_order() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 1), rising, 1);
    assert_eq!(c.held_orders(), 1);
    // No tick at all for 3 s.
    f.passes(&mut c, (OPEN + 121) * NS, (OPEN + 123) * NS);
    assert_eq!(c.held_orders(), 0);
    assert!(events(&f.out).contains(&(LiveOrderEventKind::Reject, 2, Some("feed_gap".to_owned()))));
    assert!(executions(&f.out).is_empty());
}

/// Finding 9 (LF-6b review): a chain started mid-session refuses orders on
/// the partial first bars; the next full bar is clean.
#[test]
fn test_mid_session_start_taints_the_first_bars_chain_start() {
    let start = OPEN + 600;
    let mut c =
        LiveFillChain::new(emitting(), setup(true), params(), start * NS + 1).expect("chain");
    assert_eq!(c.taints_added()[3], 1);
    let mut f = Feed::new();
    f.drive(&mut c, (start, start + 182, 1), |_| 100.0, 1_000);
    let ev = events(&f.out);
    assert!(
        ev.contains(&(
            LiveOrderEventKind::Reject,
            1,
            Some("chain_start".to_owned())
        )),
        "{ev:?}"
    );
    assert!(
        ev.iter().any(|e| e.0 == LiveOrderEventKind::Built),
        "the first full bar after the start decides normally: {ev:?}"
    );
    // Before the session: no taint.
    let pre = chain(Recorder::default(), true);
    assert_eq!(pre.taints_added()[3], 0);
}

proptest::proptest! {
    /// With the lagged clock, a tick stamped inside a second but received up
    /// to 900 ms after it ends (and no later tick of that instrument before
    /// it) still folds into its bar: never counted late.
    #[test]
    fn proptest_in_wait_ticks_fold_not_late(
        lags in proptest::collection::vec(0i64..900_000_000, 1..120),
    ) {
        let mut c = chain(Recorder::default(), true);
        let mut cum = 0u32;
        let mut now = OPEN * NS;
        for (i, lag) in lags.iter().enumerate() {
            let s = OPEN + 5 + i as i64 * 2;
            let receipt = (s + 1) * NS + lag;
            // Passes up to the receipt, every 50 ms.
            while now < receipt {
                pass(&mut c, now);
                now += 50_000_000;
            }
            cum += 1;
            c.on_tap(&tap(s, receipt, 100.0, cum, 0));
        }
        proptest::prop_assert_eq!(c.counters().late_ticks, 0);
    }
}
