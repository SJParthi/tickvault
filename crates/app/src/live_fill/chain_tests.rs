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
        trading_day: DAY / 86_400,
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
        volume_delta: 0,
        first_bucket_volume_delta: 0,
        cumulative_restarted: false,
        stale_packet: false,
        fold_seed: false,
        fold_gap_reseed: false,
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
    /// Ask for exits instead of entries.
    exits: bool,
    /// An entry (`side`) on the first bar, the exit of it on the second.
    entry_then_exit: bool,
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
        if self.entry_then_exit && self.bars.len() <= 2 {
            let side = self.side.unwrap_or(Side::Buy);
            let mut i = intent(bar.key, side);
            if self.bars.len() == 2 {
                i.kind = IntentKind::Exit;
                i.side = match side {
                    Side::Buy => Side::Sell,
                    Side::Sell => Side::Buy,
                };
            }
            out.push(i);
            return;
        }
        if (self.emit && self.bars.len() > 1) || self.emit_all {
            let mut i = intent(bar.key, self.side.unwrap_or(Side::Buy));
            if self.exits {
                i.kind = IntentKind::Exit;
            }
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
                let mut t = tap(s, s * NS + 300_000_000, price(s), self.cum, self.drop_seq);
                t.volume_delta = vol;
                t.first_bucket_volume_delta = vol;
                c.on_tap(&t);
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

/// LF-9 finding 12: a day roll drops held orders and resets the book, but
/// the setup is yesterday's, so the chain decides nothing on the new day
/// (the engine is not configured on it and sees no candle). The end of the
/// test covers `reload_for_day`, which has NO production caller today (LF-9
/// review): in production the chain stays inert until the process restarts.
#[test]
fn test_day_roll_awaiting_day_setup_until_reload_for_day_never_decides_on_yesterdays_setup() {
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 120, 1), rising, 1);
    assert_eq!(c.held_orders(), 1);
    assert!(!c.awaiting_day_setup());
    let bars_before = c.engine().bars.len();
    let next_day = (DAY + 86_400 + 30_000) * NS;
    pass(&mut c, next_day);
    assert_eq!(c.held_orders(), 0);
    assert_eq!(c.day(), DAY / 86_400 + 1);
    assert!(c.awaiting_day_setup(), "yesterday's setup is stale");
    assert_eq!(c.counters().day_rolls_without_setup, 1);
    assert_eq!(
        c.engine().bars.len(),
        bars_before,
        "the engine was not configured on yesterday's setup"
    );
    assert_eq!(c.new_day(next_day).expect("again"), 0);
    assert_eq!(c.counters().day_rolls_without_setup, 1, "counted once");
    // The new day's session: candles close, none reaches the engine, no
    // order is held and no Execute or Decision row comes out.
    let open2 = OPEN + 86_400;
    let mut f = Feed::new();
    f.drive(&mut c, (open2, open2 + 180, 2), |_| 101.0, 10);
    assert_eq!(
        c.engine().bars.len(),
        bars_before,
        "no candle reaches the engine"
    );
    assert!(c.counters().bars_unfed_stale_setup > 0);
    assert_eq!(c.held_orders(), 0);
    assert!(executions(&f.out).is_empty());
    assert!(events(&f.out).is_empty(), "{:?}", events(&f.out));
    // A setup loaded for the new day: the chain decides again.
    let mut today = setup(true);
    today.trading_day += 1;
    let mut c = c.reload_for_day(today, (open2 - 600) * NS).expect("reload");
    assert!(!c.awaiting_day_setup());
    assert!(c.engine().bars.is_empty(), "configured for the new day");
    let mut f = Feed::new();
    f.drive(
        &mut c,
        (open2, open2 + 120, 1),
        |s| if s < open2 + 60 { 100.0 } else { 101.0 },
        1,
    );
    assert_eq!(c.held_orders(), 1, "decides again on today's setup");
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

