//! The replay's TEST FIXTURE intent source (plan item LF-8): a fixed
//! schedule of "on this instrument's bar of this size that opens at this
//! time, ask for this side and size". It is NOT a strategy: it never reads a
//! price, a volume or an indicator, so it cannot decide anything; it only
//! puts orders on known candles so the replay can measure the timing and the
//! worst-case fill model around them. Rule `brutex-strategy-host-2026-10-29.md`:
//! tickvault has no strategy of its own, and this module adds none.
//!
//! Never on the live path: `live_fill::select_engine` cannot return it and a
//! source scan (`live_fill_replay_isolation_guard.rs`) keeps it out of every
//! other module.

// O(1) EXEMPT: begin: cold offline CLI, built once per replay; `on_bar` is one hash probe plus the rules of one (instrument, frame).
use std::collections::HashMap;

use serde::Deserialize;
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::brutex_host::engine::{
    BarInput, IntentKind, IntentSink, OrderIntent, Side, StrategyEngine, TickInput,
};
use tickvault_trading::brutex_host::recipe::{ParamSnapshot, Recipe};
use tickvault_trading::brutex_host::{InstrumentKey, Refusal};

const SECS_PER_DAY: i64 = 86_400;

/// One scheduled ask.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    /// The bar's open, IST seconds of day; `None` = every bar of the frame.
    pub bar_open_sod: Option<i64>,
    pub side: Side,
    pub lots: u32,
    pub kind: IntentKind,
}

/// See the module docs.
#[derive(Debug, Clone, Default)]
pub struct ScriptedIntents {
    /// Per (instrument, recipe timeframe index): the rules, in file order.
    rules: HashMap<(InstrumentKey, u8), Vec<Rule>>,
    next_id: u64,
    no_columns: Vec<String>,
}

