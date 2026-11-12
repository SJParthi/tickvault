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
        older_trade: false,
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

/// An engine that buys on its second bar and closes on its fourth, with a
/// fixed exit size. TEST DOUBLE (toy rule, no strategy of tickvault's own).
#[derive(Debug, Default)]
struct EntryThenExit {
    bars: usize,
    entry_lots: u32,
    exit_lots: u32,
    cols: Vec<String>,
}

impl StrategyEngine for EntryThenExit {
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
        let mut i = intent(bar.key, Side::Buy);
        if self.bars == 2 && self.entry_lots > 0 {
            i.qty_lots = self.entry_lots;
            out.push(i);
        }
        if self.bars == 4 {
            i.kind = IntentKind::Exit;
            i.side = Side::Sell;
            i.qty_lots = self.exit_lots;
            out.push(i);
        }
    }
    fn value_columns(&self) -> &[String] {
        &self.cols
    }
    fn bar_values(&self, _key: InstrumentKey, _tf: u8, _out: &mut [f64]) -> bool {
        false
    }
}

fn marks(out: &[ChainOutput]) -> usize {
    out.iter()
        .filter(|o| matches!(o, ChainOutput::Cmd(PaperCmd::Mark { .. })))
        .count()
}

/// LF-6b review finding 6: the entry filled only 2 of its 10 lots; the
/// strategy's full-size exit is cut to the 2 lots held, never a reversal. An
/// exit with nothing held is refused.
#[test]
fn test_exit_after_a_partial_entry_is_cut_to_the_position_held() {
    let mut s = setup(true);
    s.contracts.get_mut(&key()).expect("spec").lot_size = 50;
    let engine = EntryThenExit {
        entry_lots: 10,
        exit_lots: 10,
        ..EntryThenExit::default()
    };
    let mut c = LiveFillChain::new(engine, s, params(), (OPEN - 600) * NS).expect("chain");
    let mut f = Feed::new();
    // 8 shares a second, 2 of them ours: the entry fills 100 of 500 shares.
    f.drive(&mut c, (OPEN, OPEN + 300, 4), rising, 8);
    let x = executions(&f.out);
    assert_eq!(x.len(), 2, "{x:?}");
    assert!(!x[0].exit);
    assert_eq!(x[0].filled_qty, 100);
    assert!(x[1].exit);
    assert_eq!(x[1].side, Side::Sell);
    assert_eq!(x[1].qty, 100, "the exit is cut to the 2 lots held");
    assert!(x[1].filled_qty <= 100);
    // No entry at all: the exit is refused before any order exists.
    let engine = EntryThenExit {
        entry_lots: 0,
        exit_lots: 10,
        ..EntryThenExit::default()
    };
    let mut c =
        LiveFillChain::new(engine, setup(true), params(), (OPEN - 600) * NS).expect("chain");
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 250, 4), rising, 1_000);
    assert!(executions(&f.out).is_empty());
    assert!(
        events(&f.out)
            .iter()
            .any(|e| e.2.as_deref() == Some("exit_without_position")),
        "{:?}",
        events(&f.out)
    );
}

/// LF-6b review finding 7: once a paper order filled, the instrument's 1 s
/// closes keep going to the runtime even after the chain's own count reads
/// flat, because the runtime may have refused the closing order and still
/// hold the position (it ignores a mark for a flat one).
#[test]
fn test_marks_continue_after_the_chain_count_reads_flat() {
    let engine = EntryThenExit {
        entry_lots: 10,
        exit_lots: 10,
        ..EntryThenExit::default()
    };
    let mut c =
        LiveFillChain::new(engine, setup(true), params(), (OPEN - 600) * NS).expect("chain");
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 250, 4), rising, 1_000);
    let x = executions(&f.out);
    assert_eq!(x.len(), 2, "entry and exit both executed: {x:?}");
    assert_eq!(c.net_shares[0], 0, "the chain's own count reads flat");
    f.out.clear();
    f.drive(&mut c, (OPEN + 250, OPEN + 260, 2), |_| 101.0, 1_000);
    assert!(
        marks(&f.out) >= 5,
        "the 1 s closes are still marked: {}",
        marks(&f.out)
    );
}