/// Finding 2: a first-sight packet (`new_trade` false: an old trade time,
/// price and cumulative) opens no bar and adds no volume, so the first
/// trade's bar carries that trade's own credited volume and nothing else.
///
/// LF-9 review, third pass: this test was named for the chain's volume
/// BASELINE, which LF-9 removed (volume is the fold's own delta); setting
/// the trade's delta by hand and reading it back proved only a field copy,
/// and passed with the old differencing too. It now gives the first-sight
/// packet a non-zero delta (no fold would) and checks none of it reaches a
/// bar. That the first trade's volume through the REAL fold matches the
/// stored candle is `test_regression_chain_bar_volume_equals_the_fold_bar_volume`.
#[test]
fn test_first_sight_packet_opens_no_bar_and_adds_no_volume() {
    let mut c = chain(Recorder::default(), true);
    let mut first = tap(OPEN - 3_600, OPEN * NS - 1, 99.0, 5_000, 0);
    first.new_trade = false;
    first.volume_delta = 700;
    first.first_bucket_volume_delta = 700;
    c.on_tap(&first);
    let mut trade = tap(OPEN, OPEN * NS + 300_000_000, 100.0, 5_400, 0);
    trade.volume_delta = 400;
    trade.first_bucket_volume_delta = 400;
    c.on_tap(&trade);
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
        "the first trade's own volume, none of the first-sight packet's"
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
                t.volume_delta = 1_000;
                t.first_bucket_volume_delta = 1_000;
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

/// Runs `ticks` (IST second, raw `u32` cumulative) of one stock option
/// through the live candle fold, as the drain does, and taps every folded
/// tick into a chain whose engine records its 1 m bars. Returns the fold's
/// sealed 1 m volumes and the chain's 1 m bar volumes, by bucket open.
fn fold_and_chain_minutes(
    ticks: &[(i64, u32)],
) -> (
    std::collections::BTreeMap<i64, u64>,
    std::collections::BTreeMap<i64, u64>,
) {
    let option = InstrumentKey {
        security_id: 52_175,
        exchange_segment: 2,
    };
    let full: Vec<(i64, i64, f32, u32)> = ticks
        .iter()
        .map(|&(secs, cum)| (secs, 300_000_000, 100.0, cum))
        .collect();
    let (fold, chain) = fold_and_chain_bars(option, &full, &[]);
    let volumes = |m: std::collections::BTreeMap<i64, (u64, f64)>| {
        m.into_iter().map(|(k, (v, _))| (k, v)).collect()
    };
    (volumes(fold), volumes(chain))
}

/// 1 m bars by bucket open: (volume, close).
type MinuteBars = std::collections::BTreeMap<i64, (u64, f64)>;

/// [`fold_and_chain_minutes`] for any instrument `key` and full ticks (IST
/// second, receipt nanoseconds after that second, price, raw `u32`
/// cumulative). Returns the fold's sealed 1 m (volume, close) and the
/// chain's 1 m bar (volume, close), by bucket open. The ticks at the indices
/// in `dropped` are folded but never reach the chain (a tap ring drop): the
/// next tapped tick carries a moved `drop_seq`, as the tap does.
fn fold_and_chain_bars(
    key: InstrumentKey,
    ticks: &[(i64, i64, f32, u32)],
    dropped: &[usize],
) -> (MinuteBars, MinuteBars) {
    let (fold, chain, _) = fold_and_chain_with(key, ticks, dropped, OPEN - 600, 0);
    (fold, chain)
}

/// [`fold_and_chain_bars`] with the process (fold capture and chain) started
/// at IST second `start` (after 09:10 is a mid-session boot) and the tap's
/// loss count already at `drop_seq0` when the first tick arrives (a loss of
/// another instrument, a socket blip). Also returns the chain.
fn fold_and_chain_with(
    key: InstrumentKey,
    ticks: &[(i64, i64, f32, u32)],
    dropped: &[usize],
    start: i64,
    drop_seq0: u32,
) -> (MinuteBars, MinuteBars, LiveFillChain<Recorder>) {
    fold_and_chain_after_replay(key, &[], ticks, dropped, start, drop_seq0)
}

/// [`fold_and_chain_with`] after a boot WAL replay: the fold first re-folds
/// `replayed` (same shape as `ticks`) in replay mode, never tapped (the tap
/// sees live frames only), then hands over (`finish_replay`) as the lane
/// start does, then folds and taps `ticks` live.
fn fold_and_chain_after_replay(
    key: InstrumentKey,
    replayed: &[(i64, i64, f32, u32)],
    ticks: &[(i64, i64, f32, u32)],
    dropped: &[usize],
    start: i64,
    drop_seq0: u32,
) -> (MinuteBars, MinuteBars, LiveFillChain<Recorder>) {
    use crate::dhan_feed_stack::fold_verdict::{FoldVerdict, fold_verdict};
    use crate::dhan_feed_stack::live_fill_tap::TapVolume;
    use tickvault_common::feed::Feed as TickFeed;
    use tickvault_common::tick_types::ParsedTick;
    use tickvault_trading::candles::TfIndex;
    let mut s = setup(true);
    s.universe = vec![key];
    s.contracts.insert(
        key,
        ContractSpec {
            tick_paise: 5,
            lot_size: 1,
            symbol: "OPT".to_owned(),
        },
    );
    let mut c = LiveFillChain::new(Recorder::default(), s, params(), start * NS).expect("chain");
    let mut fold = crate::dhan_feed_stack::live_fold(64);
    fold.set_live_capture_start(u32::try_from(start).expect("fits"));
    let mut sealed = std::collections::BTreeMap::new();
    let mut f = Feed::new();
    let ist_utc = IST_OFFSET_NS_FOR_TESTS;
    let mut drop_seq = drop_seq0;
    if !replayed.is_empty() {
        fold.set_replay_mode(true);
        for &(secs, receipt_off, price, cum) in replayed {
            let tick = ParsedTick {
                security_id: key.security_id,
                exchange_segment_code: key.exchange_segment,
                last_traded_price: price,
                exchange_timestamp: u32::try_from(secs).expect("fits"),
                received_at_nanos: secs * NS + receipt_off - ist_utc,
                volume: cum,
                ..ParsedTick::default()
            };
            let _stats = fold.consume_tick(TickFeed::Dhan, &tick, None, |_, _, _, _, _| {});
        }
        let _handed = fold.finish_replay(false, |_, _, _, _, _| {});
    }
    for (n, &(secs, receipt_off, price, cum)) in ticks.iter().enumerate() {
        let receipt_ist = secs * NS + receipt_off;
        f.passes(&mut c, (secs * NS).max(OPEN * NS - NS), receipt_ist);
        let tick = ParsedTick {
            security_id: key.security_id,
            exchange_segment_code: key.exchange_segment,
            last_traded_price: price,
            exchange_timestamp: u32::try_from(secs).expect("fits"),
            received_at_nanos: receipt_ist - ist_utc,
            volume: cum,
            ..ParsedTick::default()
        };
        let stats = fold.consume_tick(TickFeed::Dhan, &tick, None, |_, _, _, tf, state| {
            if tf == TfIndex::M1 {
                sealed.insert(
                    i64::from(state.bucket_start_ist_secs),
                    (state.volume, state.close),
                );
            }
        });
        if dropped.contains(&n) {
            drop_seq = drop_seq.wrapping_add(1);
            continue;
        }
        if let FoldVerdict::Folded {
            new_trade,
            older_trade,
        } = fold_verdict(&stats, tick.received_at_nanos)
        {
            c.on_tap(&TapTick::from_parsed(
                &tick,
                None,
                drop_seq,
                new_trade,
                older_trade,
                TapVolume::from_stats(&stats),
            ));
        }
    }
    let last = ticks.last().map_or(OPEN, |t| t.0);
    f.passes(&mut c, last * NS + 400_000_000, (last + 5) * NS);
    let chain_bars = c
        .engine()
        .bars
        .iter()
        .map(|b| (b.open_ist_secs, (b.volume, b.close)))
        .collect();
    (sealed, chain_bars, c)
}

/// LF-9 review (high): an index prints two values in one exchange second.
/// The fold reads the second as an `older_trade` (same second, no new
/// cumulative, another price) but NOT a stale packet, so its stored 1 m
/// close is the later price. The chain must close on the same price; a
/// stale packet (a cumulative below the high mark) still never moves it.
#[test]
fn test_regression_same_second_index_ticks_close_where_the_fold_closes() {
    let nifty = InstrumentKey {
        security_id: 13,
        exchange_segment: 0,
    };
    let t = OPEN + 2_700;
    let ticks = [
        (t, 100_000_000, 100.0_f32, 0_u32),
        (t, 600_000_000, 101.0, 0),
        (t + 70, 300_000_000, 102.0, 0),
        (t + 130, 300_000_000, 103.0, 0),
    ];
    let (fold, chain) = fold_and_chain_bars(nifty, &ticks, &[]);
    let minute = t - t.rem_euclid(60);
    let want = fold
        .get(&minute)
        .copied()
        .expect("the fold sealed the minute");
    assert_eq!(
        want.1, 101.0,
        "the fold closes on the later price: {fold:?}"
    );
    assert_eq!(
        chain.get(&minute).copied(),
        Some(want),
        "chain {chain:?} fold {fold:?}"
    );
    // A stock whose second tick is a stale packet (cumulative below the
    // high mark): neither side moves the close.
    let stock = InstrumentKey {
        security_id: 52_175,
        exchange_segment: 2,
    };
    let ticks = [
        (t, 100_000_000, 100.0_f32, 1_000_u32),
        (t + 1, 100_000_000, 101.0, 1_200),
        (t + 2, 100_000_000, 120.0, 1_100),
        (t + 70, 300_000_000, 102.0, 1_300),
        (t + 130, 300_000_000, 103.0, 1_400),
    ];
    let (fold, chain) = fold_and_chain_bars(stock, &ticks, &[]);
    let want = fold
        .get(&minute)
        .copied()
        .expect("the fold sealed the minute");
    assert_eq!(
        want.1, 101.0,
        "a stale packet never moves the close: {fold:?}"
    );
    assert_eq!(chain.get(&minute).copied(), Some(want), "{chain:?}");
}

const IST_OFFSET_NS_FOR_TESTS: i64 = tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS;

/// LF-9 findings 8, 9 and 10: every chain 1 m bar carries the volume of the
/// stored candle of the same minute, for the opening bar (the day's first
/// trade, which the chain used to swallow as its baseline), across a `u32`
/// cumulative wrap (which the chain used to read as a fall, adding 0 for the
/// rest of the day) and across a vendor counter restart.
#[test]
fn test_regression_chain_bar_volume_equals_the_fold_bar_volume() {
    // An instrument first seen ten minutes after the open: its first trade
    // only seeds (the fold's own rule), on both sides.
    let b = OPEN + 600;
    let cases: [(&str, i64, Vec<(i64, u32)>); 3] = [
        (
            "opening bar",
            OPEN,
            vec![
                (OPEN, 1_000),
                (OPEN + 30, 1_400),
                (OPEN + 70, 1_500),
                (OPEN + 125, 1_600),
            ],
        ),
        (
            "u32 wrap",
            b,
            vec![
                (b, 4_294_967_000),
                (b + 20, 4_294_967_200),
                (b + 40, 100),
                (b + 65, 300),
                (b + 121, 400),
            ],
        ),
        (
            "counter restart",
            b,
            vec![
                (b, 3_000_000_000),
                (b + 20, 3_000_000_100),
                (b + 40, 1_000),
                (b + 50, 1_200),
                (b + 70, 1_300),
                (b + 125, 1_400),
            ],
        ),
    ];
    for (name, start, ticks) in cases {
        let (fold, chain) = fold_and_chain_minutes(&ticks);
        for minute in [start, start + 60] {
            let want = fold.get(&minute).copied();
            assert!(want.is_some(), "{name}: the fold sealed {minute}: {fold:?}");
            assert_eq!(
                chain.get(&minute).copied(),
                want,
                "{name}: minute {}: fold {fold:?} chain {chain:?}",
                minute - OPEN
            );
        }
        if name == "opening bar" {
            assert_eq!(
                fold.get(&OPEN),
                Some(&1_400),
                "the first trade's own volume"
            );
        }
        if name == "u32 wrap" {
            assert_eq!(fold.get(&b), Some(&396), "the wrap counts forward");
        }
    }
}

/// LF-9 review: tapped ticks the tap ring dropped after the fold counted
/// them no longer leave the chain's bars short for the rest of the day: the
/// next tick of the instrument carries their volume, so the chain's day total
/// equals the fold's (the minute a dropped tick's volume lands in may differ).
#[test]
fn test_regression_dropped_tap_ticks_keep_the_chain_day_volume_whole() {
    let option = InstrumentKey {
        security_id: 52_175,
        exchange_segment: 2,
    };
    let b = OPEN + 600;
    let ticks = [
        (b, 300_000_000, 100.0_f32, 10_000_u32),
        (b + 10, 300_000_000, 100.0, 10_500),
        (b + 20, 300_000_000, 100.0, 12_000),
        (b + 30, 300_000_000, 100.0, 15_000),
        (b + 40, 300_000_000, 100.0, 15_100),
        (b + 70, 300_000_000, 100.0, 15_400),
        (b + 80, 300_000_000, 100.0, 16_000),
        (b + 130, 300_000_000, 100.0, 16_100),
        (b + 190, 300_000_000, 100.0, 16_200),
    ];
    // Ticks 2 and 3 (4,500 shares) and tick 6 (600 shares) never reach the
    // chain.
    let (fold, chain) = fold_and_chain_bars(option, &ticks, &[2, 3, 6]);
    let total = |m: &std::collections::BTreeMap<i64, (u64, f64)>| {
        m.iter()
            .filter(|(k, _)| **k < b + 180)
            .map(|(_, v)| v.0)
            .sum::<u64>()
    };
    assert_eq!(total(&fold), 6_100, "{fold:?}");
    assert_eq!(total(&chain), total(&fold), "chain {chain:?} fold {fold:?}");
}

/// LF-9 review: the thread's own steps (not the chain methods alone) apply
/// a position report from the runtime and take back an Execute the command
/// channel refused, so a later edit dropping either loop breaks this test.
#[test]
fn test_regression_thread_steps_apply_reports_and_forget_unsent() {
    use crate::live_fill::{SendDrops, apply_position_reports, forward_outputs};
    use tickvault_core::websocket::frame_ring::frame_ring;
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 125, 66), rising, 1_000);
    let x = executions(&f.out);
    assert_eq!(x.len(), 1);
    assert_eq!(c.net_shares[0], 10, "counted while in flight");
    // The runtime refused it: its report, through the thread's step.
    let (pos_tx, mut pos_rx) = frame_ring::<PositionReport>(4);
    pos_rx.set_busy_polling(true);
    assert!(
        pos_tx
            .try_send(crate::live_fill::paper_rows::position_report(&x[0], 0))
            .is_ok()
    );
    apply_position_reports(&mut c, &mut pos_rx);
    assert_eq!(c.counters().position_reports, 1);
    assert_eq!(c.net_shares[0], 0);
    // The same Execute again, now refused by a full command channel: the
    // thread's step takes its shares back.
    c.net_shares[0] = 10;
    c.inflight[0] = 10;
    c.push_out(ChainOutput::Cmd(PaperCmd::Execute(Box::new(x[0].clone()))));
    let (cmd_tx, _cmd_rx) = frame_ring::<PaperCmd>(1);
    assert!(
        cmd_tx
            .try_send(PaperCmd::Mark {
                security_id: 1,
                segment: tickvault_common::types::ExchangeSegment::NseEquity,
                price: 1.0,
            })
            .is_ok()
    );
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(8);
    let mut drops = SendDrops::default();
    let mut unsent = Vec::with_capacity(4);
    forward_outputs(&mut c, &cmd_tx, &ev_tx, &mut drops, &mut unsent);
    assert_eq!(drops.cmds, 1);
    assert_eq!(c.counters().executes_unsent, 1);
    assert_eq!((c.net_shares[0], c.inflight[0]), (0, 0));
    assert!(unsent.is_empty());
}