impl ScriptedIntents {
    /// Rules held (all instruments and frames). O(keys).
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.values().map(Vec::len).sum()
    }

    /// The instruments the schedule names. O(keys).
    #[must_use]
    pub fn instruments(&self) -> Vec<InstrumentKey> {
        let mut v: Vec<InstrumentKey> = self.rules.keys().map(|(k, _)| *k).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

impl StrategyEngine for ScriptedIntents {
    fn configure(
        &mut self,
        _recipe: &Recipe,
        _params: Option<&ParamSnapshot>,
        universe: &[InstrumentKey],
    ) -> Result<(), Refusal> {
        for (key, _) in self.rules.keys() {
            if !universe.contains(key) {
                return Err(Refusal::Engine(format!(
                    "scheduled instrument {}/{} is not in the universe",
                    key.security_id, key.exchange_segment
                )));
            }
        }
        self.next_id = 0;
        Ok(())
    }

    fn on_tick(&mut self, _tick: &TickInput, _out: &mut IntentSink) {}

    /// Reads the bar's key, frame and open time only: never a price.
    fn on_bar(&mut self, bar: &BarInput, out: &mut IntentSink) {
        let Some(rules) = self.rules.get(&(bar.key, bar.timeframe)) else {
            return;
        };
        let sod = bar.open_ist_secs.rem_euclid(SECS_PER_DAY);
        for r in rules {
            if r.bar_open_sod.is_some_and(|s| s != sod) {
                continue;
            }
            self.next_id = self.next_id.saturating_add(1);
            let _kept = out.push(OrderIntent {
                key: bar.key,
                timeframe: bar.timeframe,
                kind: r.kind,
                side: r.side,
                qty_lots: r.lots,
                entry_price: 0.0,
                stop_loss: 0.0,
                target: 0.0,
                intent_id: self.next_id,
                at_ist_secs: bar.open_ist_secs,
            });
        }
    }

    fn value_columns(&self) -> &[String] {
        &self.no_columns
    }

    fn bar_values(&self, _key: InstrumentKey, _timeframe: u8, _out: &mut [f64]) -> bool {
        false
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleFile {
    intent: Vec<IntentLine>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentLine {
    security_id: u64,
    segment: String,
    frame_secs: u32,
    #[serde(default)]
    bar_open_ist: Option<String>,
    #[serde(default)]
    every_bar: bool,
    side: String,
    lots: u32,
    kind: String,
}

/// The segment named by its wire text (`NSE_EQ`, `NSE_FNO`, `IDX_I`, ...).
/// O(segments).
#[must_use]
pub fn parse_segment(text: &str) -> Option<ExchangeSegment> {
    (0u8..=16)
        .filter_map(ExchangeSegment::from_byte)
        .find(|s| s.as_str() == text)
}

/// `HH:MM:SS` to seconds of day. O(1).
fn parse_sod(text: &str) -> Option<i64> {
    let t = chrono::NaiveTime::parse_from_str(text, "%H:%M:%S").ok()?;
    Some(i64::from(chrono::Timelike::num_seconds_from_midnight(&t)))
}

/// Whether a bar of `frame_secs` can open at `sod` (IST seconds of day) and
/// reach the strategy: on the frame's grid (bars are cut from IST midnight,
/// as the candle book cuts them) and inside the session (open at or after
/// 09:15:00, end at or before 15:30:00, `timing::bar_in_session`). A rule
/// that fails this could never fire, and a replay with it would read as a
/// clean day with no orders. O(1).
///
/// # Errors
/// Why the bar can never be decided on, as text.
pub fn check_bar_open(sod: i64, frame_secs: u32) -> Result<(), String> {
    let size = i64::from(frame_secs.max(1));
    if sod.rem_euclid(size) != 0 {
        return Err(format!(
            "bar_open_ist is not on the {frame_secs} s bar grid (bars open at multiples of \
             {frame_secs} s from midnight), so no bar ever opens then"
        ));
    }
    if !crate::live_fill::timing::bar_in_session(sod, sod.saturating_add(size)) {
        return Err(
            "bar_open_ist is outside the session (a bar must open at or after 09:15:00 and \
             end by 15:30:00), so no bar is ever decided on then"
                .to_owned(),
        );
    }
    Ok(())
}

/// Whether an `every_bar` rule of `frame_secs` can ever fire: the first bar
/// of the frame's grid (cut from IST midnight) that opens at or after 09:15:00
/// must also end by 15:30:00 ([`check_bar_open`] on it). A frame no bar of
/// which lies inside the session (a 6-hour frame: 06:00-12:00 opens too early,
/// 12:00-18:00 ends too late) is refused instead of replaying to a silent day
/// with no orders. O(1).
///
/// # Errors
/// Why no bar of the frame is ever decided on, as text.
pub fn check_every_bar(frame_secs: u32) -> Result<(), String> {
    let size = i64::from(frame_secs.max(1));
    let first_open = crate::live_fill::timing::SESSION_OPEN_SOD
        .saturating_add(size - 1)
        .div_euclid(size)
        .saturating_mul(size);
    check_bar_open(first_open, frame_secs).map_err(|_| {
        format!(
            "every_bar with frame_secs = {frame_secs}: no bar of this frame opens at or after \
             09:15:00 and ends by 15:30:00, so no bar is ever decided on"
        )
    })
}

/// Parses a schedule file (TOML, `[[intent]]` tables; see the module docs of
/// `live_fill::replay`). Returns the frames in first-use order (the recipe's
/// timeframes) and the intent source. Unknown keys, a bad time, side, kind or
/// segment, `lots = 0`, `frame_secs = 0`, a `bar_open_ist` no bar can open
/// at ([`check_bar_open`]), an `every_bar` frame with no bar inside the
/// session ([`check_every_bar`]), or neither / both of `bar_open_ist` and
/// `every_bar` are refused. O(lines).
///
/// # Errors
/// The first problem found, as text.
pub fn parse_schedule(text: &str) -> Result<(Vec<u32>, ScriptedIntents), String> {
    use figment::providers::{Format, Toml};
    let file: ScheduleFile = figment::Figment::new()
        .merge(Toml::string(text))
        .extract()
        .map_err(|e| format!("schedule: {e}"))?;
    if file.intent.is_empty() {
        return Err("schedule: no [[intent]] lines".to_owned());
    }
    let mut frames: Vec<u32> = Vec::new();
    let mut s = ScriptedIntents::default();
    for (i, l) in file.intent.iter().enumerate() {
        let at = |m: &str| format!("schedule intent {}: {m}", i + 1);
        if l.frame_secs == 0 {
            return Err(at("frame_secs must be above 0"));
        }
        if l.lots == 0 {
            return Err(at("lots must be above 0"));
        }
        let segment = parse_segment(&l.segment).ok_or_else(|| at("unknown segment"))?;
        let side = match l.side.as_str() {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => return Err(at("side must be BUY or SELL")),
        };
        let kind = match l.kind.as_str() {
            "entry" => IntentKind::Entry,
            "exit" => IntentKind::Exit,
            _ => return Err(at("kind must be entry or exit")),
        };
        let bar_open_sod = match (&l.bar_open_ist, l.every_bar) {
            (Some(t), false) => {
                let sod = parse_sod(t).ok_or_else(|| at("bad bar_open_ist, want HH:MM:SS"))?;
                check_bar_open(sod, l.frame_secs).map_err(|m| at(&m))?;
                Some(sod)
            }
            (None, true) => {
                check_every_bar(l.frame_secs).map_err(|m| at(&m))?;
                None
            }
            _ => return Err(at("give exactly one of bar_open_ist or every_bar = true")),
        };
        let tf = match frames.iter().position(|&f| f == l.frame_secs) {
            Some(p) => p,
            None => {
                frames.push(l.frame_secs);
                frames.len() - 1
            }
        };
        let tf = u8::try_from(tf).map_err(|_| at("more than 255 frames"))?;
        let key = InstrumentKey {
            security_id: l.security_id,
            exchange_segment: segment.binary_code(),
        };
        s.rules.entry((key, tf)).or_default().push(Rule {
            bar_open_sod,
            side,
            lots: l.lots,
            kind,
        });
    }
    Ok((frames, s))
}
// O(1) EXEMPT: end