/// LF-6b review finding 3: a pass whose drain stopped at the per-pass cap
/// runs the book clock on the newest receipt read, not the wall, so a bar
/// whose ticks may still be queued is neither closed nor decided.
#[test]
fn test_backlogged_pass_never_runs_the_clock_past_the_stream_on_pass_bounded() {
    let mut c = chain(Recorder::default(), true);
    let mut f = Feed::new();
    // Ticks through 09:16:59 (received at 09:16:59.3), passes to 09:17:00.
    f.drive(&mut c, (OPEN, OPEN + 120, 0), |_| 100.0, 10);
    let before = c.engine().bars.len();
    // The wall is at 09:17:05, but the thread is 6 s behind its ring.
    let wall = (OPEN + 125) * NS;
    c.on_pass_bounded(wall, wall - DAY * NS, true)
        .expect("pass");
    assert_eq!(
        c.engine().bars.len(),
        before,
        "the 09:16 bar is not released on seconds still queued"
    );
    // Caught up: the same wall releases it.
    c.on_pass_bounded(wall, wall - DAY * NS, false)
        .expect("pass");
    assert_eq!(c.engine().bars.len(), before + 1);
}

/// LF-6b review finding 3: a loss the tap counted behind ticks still queued
/// is read out of band and refuses the candle before it is decided.
#[test]
fn test_tap_loss_read_out_of_band_refuses_the_next_decision_note_tap_loss() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    // Up to 09:17:00.9: the 09:16 bar (the one the engine orders on) is
    // released at 09:17:01.
    f.drive(&mut c, (OPEN, OPEN + 120, 0), rising, 1_000);
    f.passes(
        &mut c,
        (OPEN + 120) * NS + 100_000_000,
        (OPEN + 121) * NS - 1,
    );
    assert!(events(&f.out).is_empty());
    c.note_tap_loss(1);
    f.passes(&mut c, (OPEN + 121) * NS, (OPEN + 121) * NS + 200_000_000);
    let ev = events(&f.out);
    assert!(
        ev.contains(&(
            LiveOrderEventKind::Reject,
            1,
            Some("tick_dropped".to_owned())
        )),
        "{ev:?}"
    );
    assert_eq!(c.held_orders(), 0);
    // The queued tick that carries the same count taints nothing more.
    let added = c.taints_added();
    c.on_tap(&tap(
        OPEN + 121,
        (OPEN + 121) * NS + 100_000_000,
        101.0,
        f.cum + 1,
        1,
    ));
    assert_eq!(c.taints_added(), added);
}

/// LF-6b review finding 4: a real but OLDER trade (a backup copy accepted
/// late, a stale cumulative regression) whose price belongs to the decision
/// candle, arriving after that candle closed, skips the order as a late
/// price; one inside a still-open bar widens it without moving its close.
#[test]
fn test_older_real_trade_on_a_closed_candle_is_a_late_price() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 0), rising, 100);
    // 09:17:00.3: the 09:17:00 tick closes the 09:16 bar; a backup copy of a
    // 09:16:59 trade the primary skipped arrives at 09:17:00.5 with a lower
    // cumulative: the fold marks it an older trade.
    f.cum += 100;
    c.on_tap(&tap(
        OPEN + 120,
        (OPEN + 120) * NS + 300_000_000,
        101.0,
        f.cum,
        0,
    ));
    let mut older = tap(
        OPEN + 119,
        (OPEN + 120) * NS + 500_000_000,
        101.5,
        f.cum - 50,
        0,
    );
    older.new_trade = false;
    older.older_trade = true;
    c.on_tap(&older);
    f.passes(&mut c, (OPEN + 120) * NS + 600_000_000, (OPEN + 122) * NS);
    assert_eq!(c.counters().late_ticks, 1);
    assert!(
        events(&f.out).contains(&(
            LiveOrderEventKind::Reject,
            1,
            Some(REASON_LATE_PRICE.to_owned())
        )),
        "{:?}",
        events(&f.out)
    );
    assert!(executions(&f.out).is_empty());
    // Inside the open 09:17 bar: the high widens, the close stays.
    let mut c = chain(Recorder::default(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 70, 0), |_| 100.0, 10);
    let mut inside = tap(OPEN + 65, (OPEN + 70) * NS, 104.0, f.cum - 5, 0);
    inside.new_trade = false;
    inside.older_trade = true;
    c.on_tap(&inside);
    f.drive(&mut c, (OPEN + 70, OPEN + 121, 1), |_| 100.0, 10);
    let bar = c
        .engine()
        .bars
        .iter()
        .find(|b| b.open_ist_secs == OPEN + 60)
        .copied()
        .expect("09:16 bar");
    assert!(
        (bar.high - 104.0).abs() < 1e-9,
        "the older price widens the bar"
    );
    assert!(
        (bar.close - 100.0).abs() < 1e-9,
        "the close is the newest price"
    );
}