/// LF-9 review: an exit decided while its entry is still held (waiting for
/// volume) is sized against the position that entry opens, not refused
/// `exit_without_position` with the entry then filling and never closed.
#[test]
fn test_regression_an_exit_decided_while_its_entry_is_held_is_placed() {
    let engine = Recorder {
        entry_then_exit: true,
        ..Recorder::default()
    };
    let mut c = chain(engine, true);
    let mut f = Feed::new();
    // The 09:15 candle asks for Buy 10 (decided 09:16:02.5; fill seconds
    // 09:16:00 to 09:16:59). The 09:16 minute trades nothing until 09:16:59,
    // so the entry (covered there) is still held, waiting for 09:17:00 to be
    // final, when the 09:16 candle asks for the exit (09:17:01.0).
    f.drive(&mut c, (OPEN, OPEN + 60, 0), |_| 100.0, 1_000);
    f.drive(&mut c, (OPEN + 60, OPEN + 119, 0), |_| 100.0, 0);
    f.drive(&mut c, (OPEN + 119, OPEN + 200, 5), |_| 100.0, 1_000);
    let ev = events(&f.out);
    assert!(
        !ev.iter()
            .any(|e| e.2.as_deref() == Some("exit_without_position")),
        "the exit is not refused: {ev:?}"
    );
    let x = executions(&f.out);
    assert_eq!(x.len(), 2, "the entry and its exit: {ev:?}");
    assert_eq!(x[0].filled_qty, 10, "the entry filled: {x:?}");
    assert!(x[1].exit);
    assert_eq!(c.net_shares[0], 0, "flat after the exit: {x:?}");
}

