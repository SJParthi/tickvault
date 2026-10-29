//! The replay harness: feed the check candles through the strategy engine and
//! compare every value and every signal with the check files, bar by bar, bit
//! for bit. The first difference names the file, row, bar time, instrument,
//! field, expected and got. O(bars × value columns) once per load, cold.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use super::check_files::{CheckBar, CheckValues, SignalKind, SignalRow};
use super::engine::{BarInput, IntentKind, IntentSink, Side, StrategyEngine};
use super::recipe::{CHECK_SIGNALS, CHECK_VALUES};
use super::{InstrumentKey, Refusal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub file: String,
    pub row: usize,
    pub ts_ist: String,
    pub key: Option<InstrumentKey>,
    pub field: String,
    pub expected: String,
    pub got: String,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = self.key.map_or_else(
            || "-".to_owned(),
            |k| format!("{}/{}", k.security_id, k.exchange_segment),
        );
        write!(
            f,
            "{} row {} bar {} instrument {} field {}: expected {}, got {}",
            self.file, self.row, self.ts_ist, key, self.field, self.expected, self.got
        )
    }
}

/// Per-replay totals for the audit line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayReport {
    pub bars: usize,
    pub values_checked: usize,
    pub signals_checked: usize,
}

fn mm(
    file: &str,
    row: usize,
    ts: &str,
    key: Option<InstrumentKey>,
    field: &str,
    expected: impl ToString,
    got: impl ToString,
) -> Refusal {
    Refusal::Replay(Box::new(Mismatch {
        file: file.to_owned(),
        row,
        ts_ist: ts.to_owned(),
        key,
        field: field.to_owned(),
        expected: expected.to_string(),
        got: got.to_string(),
    }))
}

/// Bit-for-bit float equality (`0.0` and `-0.0` differ; the contract says so).
#[must_use]
pub fn same_bits(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits()
}

pub fn replay<E: StrategyEngine + ?Sized>(
    engine: &mut E,
    timeframes: &[String],
    candles: &[CheckBar],
    values: &CheckValues,
    signals: &[SignalRow],
) -> Result<ReplayReport, Refusal> {
    if candles.is_empty() {
        return Err(Refusal::EmptyCandles);
    }
    if engine.value_columns() != values.columns.as_slice() {
        return Err(mm(
            CHECK_VALUES,
            0,
            "-",
            None,
            "header",
            values.columns.join("|"),
            engine.value_columns().join("|"),
        ));
    }
    let mut sink = IntentSink::with_capacity(8);
    let mut got_values = vec![0.0_f64; values.columns.len()];
    let mut report = ReplayReport::default();
    let n = candles.len();
    for (i, bar) in candles.iter().enumerate() {
        let tf = timeframes
            .iter()
            .position(|t| *t == bar.timeframe)
            .ok_or_else(|| Refusal::BadCheckFile {
                name: "check_candles.csv".to_owned(),
                row: bar.row,
                detail: format!("timeframe {} not in the recipe", bar.timeframe),
            })?;
        let tf = u8::try_from(tf)
            .map_err(|_| Refusal::BadRecipe("more than 255 timeframes".to_owned()))?;
        let open_ist_secs = chrono::NaiveDateTime::parse_from_str(&bar.ts_ist, "%Y-%m-%dT%H:%M:%S")
            .map(|t| t.and_utc().timestamp())
            .map_err(|_| Refusal::BadCheckFile {
                name: "check_candles.csv".to_owned(),
                row: bar.row,
                detail: "ts_ist".to_owned(),
            })?;
        sink.clear();
        engine.on_bar(
            &BarInput {
                key: bar.key,
                timeframe: tf,
                open_ist_secs,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
            },
            &mut sink,
        );
        report.bars += 1;

        // Values: one row per bar, same order.
        let Some(exp) = values.rows.get(i) else {
            return Err(mm(
                CHECK_VALUES,
                n + 1,
                &bar.ts_ist,
                Some(bar.key),
                "row count",
                n,
                values.rows.len(),
            ));
        };
        if exp.ts_ist != bar.ts_ist || exp.key != bar.key || exp.timeframe != bar.timeframe {
            return Err(mm(
                CHECK_VALUES,
                exp.row,
                &bar.ts_ist,
                Some(bar.key),
                "bar alignment",
                format!("{} {}", bar.ts_ist, bar.timeframe),
                format!("{} {}", exp.ts_ist, exp.timeframe),
            ));
        }
        if !engine.bar_values(bar.key, tf, &mut got_values) {
            return Err(mm(
                CHECK_VALUES,
                exp.row,
                &bar.ts_ist,
                Some(bar.key),
                "values",
                "a value row",
                "none",
            ));
        }
        for (c, (e, g)) in exp.values.iter().zip(got_values.iter()).enumerate() {
            if !same_bits(*e, *g) {
                return Err(mm(
                    CHECK_VALUES,
                    exp.row,
                    &bar.ts_ist,
                    Some(bar.key),
                    &values.columns[c],
                    e,
                    g,
                ));
            }
            report.values_checked += 1;
        }

        // Signals: one row per bar, same order.
        let Some(s) = signals.get(i) else {
            return Err(mm(
                CHECK_SIGNALS,
                n + 1,
                &bar.ts_ist,
                Some(bar.key),
                "row count",
                n,
                signals.len(),
            ));
        };
        if s.ts_ist != bar.ts_ist || s.key != bar.key || s.timeframe != bar.timeframe {
            return Err(mm(
                CHECK_SIGNALS,
                s.row,
                &bar.ts_ist,
                Some(bar.key),
                "bar alignment",
                format!("{} {}", bar.ts_ist, bar.timeframe),
                format!("{} {}", s.ts_ist, s.timeframe),
            ));
        }
        compare_signal(s, sink.intents())?;
        report.signals_checked += 1;
    }
    if values.rows.len() != n {
        let r = values.rows.get(n).map_or(0, |r| r.row);
        return Err(mm(
            CHECK_VALUES,
            r,
            "-",
            None,
            "row count",
            n,
            values.rows.len(),
        ));
    }
    if signals.len() != n {
        let r = signals.get(n).map_or(0, |r| r.row);
        return Err(mm(
            CHECK_SIGNALS,
            r,
            "-",
            None,
            "row count",
            n,
            signals.len(),
        ));
    }
    Ok(report)
}