/// LF-6b review round 2, finding 2: an instrument whose first packet of the
/// day arrives at 09:44:30 (a late subscribe, a widen or a resubscribe) has
/// a 09:30-09:45 bar built from 30 s of trades. Orders on it are refused
/// `first_bar_partial`; the next full bar decides normally.
#[test]
fn test_regression_first_bar_after_a_late_first_packet_is_refused_first_bar_partial() {
    let mut s = setup(true);
    s.engine_frames_secs = vec![900];
    let engine = Recorder {
        emit_all: true,
        ..Recorder::default()
    };
    let mut c = LiveFillChain::new(engine, s, params(), (OPEN - 600) * NS).expect("chain");
    let mut f = Feed::new();
    // First packet 09:44:30.3; the 09:30 bar closes at 09:45:00.
    f.drive(&mut c, (OPEN + 1_770, OPEN + 1_802, 0), |_| 100.0, 1_000);
    let ev = events(&f.out);
    assert!(
        ev.contains(&(
            LiveOrderEventKind::Reject,
            1,
            Some(REASON_FIRST_BAR_PARTIAL.to_owned())
        )),
        "{ev:?}"
    );
    assert!(
        ev.iter().all(|e| e.0 != LiveOrderEventKind::Built),
        "{ev:?}"
    );
    // The 09:45-10:00 bar starts after the first packet: decided normally.
    f.drive(&mut c, (OPEN + 1_802, OPEN + 2_702, 0), |_| 100.0, 1_000);
    assert!(
        events(&f.out)
            .iter()
            .any(|e| e.0 == LiveOrderEventKind::Built),
        "the first full bar decides"
    );
}

/// LF-6b review round 2, finding 3: the stream is silent from 09:16:00.05 to
/// 09:16:02.2. The feed gap `[09:16:00, 09:16:02]` is recorded and handed out
/// before the opening candle is decided (09:16:02.5) and does not touch the
/// candle itself, only its fill seconds. The decision must still refuse it.
#[test]
fn test_regression_taint_over_the_fill_window_before_the_decision_refuses_the_order() {
    let engine = Recorder {
        emit_all: true,
        ..Recorder::default()
    };
    let mut c = chain(engine, true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 60, 0), |_| 100.0, 1_000);
    // The 09:16:00 trade received at 09:16:00.05 closes the opening bar.
    f.cum += 1_000;
    c.on_tap(&tap(
        OPEN + 60,
        (OPEN + 60) * NS + 50_000_000,
        100.0,
        f.cum,
        0,
    ));
    // Silence; the pass at 09:16:02.1 records the gap.
    f.passes(
        &mut c,
        (OPEN + 60) * NS + 100_000_000,
        (OPEN + 62) * NS + 200_000_000,
    );
    assert!(c.taints_added()[1] >= 1, "a feed gap was recorded");
    assert_eq!(c.held_orders(), 0, "nothing decided yet");
    f.cum += 1_000;
    c.on_tap(&tap(
        OPEN + 62,
        (OPEN + 62) * NS + 200_000_000,
        100.0,
        f.cum,
        0,
    ));
    f.drive(&mut c, (OPEN + 63, OPEN + 125, 2), |_| 100.0, 1_000);
    let opening: Vec<_> = f
        .out
        .iter()
        .filter_map(|o| match o {
            ChainOutput::Event(e) if e.candle_start_ist_nanos == Some(OPEN * NS) => {
                Some((e.event, e.reason.clone()))
            }
            _ => None,
        })
        .collect();
    assert!(
        opening.contains(&(LiveOrderEventKind::Reject, Some("feed_gap".to_owned()))),
        "{opening:?}"
    );
    assert!(
        executions(&f.out)
            .iter()
            .all(|x| x.base.candle_start_ist_nanos != Some(OPEN * NS)),
        "no paper order fills on the silent seconds"
    );
}