/// LF-9 finding 2: (a) an exit decided while an earlier exit is held is
/// held too, not refused (LF-9 review finding 7: the reservation that
/// refused it left a short first exit's remainder open); the held book
/// finishes it after the first, and the runtime clamps it against what the
/// first left (see `test_regression_a_short_first_exit_is_completed_by_the_overlapping_one`);
/// (b) the runtime's position report replaces the chain's own count, so an
/// entry the runtime refused (or an exit it cut or gave up) no longer leaves
/// the chain sizing later exits against a position the runtime does not hold.
#[test]
fn test_regression_overlapping_exits_on_position_report_and_forget_unsent_resync_the_position() {
    // (a) Long 10 (as the runtime reports it); exits of 10 on two
    // consecutive candles: the second, decided while the first is held, is
    // held as well.
    let engine = Recorder {
        emit_all: true,
        exits: true,
        side: Some(Side::Sell),
        ..Recorder::default()
    };
    let mut c = chain(engine, true);
    c.on_position_report(&PositionReport {
        security_id: 2885,
        segment: 1,
        expected_shares: 0,
        runtime_net_shares: 10,
    });
    assert_eq!(c.counters().position_resyncs, 1);
    let mut f = Feed::new();
    // The 09:15 and 09:16 candles each ask for an exit; the first is still
    // held (its window runs past 09:17:01) when the second is decided.
    f.drive(&mut c, (OPEN, OPEN + 120, 1), rising, 1);
    let ev = events(&f.out);
    // No Reject row at all, under any reason (LF-9 review, third pass: the
    // `exit_already_held` reason this asserted against no longer exists, so
    // that check could never fail).
    assert!(
        !ev.iter().any(|e| e.0 == LiveOrderEventKind::Reject),
        "the overlapping exit is not refused: {ev:?}"
    );
    assert_eq!(c.counters().held, 2, "both exits were held: {ev:?}");
    // (b) The runtime refused an entry the chain counted: its report puts
    // the chain back on the runtime's (flat) position.
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 125, 66), rising, 1_000);
    let x = executions(&f.out);
    assert_eq!(x.len(), 1, "one entry went to the runtime");
    assert_eq!(x[0].filled_qty, 10);
    assert_eq!(c.net_shares[0], 10, "counted while in flight");
    let refused = crate::live_fill::paper_rows::position_report(&x[0], 0);
    c.on_position_report(&refused);
    assert_eq!(c.net_shares[0], 0, "the runtime booked nothing");
    assert_eq!(c.inflight[0], 0);
    // An Execute the command channel refused is taken back too.
    let mut c = chain(emitting(), true);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 125, 66), rising, 1_000);
    let x = executions(&f.out);
    assert_eq!(c.net_shares[0], 10);
    c.forget_unsent(x[0].security_id, x[0].segment.binary_code(), 10);
    assert_eq!((c.net_shares[0], c.inflight[0]), (0, 0));
    assert_eq!(c.counters().executes_unsent, 1);
}