fn compare_signal(s: &SignalRow, got: &[super::engine::OrderIntent]) -> Result<(), Refusal> {
    let file = CHECK_SIGNALS;
    let k = Some(s.key);
    match (s.signal, got) {
        (SignalKind::None, []) => Ok(()),
        (SignalKind::None, [g, ..]) => Err(mm(
            file,
            s.row,
            &s.ts_ist,
            k,
            "signal",
            "none",
            kind_name(g.kind),
        )),
        (_, []) => Err(mm(
            file,
            s.row,
            &s.ts_ist,
            k,
            "signal",
            signal_name(s.signal),
            "none",
        )),
        (_, [_, _, ..]) => Err(mm(file, s.row, &s.ts_ist, k, "signal count", 1, got.len())),
        (want, [g]) => {
            let want_kind = if want == SignalKind::Entry {
                IntentKind::Entry
            } else {
                IntentKind::Exit
            };
            if g.kind != want_kind {
                return Err(mm(
                    file,
                    s.row,
                    &s.ts_ist,
                    k,
                    "signal",
                    signal_name(want),
                    kind_name(g.kind),
                ));
            }
            let side = match g.side {
                Side::Buy => "buy",
                Side::Sell => "sell",
            };
            if s.side != side {
                return Err(mm(file, s.row, &s.ts_ist, k, "side", &s.side, side));
            }
            if s.qty_lots != g.qty_lots {
                return Err(mm(
                    file, s.row, &s.ts_ist, k, "qty_lots", s.qty_lots, g.qty_lots,
                ));
            }
            for (field, e, gv) in [
                ("entry_price", s.entry_price, g.entry_price),
                ("stop_loss", s.stop_loss, g.stop_loss),
                ("target", s.target, g.target),
            ] {
                if !same_bits(e, gv) {
                    return Err(mm(file, s.row, &s.ts_ist, k, field, e, gv));
                }
            }
            Ok(())
        }
    }
}

fn kind_name(k: IntentKind) -> &'static str {
    match k {
        IntentKind::Entry => "entry",
        IntentKind::Exit => "exit",
    }
}

fn signal_name(s: SignalKind) -> &'static str {
    match s {
        SignalKind::None => "none",
        SignalKind::Entry => "entry",
        SignalKind::Exit => "exit",
    }
}
// O(1) EXEMPT: end