/// LF-6b review round 2, finding 4: an order decided on the 15:29 bar fills
/// on seconds up to 15:31:00, while F&O still trades. A stall from 15:30:10
/// must refuse it: the feed-gap watch stays armed past the 15:30 strategy
/// close, to 15:40.
#[test]
fn test_regression_feed_gap_after_the_strategy_close_refuses_the_held_order() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    let t0 = DAY + 55_620; // 15:27:00
    let t1 = DAY + 55_810; // 15:30:10
    f.drive(&mut c, (t0, t1, 0), |_| 100.0, 1);
    assert_eq!(c.held_orders(), 1, "the 15:29 bar's order is held");
    f.passes(&mut c, (t1 + 1) * NS, (DAY + 55_830) * NS);
    assert_eq!(c.held_orders(), 0);
    let ev = events(&f.out);
    assert!(
        ev.contains(&(LiveOrderEventKind::Reject, 2, Some("feed_gap".to_owned()))),
        "{ev:?}"
    );
    assert!(
        executions(&f.out)
            .iter()
            .all(|x| x.base.candle_start_ist_nanos != Some((DAY + 55_740) * NS)),
        "the 15:29 order never fills over the stall"
    );
}

/// LF-6b review round 2, finding 5: the FRAME DRAIN runs 1.5 s behind (the
/// tap ring itself is empty, so no pass is backlogged). Each second's last
/// ticks reach the chain after the wall-clock reading would have closed the
/// second. With the drain's lag seen on the ticks, the clock follows the
/// newest receipt, so no tick of a closed second arrives late.
#[test]
fn test_regression_frame_drain_lag_holds_the_clock_to_the_stream() {
    let mut c = chain(Recorder::default(), true);
    let start = OPEN + 100;
    let end = OPEN + 130;
    let lag = 1_500_000_000;
    let ticks = (end - start) * 10;
    let mut k = 0i64;
    let mut cum = 0u32;
    let mut now = start * NS;
    while now <= (end + 5) * NS {
        while k < ticks && start * NS + k * 100_000_000 + 50_000_000 + lag <= now {
            cum += 1;
            c.on_tap(&tap(
                start + k / 10,
                start * NS + k * 100_000_000 + 50_000_000,
                100.0,
                cum,
                0,
            ));
            k += 1;
        }
        pass(&mut c, now);
        now += 100_000_000;
    }
    assert!(c.counters().bars_released > 0);
    assert_eq!(
        c.counters().late_ticks,
        0,
        "a second is never closed while its ticks are still in the drain"
    );
}

/// Asks for 10 shares of `key()` on its second 1 m bar and of nothing else.
/// TEST DOUBLE.
#[derive(Debug, Default)]
struct OneKeyOnce {
    key_bars: usize,
}

impl StrategyEngine for OneKeyOnce {
    fn configure(
        &mut self,
        _recipe: &Recipe,
        _params: Option<&ParamSnapshot>,
        _universe: &[InstrumentKey],
    ) -> Result<(), Refusal> {
        self.key_bars = 0;
        Ok(())
    }
    fn on_tick(&mut self, _tick: &TickInput, _out: &mut IntentSink) {}
    fn on_bar(&mut self, bar: &BarInput, out: &mut IntentSink) {
        if bar.key != key() {
            return;
        }
        self.key_bars += 1;
        if self.key_bars == 2 {
            out.push(intent(bar.key, Side::Buy));
        }
    }
    fn value_columns(&self) -> &[String] {
        &[]
    }
    fn bar_values(&self, _key: InstrumentKey, _tf: u8, _out: &mut [f64]) -> bool {
        false
    }
}