/// LF-9 review, finding 7: a held exit fills SHORT (5 of 10 on a thin
/// minute) while a second exit is decided over it. The second is no longer
/// refused (`exit_already_held` left the other 5 open until the daily
/// reset): it is held, finishes after the first, and the runtime clamps it
/// against what the first left, so the paper position ends flat.
#[test]
fn test_regression_a_short_first_exit_is_completed_by_the_overlapping_one() {
    use crate::live_fill::paper_rows::{clamp_exit, position_report};
    let engine = Recorder {
        emit_all: true,
        exits: true,
        side: Some(Side::Sell),
        ..Recorder::default()
    };
    let mut c = chain(engine, true);
    let mut runtime: i64 = 10;
    c.on_position_report(&PositionReport {
        security_id: 2885,
        segment: 1,
        expected_shares: 0,
        runtime_net_shares: runtime,
    });
    let mut f = Feed::new();
    // The 09:15 candle's exit fills 1 share a second for 5 seconds of its
    // 09:16 window, then nothing: 5 of 10. The 09:16 candle's exit (decided
    // at 09:17:01 while the first is still held) fills on 09:17.
    f.drive(&mut c, (OPEN, OPEN + 60, 0), |_| 100.0, 1_000);
    f.drive(&mut c, (OPEN + 60, OPEN + 65, 0), |_| 100.0, 4);
    f.drive(&mut c, (OPEN + 65, OPEN + 120, 0), |_| 100.0, 0);
    f.drive(&mut c, (OPEN + 120, OPEN + 260, 70), |_| 100.0, 1_000);
    let ev = events(&f.out);
    // Both exits were held, under no Reject of any reason (LF-9 review,
    // third pass: the `exit_already_held` reason this asserted against no
    // longer exists). Later candles' exits find no position left and are
    // refused `exit_without_position`, which is expected.
    let kinds: Vec<LiveOrderEventKind> = ev.iter().take(4).map(|e| e.0).collect();
    assert_eq!(
        kinds,
        vec![
            LiveOrderEventKind::Decision,
            LiveOrderEventKind::Built,
            LiveOrderEventKind::Decision,
            LiveOrderEventKind::Built,
        ],
        "{ev:?}"
    );
    // The runtime, in the order the Executes were sent.
    let x = executions(&f.out);
    assert_eq!(x.len(), 2, "both exits reached the runtime: {ev:?}");
    assert_eq!(x[0].filled_qty, 5, "the first exit filled short: {x:?}");
    for e in &x {
        assert!(e.exit);
        let held = i32::try_from(runtime).expect("fits");
        let booked = clamp_exit(held, e).map_or(0, |cut| cut.filled_qty);
        runtime -= booked;
        c.on_position_report(&position_report(e, i32::try_from(runtime).expect("fits")));
    }
    assert_eq!(runtime, 0, "the second exit closed what the first left");
    assert_eq!(c.net_shares[0], 0);
}

/// LF-9 review, finding 3: volume of a tapped tick the tap ring dropped is
/// never added to a later bar outside every tainted stretch. An illiquid
/// instrument's dropped tick (300 shares, the fold credited them to its own
/// minute) is followed by its next trade ten minutes later, on a bar no
/// taint covers: that bar carries only its own 20 shares (as the stored
/// candle does), the 300 are counted as discarded, and the instrument's day
/// volume is marked unknown, so its orders are refused for the day.
#[test]
fn test_regression_dropped_volume_never_lands_on_an_untainted_bar() {
    let mut c = chain(Recorder::default(), true);
    let carry = OPEN + 1_200;
    let mut drop_seq = 0u32;
    for s in OPEN + 590..=carry + 70 {
        if s == OPEN + 610 {
            // The instrument's tick at this second (cum 10,300) was dropped
            // by the tap ring after the fold counted it.
            drop_seq = 1;
            c.note_tap_loss(1);
        }
        // Another instrument keeps the stream alive (no feed gap).
        let mut other = tap(s, s * NS + 300_000_000, 50.0, 0, drop_seq);
        other.security_id = 999;
        c.on_tap(&other);
        let mut t = None;
        if s == OPEN + 600 {
            t = Some((10_000, 10_000));
        } else if s == carry {
            t = Some((10_320, 20));
        }
        if let Some((cum, delta)) = t {
            let mut k = tap(s, s * NS + 300_000_000, 100.0, cum, drop_seq);
            k.volume_delta = delta;
            k.first_bucket_volume_delta = delta;
            c.on_tap(&k);
        }
        pass(&mut c, s * NS + 400_000_000);
        c.drain_outputs(|_| {});
    }
    assert_eq!(c.counters().missed_volume_discarded, 300);
    assert!(c.volume_unknown[0], "the day volume is unknown");
    let bar = c
        .engine()
        .bars
        .iter()
        .find(|b| b.open_ist_secs == carry)
        .expect("the carry minute was released");
    assert_eq!(bar.volume, 20, "the stored candle's volume, not 320");
}

/// LF-9 review, finding 9: an instrument whose FIRST tapped tick comes after
/// the tap lost ticks the same day, and whose fold delta is not its whole
/// cumulative, may have lost volume the chain cannot place (a dropped first
/// tick has nothing to difference against): its day volume is marked unknown
/// and its orders are refused `day_volume_unknown`. With no loss, or with
/// the fold crediting the whole cumulative (a pre-market boot's first
/// trade), nothing is marked.
#[test]
fn test_regression_first_tapped_tick_after_a_loss_marks_the_day_volume_unknown() {
    for (drop_seq, cum0, flagged) in [(1u32, 5_000u32, true), (0, 5_000, false), (1, 0, false)] {
        let mut c = chain(emitting(), true);
        let mut f = Feed::new();
        f.cum = cum0;
        f.drop_seq = drop_seq;
        f.drive(&mut c, (OPEN, OPEN + 240, 66), rising, 1_000);
        let case = format!("drop_seq={drop_seq} cum0={cum0}");
        assert_eq!(c.volume_unknown[0], flagged, "{case}");
        assert_eq!(
            c.counters().volume_unknown_marked,
            u64::from(flagged),
            "{case}"
        );
        let refused = events(&f.out)
            .iter()
            .any(|e| e.2.as_deref() == Some(REASON_DAY_VOLUME_UNKNOWN));
        assert_eq!(refused, flagged, "{case}: {:?}", events(&f.out));
    }
}

/// LF-9 review (second pass), the high finding: a day-long
/// `day_volume_unknown` mark refuses only NEW risk. A paper long booked before
/// the instrument was marked is still closed by the strategy's exit (it was
/// refused for the rest of the day, so the position stayed open until the
/// daily reset); an entry on the same instrument is still refused.
#[test]
fn test_regression_a_day_volume_unknown_mark_still_lets_an_exit_out() {
    let engine = Recorder {
        emit_all: true,
        exits: true,
        side: Some(Side::Sell),
        ..Recorder::default()
    };
    let mut c = chain(engine, true);
    c.on_position_report(&PositionReport {
        security_id: 2885,
        segment: 1,
        expected_shares: 0,
        runtime_net_shares: 10,
    });
    c.mark_volume_unknown(0);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 130, 66), rising, 1_000);
    let ev = events(&f.out);
    assert!(
        !ev.iter()
            .any(|e| e.2.as_deref() == Some(REASON_DAY_VOLUME_UNKNOWN)),
        "an exit is never refused for the mark: {ev:?}"
    );
    let x = executions(&f.out);
    assert!(!x.is_empty(), "the exit was sent: {ev:?}");
    assert!(x.iter().all(|e| e.exit), "{x:?}");
    assert_eq!(c.net_shares[0], 0, "the long is closed: {x:?}");
    // An entry on a marked instrument is still refused.
    let mut c = chain(emitting(), true);
    c.mark_volume_unknown(0);
    let mut f = Feed::new();
    f.drive(&mut c, (OPEN, OPEN + 130, 66), rising, 1_000);
    assert!(executions(&f.out).is_empty());
    assert!(
        events(&f.out)
            .iter()
            .any(|e| e.2.as_deref() == Some(REASON_DAY_VOLUME_UNKNOWN)),
        "{:?}",
        events(&f.out)
    );
}

/// LF-9 review, third pass: a mid-session boot re-folds the WAL tail, which
/// seeds the fold's slot of every instrument that traded there, so the
/// instrument's first LIVE tick is not the seed but the tick that ends the
/// replay gap (delta 0, `fold_seed` false). After a tap loss of another
/// instrument (`drop_seq` moved) it was marked `day_volume_unknown` and every
/// entry on it refused for the day, although the fold credited no earlier
/// live tick of it. It now carries `fold_gap_reseed` and is not marked. A
/// later tick whose own volume the tap dropped is still found (the bars stay
/// at or below the stored candles).
#[test]
fn test_regression_a_replay_gap_tick_after_a_loss_is_never_marked_unknown() {
    use crate::dhan_feed_stack::live_fill_tap::TapVolume;
    use tickvault_common::feed::Feed as TickFeed;
    use tickvault_common::tick_types::ParsedTick;
    let option = InstrumentKey {
        security_id: 52_175,
        exchange_segment: 2,
    };
    let r = 300_000_000;
    let mid = OPEN + 3_600;
    // The previous process's tail, re-folded by the boot replay.
    let replayed = vec![
        (mid - 90, r, 99.5_f32, 49_000_u32),
        (mid - 40, r, 99.8, 49_600),
    ];
    let live = vec![
        (mid + 100, r, 100.0_f32, 50_000_u32),
        (mid + 110, r, 100.5, 50_500),
        (mid + 170, r, 101.0, 51_000),
        (mid + 240, r, 101.5, 51_500),
    ];
    // The fold's own reading of the first live tick: a gap end, not a seed.
    let mut fold = crate::dhan_feed_stack::live_fold(64);
    fold.set_live_capture_start(u32::try_from(mid).expect("fits"));
    fold.set_replay_mode(true);
    let parsed = |(secs, off, price, cum): (i64, i64, f32, u32)| ParsedTick {
        security_id: option.security_id,
        exchange_segment_code: option.exchange_segment,
        last_traded_price: price,
        exchange_timestamp: u32::try_from(secs).expect("fits"),
        received_at_nanos: secs * NS + off - IST_OFFSET_NS_FOR_TESTS,
        volume: cum,
        ..ParsedTick::default()
    };
    for t in &replayed {
        let _s = fold.consume_tick(TickFeed::Dhan, &parsed(*t), None, |_, _, _, _, _| {});
    }
    let _handed = fold.finish_replay(false, |_, _, _, _, _| {});
    let stats = fold.consume_tick(TickFeed::Dhan, &parsed(live[0]), None, |_, _, _, _, _| {});
    let v = TapVolume::from_stats(&stats);
    assert_eq!(v.delta, 0, "{stats:?}");
    assert!(
        !v.seed,
        "the replay seeded the slot, not this tick: {stats:?}"
    );
    assert!(
        v.gap_reseed,
        "the first live tick ends the replay gap: {stats:?}"
    );
    // Through the chain, with another instrument's loss already counted.
    let (fold_bars, chain_bars, c) =
        fold_and_chain_after_replay(option, &replayed, &live, &[], mid, 3);
    assert!(!c.volume_unknown[0], "a replay gap end is never marked");
    assert_eq!(c.counters().volume_unknown_marked, 0);
    for (open, (v, _)) in &chain_bars {
        if let Some((stored, _)) = fold_bars.get(open) {
            assert!(v <= stored, "chain {chain_bars:?} fold {fold_bars:?}");
        }
    }
    // The gap end itself dropped by the tap: the next tapped tick is neither
    // a seed nor a gap end, so the loss is still marked (refuse more, never
    // fewer).
    let (_, _, c) = fold_and_chain_after_replay(option, &replayed, &live, &[0], mid, 3);
    assert!(
        c.volume_unknown[0],
        "a tick after a dropped gap end is marked"
    );
}