/// LF-6b review round 3, finding 1: 800 instruments with a paper position
/// release their 1 s bars in the same pass as an order finishes. Their marks
/// (more than the 768 slots the outbox used to have) must not crowd out the
/// Execute, and each instrument gets at most one mark per pass.
#[test]
fn test_regression_marks_of_many_traded_instruments_never_crowd_out_the_execute() {
    const OTHERS: u64 = 800;
    let mut s = setup(true);
    for i in 0..OTHERS {
        s.universe.push(InstrumentKey {
            security_id: 50_000 + i,
            exchange_segment: 1,
        });
    }
    let mut c =
        LiveFillChain::new(OneKeyOnce::default(), s, params(), (OPEN - 600) * NS).expect("chain");
    for t in c.traded_today.iter_mut().skip(1) {
        *t = true;
    }
    let mut cum = vec![0u32; usize::try_from(OTHERS).expect("fits") + 1];
    let mut out: Vec<ChainOutput> = Vec::new();
    let mut most_marks_in_a_pass = 0usize;
    let (from, to) = (OPEN, OPEN + 125);
    let mut sec = from;
    let mut now = from * NS;
    while now <= (to + 3) * NS {
        while sec < to && sec * NS + 300_000_000 <= now {
            for (i, cv) in cum.iter_mut().enumerate() {
                *cv += 1_000;
                let mut t = tap(sec, sec * NS + 300_000_000, rising(sec), *cv, 0);
                t.security_id = if i == 0 { 2885 } else { 50_000 + i as u64 - 1 };
                c.on_tap(&t);
            }
            sec += 1;
        }
        pass(&mut c, now);
        let mut pass_out = Vec::new();
        c.drain_outputs(|o| pass_out.push(o));
        most_marks_in_a_pass = most_marks_in_a_pass.max(marks(&pass_out));
        out.extend(pass_out);
        now += 100_000_000;
    }
    let x = executions(&out);
    assert_eq!(x.len(), 1, "the Execute must come out: {:?}", c.counters());
    assert_eq!(x[0].filled_qty, 10);
    assert_eq!(c.counters().executions_lost, 0);
    assert_eq!(c.counters().outbox_overflow, 0, "{:?}", c.counters());
    assert!(
        most_marks_in_a_pass > 768,
        "the pass carried more marks than the old outbox: {most_marks_in_a_pass}"
    );
    assert!(most_marks_in_a_pass <= usize::try_from(OTHERS).expect("fits") + 1);
}

/// LF-6b review round 3, finding 1: two seconds of the same instrument
/// released in one pass give ONE mark carrying the newer close.
#[test]
fn test_push_mark_folds_an_instruments_marks_within_a_pass() {
    let mut s = setup(true);
    s.universe.push(InstrumentKey {
        security_id: 9,
        exchange_segment: 1,
    });
    let mut c =
        LiveFillChain::new(Recorder::default(), s, params(), (OPEN - 600) * NS).expect("chain");
    c.pass_floor = 0;
    c.push_mark(0, 2885, ExchangeSegment::NseEquity, 100.0);
    c.push_mark(0, 2885, ExchangeSegment::NseEquity, 101.5);
    assert_eq!(c.outputs_waiting(), 1);
    assert_eq!(c.counters().marks_coalesced, 1);
    // A new pass never folds into an earlier pass's mark still queued.
    c.pass_floor = c.outbox.len();
    c.push_mark(0, 2885, ExchangeSegment::NseEquity, 102.0);
    let mut prices = Vec::new();
    c.drain_outputs(|o| {
        if let ChainOutput::Cmd(PaperCmd::Mark { price, .. }) = o {
            prices.push(price);
        }
    });
    assert_eq!(prices, vec![101.5, 102.0]);
}

/// LF-7: the Decision row carries the four pricing inputs the daily report
/// re-prices with (tick, lot, participation, window); the Built row and the
/// execution's later rows keep them NULL.
#[test]
fn test_decision_row_carries_pricing_inputs() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 0), rising, 1_000);
    f.drive(&mut c, (OPEN + 120, OPEN + 125, 2), |_| 102.0, 1_000);
    let rows: Vec<&LiveOrderEvent> = f
        .out
        .iter()
        .filter_map(|o| match o {
            ChainOutput::Event(e) => Some(e.as_ref()),
            _ => None,
        })
        .collect();
    let decision = rows
        .iter()
        .find(|e| e.event == LiveOrderEventKind::Decision)
        .expect("a decision row");
    assert_eq!(decision.tick_paise, Some(5));
    assert_eq!(decision.lot_size, Some(1));
    assert_eq!(decision.participation_bps, Some(2_500));
    assert_eq!(decision.max_fill_seconds, Some(60));
    let built = rows
        .iter()
        .find(|e| e.event == LiveOrderEventKind::Built)
        .expect("a built row");
    assert_eq!(built.tick_paise, None);
    assert_eq!(built.max_fill_seconds, None);
    let x = executions(&f.out);
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].base.tick_paise, None);
    assert_eq!(x[0].base.participation_bps, None);
}