/// LF-9 review (second pass): a first tapped tick that is the fold's own
/// SEED is never marked `day_volume_unknown`, whatever loss came before it:
/// the fold credited nothing earlier, so nothing can be missing. Driven
/// through the real fold: (a) a cash equity's 09:15 first trade after a
/// pre-open auction (its first bucket opens on the auction volume, so its
/// delta is below the cumulative), (b) the same with no pre-open packet,
/// (c) a mid-session boot's first trade (delta 0). (d) A mid-session boot
/// whose real seed tick the tap dropped is still marked, and (e) so is an
/// equity whose seed tick was dropped. In every case the chain's bars never
/// carry more volume than the stored candles.
#[test]
fn test_regression_a_seed_tick_after_a_loss_is_never_marked_unknown() {
    let equity = InstrumentKey {
        security_id: 2885,
        exchange_segment: 1,
    };
    let option = InstrumentKey {
        security_id: 52_175,
        exchange_segment: 2,
    };
    let r = 300_000_000;
    let pre_open = vec![
        (OPEN - 420, r, 100.0_f32, 100_000_u32),
        (OPEN, r, 100.0, 120_000),
        (OPEN + 10, r, 100.5, 121_000),
        (OPEN + 70, r, 101.0, 122_000),
    ];
    let no_pre_open = pre_open[1..].to_vec();
    let mid = OPEN + 3_600;
    let mid_ticks = vec![
        (mid + 100, r, 100.0_f32, 50_000_u32),
        (mid + 110, r, 100.5, 50_500),
        (mid + 170, r, 101.0, 51_000),
        (mid + 240, r, 101.5, 51_500),
    ];
    type Case = (
        &'static str,
        InstrumentKey,
        Vec<(i64, i64, f32, u32)>,
        Vec<usize>,
        i64,
        bool,
    );
    let cases: [Case; 5] = [
        (
            "equity after auction",
            equity,
            pre_open.clone(),
            vec![],
            OPEN - 600,
            false,
        ),
        (
            "equity, no pre-open",
            equity,
            no_pre_open,
            vec![],
            OPEN - 600,
            false,
        ),
        (
            "mid-session seed",
            option,
            mid_ticks.clone(),
            vec![],
            mid,
            false,
        ),
        (
            "mid-session seed dropped",
            option,
            mid_ticks,
            vec![0],
            mid,
            true,
        ),
        (
            "equity seed dropped",
            equity,
            pre_open,
            vec![1],
            OPEN - 600,
            true,
        ),
    ];
    for (name, key, ticks, dropped, start, flagged) in cases {
        let (fold, chain, c) = fold_and_chain_with(key, &ticks, &dropped, start, 3);
        assert_eq!(c.volume_unknown[0], flagged, "{name}");
        // A mid-session boot's first bars are withheld by the fold (partial)
        // and refused by the chain's start taint; compare the stored ones.
        for (open, (v, _)) in &chain {
            if let Some((stored, _)) = fold.get(open) {
                assert!(v <= stored, "{name}: chain {chain:?} fold {fold:?}");
            }
        }
        assert!(!fold.is_empty(), "{name}: the fold stored bars: {fold:?}");
        if !flagged {
            assert_eq!(c.counters().volume_unknown_marked, 0, "{name}");
        }
    }
}

/// LF-9 review (second pass): the tap ring drops the very tick on which the
/// fold re-anchored a vendor counter restart. The chain never saw its
/// `cumulative_restarted`, so it added the opening tick of each frame's next
/// bar where the fold adds nothing, and its bar read MORE volume than the
/// stored candle while outside the drop taint. The next tapped tick (below
/// the last tapped cumulative, after a loss, not stale) now carries the
/// restart, and the instrument's day volume is marked unknown.
#[test]
fn test_regression_a_dropped_counter_restart_tick_never_lengthens_a_chain_bar() {
    let option = InstrumentKey {
        security_id: 52_175,
        exchange_segment: 2,
    };
    let b = OPEN + 600;
    let r = 300_000_000;
    let ticks = [
        (b, r, 100.0_f32, 3_000_000_000_u32),
        (b + 20, r, 100.0, 3_000_000_100),
        (b + 40, r, 100.0, 1_000), // the restart; the tap drops it
        (b + 50, r, 100.0, 1_200),
        (b + 70, r, 100.0, 1_300),
        (b + 125, r, 100.0, 1_400),
        (b + 185, r, 100.0, 1_500),
    ];
    let (fold, chain, c) = fold_and_chain_with(option, &ticks, &[2], OPEN - 600, 0);
    assert!(c.volume_unknown[0], "the day volume is unknown");
    let next = b + 60;
    assert_eq!(
        chain.get(&next).map(|v| v.0),
        fold.get(&next).map(|v| v.0),
        "chain {chain:?} fold {fold:?}"
    );
    for (open, (v, _)) in &chain {
        let stored = fold.get(open).map_or(0, |x| x.0);
        assert!(*v <= stored, "chain {chain:?} fold {fold:?}");
    }
}

/// LF-9 review, finding 5: a day roll onto yesterday's setup leaves the
/// chain deciding nothing, and the thread's own pass step now sets the
/// engine-configured gauge to 0 at that edge (it read 1 all day before).
/// Local `/metrics` only.
#[test]
fn test_regression_a_stale_setup_day_roll_sets_the_engine_gauge_to_zero_note_inert() {
    use crate::live_fill::{CounterPublisher, ENGINE_CONFIGURED_GAUGE, pass_step};
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        let mut c = chain(Recorder::default(), true);
        metrics::gauge!(ENGINE_CONFIGURED_GAUGE).set(1.0);
        let mut p = CounterPublisher::new(c.counters(), c.taints_added());
        let now = OPEN * NS;
        pass_step(&mut c, &mut p, now, now - DAY * NS, false).expect("pass");
        let r = handle.render();
        assert!(r.contains("tv_live_fill_engine_configured 1"), "{r}");
        let next = now + 86_400 * NS;
        pass_step(&mut c, &mut p, next, next - DAY * NS, false).expect("roll");
        assert!(c.awaiting_day_setup());
        let r = handle.render();
        assert!(r.contains("tv_live_fill_engine_configured 0"), "{r}");
    });
}

/// LF-9 hardening: a `u32` wrap of the vendor cumulative on a TAPPED tick,
/// right after the tap's global loss count moved (a loss of ANOTHER
/// instrument, a shed, a socket mask change), is a wrap the fold counts
/// forward exactly, not a counter restart on a dropped tick. The chain reads
/// it by the fold's own wrap rule: nothing is marked `day_volume_unknown`
/// and every 1 m bar carries the fold's volume (60 ticks of 100). Before,
/// the fall read as a dropped restart: the instrument's day volume was
/// marked unknown, and its every order refused for the day.
#[test]
fn test_regression_u32_wrap_after_a_global_loss_is_not_a_dropped_restart() {
    let mut c = chain(Recorder::default(), true);
    let cum0 = u32::MAX - 5_049;
    let mut wrap_seq_seen = false;
    for i in 0..180u32 {
        let s = OPEN + i64::from(i);
        let cum = cum0.wrapping_add(i * 100);
        // The loss count moves on the wrap tick itself (another
        // instrument's tick was dropped just before it).
        let drop_seq = u32::from(i >= 51);
        if i == 51 {
            assert!(cum < cum0, "this tick wraps");
            wrap_seq_seen = true;
        }
        let mut t = tap(s, s * NS + 300_000_000, 100.0, cum, drop_seq);
        t.volume_delta = 100;
        t.first_bucket_volume_delta = 100;
        c.on_tap(&t);
        pass(&mut c, s * NS + 400_000_000);
        c.drain_outputs(|_| {});
    }
    pass(&mut c, (OPEN + 185) * NS);
    c.drain_outputs(|_| {});
    assert!(wrap_seq_seen);
    assert!(!c.volume_unknown[0], "a wrap is not a dropped restart");
    assert_eq!(c.counters().volume_unknown_marked, 0);
    for open in [OPEN, OPEN + 60, OPEN + 120] {
        let bar = c
            .engine()
            .bars
            .iter()
            .find(|b| b.open_ist_secs == open)
            .expect("minute released");
        assert_eq!(bar.volume, 6_000, "minute at {open}");
    }
}

/// The chain's wrap rule is the fold's: a fall from the top `2^28` of the
/// `u32` range into the bottom `2^28` counts forward; any other fall is a
/// restart (`None`).
#[test]
fn test_forward_cumulative_reads_a_wrap_as_the_fold_does() {
    assert_eq!(forward_cumulative(10, 25), Some(15));
    assert_eq!(forward_cumulative(7, 7), Some(0));
    assert_eq!(forward_cumulative(u32::MAX - 49, 50), Some(100));
    // A restart from mid-range: not a wrap.
    assert_eq!(forward_cumulative(10_000_000, 50), None);
    // Just outside each window: not a wrap.
    let top = u32::try_from(CUMULATIVE_WRAP_SPAN - CUMULATIVE_WRAP_WINDOW).expect("fits");
    let bottom = u32::try_from(CUMULATIVE_WRAP_WINDOW).expect("fits");
    assert_eq!(forward_cumulative(top - 1, 0), None);
    assert_eq!(forward_cumulative(top, bottom), None);
    assert_eq!(
        forward_cumulative(top, bottom - 1),
        Some(u64::from(bottom - 1) + u64::from(u32::MAX - top) + 1)
    );
}

/// LF-9 hardening (finding 12, midnight edge): the chain's setup day is the
/// day the setup was CHECKED for (`ChainSetup::trading_day`), never the day
/// of the clock the chain is started on. A setup checked at 23:59:59.999 IST
/// for day D-1 and a chain started at 00:00:00.000 on D (an evening-drain
/// restart crossing midnight) stays inert on D: no candle reaches the
/// engine and no order is held. The same setup started at 23:59:59.999 and
/// rolled over by a pass at 00:00:00.000 is inert too. Before, a chain
/// started just past midnight stamped yesterday's setup as today's and
/// decided the whole day on yesterday's universe, lot and tick sizes.
#[test]
fn test_regression_a_setup_checked_before_midnight_is_never_todays_setup() {
    let next_midnight = DAY + 86_400;
    // Started at D 00:00:00.000 on a setup for D-1.
    let c =
        LiveFillChain::new(emitting(), setup(true), params(), next_midnight * NS).expect("chain");
    assert_eq!(c.day(), DAY / 86_400 + 1);
    assert!(c.awaiting_day_setup(), "yesterday's setup at once");
    let mut c = c;
    let open2 = OPEN + 86_400;
    let mut f = Feed::new();
    f.drive(&mut c, (open2, open2 + 180, 2), rising_at(open2), 10);
    assert!(c.engine().bars.is_empty(), "no candle reaches the engine");
    assert!(c.counters().bars_unfed_stale_setup > 0);
    assert_eq!(c.held_orders(), 0);
    assert!(executions(&f.out).is_empty());
    // Started at D-1 23:59:59.999, rolled over by a pass at D 00:00:00.000.
    let mut c = LiveFillChain::new(
        emitting(),
        setup(true),
        params(),
        next_midnight * NS - 1_000_000,
    )
    .expect("chain");
    assert!(!c.awaiting_day_setup(), "still D-1");
    pass(&mut c, next_midnight * NS);
    assert!(c.awaiting_day_setup(), "the roll leaves it inert");
    let mut f = Feed::new();
    f.drive(&mut c, (open2, open2 + 180, 2), rising_at(open2), 10);
    assert!(c.engine().bars.is_empty());
    assert_eq!(c.held_orders(), 0);
    assert!(executions(&f.out).is_empty());
    // A setup checked for D decides on D.
    let mut today = setup(true);
    today.trading_day = DAY / 86_400 + 1;
    let mut c = LiveFillChain::new(emitting(), today, params(), next_midnight * NS).expect("chain");
    assert!(!c.awaiting_day_setup());
    let mut f = Feed::new();
    f.drive(&mut c, (open2, open2 + 120, 1), rising_at(open2), 1);
    assert_eq!(c.held_orders(), 1, "decides on today's setup");
}

/// [`rising`] for a session opening at `open`.
fn rising_at(open: i64) -> impl Fn(i64) -> f32 {
    move |s| if s < open + 60 { 100.0 } else { 101.0 }
}
