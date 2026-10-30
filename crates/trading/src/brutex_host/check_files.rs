//! The three check files (contract §3.3). Strict CSV: header row, comma, no
//! quotes, `.` decimal. Floats are kept as parsed `f64` and compared bit for bit.
//! O(rows) once per load, cold.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use super::{InstrumentKey, Refusal};

#[derive(Debug, Clone, PartialEq)]
pub struct CheckBar {
    pub row: usize,
    pub ts_ist: String,
    pub key: InstrumentKey,
    pub timeframe: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckValues {
    pub columns: Vec<String>,
    pub rows: Vec<ValueRowExpected>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValueRowExpected {
    pub row: usize,
    pub ts_ist: String,
    pub key: InstrumentKey,
    pub timeframe: String,
    pub values: Vec<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    None,
    Entry,
    Exit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignalRow {
    pub row: usize,
    pub ts_ist: String,
    pub key: InstrumentKey,
    pub timeframe: String,
    pub signal: SignalKind,
    pub side: String,
    pub qty_lots: u32,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub target: f64,
}

const CANDLE_HEADER: &str =
    "ts_ist,security_id,exchange_segment,timeframe,open,high,low,close,volume";
const SIGNAL_HEADER: &str = "ts_ist,security_id,exchange_segment,timeframe,signal,side,qty_lots,entry_price,stop_loss,target";
const VALUE_PREFIX: &str = "ts_ist,security_id,exchange_segment,timeframe";

fn bad(name: &str, row: usize, detail: impl Into<String>) -> Refusal {
    Refusal::BadCheckFile {
        name: name.to_owned(),
        row,
        detail: detail.into(),
    }
}

fn lines<'a>(name: &str, bytes: &'a [u8]) -> Result<(&'a str, Vec<(usize, &'a str)>), Refusal> {
    let text = std::str::from_utf8(bytes).map_err(|_| bad(name, 0, "not UTF-8"))?;
    let mut it = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty());
    let (_, header) = it.next().ok_or_else(|| bad(name, 0, "no header row"))?;
    let header = header.trim_end_matches('\r');
    Ok((
        header,
        it.map(|(i, l)| (i + 1, l.trim_end_matches('\r'))).collect(),
    ))
}

fn f(name: &str, row: usize, field: &str, s: &str) -> Result<f64, Refusal> {
    let v: f64 = s
        .parse()
        .map_err(|_| bad(name, row, format!("{field} {s:?} is not a number")))?;
    if !v.is_finite() {
        return Err(bad(name, row, format!("{field} is not finite")));
    }
    Ok(v)
}

fn key_ts(name: &str, row: usize, c: &[&str]) -> Result<(String, InstrumentKey, String), Refusal> {
    let ts = c[0];
    if chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S").is_err() {
        return Err(bad(name, row, format!("ts_ist {ts:?}")));
    }
    let security_id = c[1]
        .parse::<u64>()
        .map_err(|_| bad(name, row, "security_id"))?;
    let exchange_segment = c[2]
        .parse::<u8>()
        .map_err(|_| bad(name, row, "exchange_segment"))?;
    if c[3].is_empty() {
        return Err(bad(name, row, "timeframe empty"));
    }
    Ok((
        ts.to_owned(),
        InstrumentKey {
            security_id,
            exchange_segment,
        },
        c[3].to_owned(),
    ))
}

pub fn parse_candles(name: &str, bytes: &[u8]) -> Result<Vec<CheckBar>, Refusal> {
    let (header, rows) = lines(name, bytes)?;
    if header != CANDLE_HEADER {
        return Err(bad(name, 0, format!("header {header:?}")));
    }
    if rows.is_empty() {
        return Err(Refusal::EmptyCandles);
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut prev: Option<(InstrumentKey, String, String)> = None;
    for (row, line) in rows {
        let c: Vec<&str> = line.split(',').collect();
        if c.len() != 9 {
            return Err(bad(name, row, format!("{} columns, want 9", c.len())));
        }
        let (ts_ist, key, timeframe) = key_ts(name, row, &c)?;
        let (open, high, low, close) = (
            f(name, row, "open", c[4])?,
            f(name, row, "high", c[5])?,
            f(name, row, "low", c[6])?,
            f(name, row, "close", c[7])?,
        );
        if !(low <= open && open <= high && low <= close && close <= high) {
            return Err(bad(name, row, "low <= open, close <= high does not hold"));
        }
        let volume = c[8].parse::<u64>().map_err(|_| bad(name, row, "volume"))?;
        // Per instrument and timeframe, time must strictly rise (a duplicate or
        // out-of-order bar would replay differently from the backtest).
        if let Some((pk, ptf, pts)) = &prev
            && *pk == key
            && *ptf == timeframe
            && *pts >= ts_ist
        {
            return Err(bad(name, row, "bar time does not rise for this instrument"));
        }
        prev = Some((key, timeframe.clone(), ts_ist.clone()));
        out.push(CheckBar {
            row,
            ts_ist,
            key,
            timeframe,
            open,
            high,
            low,
            close,
            volume,
        });
    }
    Ok(out)
}

pub fn parse_values(name: &str, bytes: &[u8]) -> Result<CheckValues, Refusal> {
    let (header, rows) = lines(name, bytes)?;
    let rest = header
        .strip_prefix(VALUE_PREFIX)
        .ok_or_else(|| bad(name, 0, format!("header {header:?}")))?;
    let columns: Vec<String> = rest.split(',').skip(1).map(str::to_owned).collect();
    if columns.is_empty() || columns.iter().any(String::is_empty) || !rest.starts_with(',') {
        return Err(bad(name, 0, "no value columns"));
    }
    let want = 4 + columns.len();
    let mut out = Vec::with_capacity(rows.len());
    for (row, line) in rows {
        let c: Vec<&str> = line.split(',').collect();
        if c.len() != want {
            return Err(bad(name, row, format!("{} columns, want {want}", c.len())));
        }
        let (ts_ist, key, timeframe) = key_ts(name, row, &c)?;
        let mut values = Vec::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            values.push(f(name, row, col, c[4 + i])?);
        }
        out.push(ValueRowExpected {
            row,
            ts_ist,
            key,
            timeframe,
            values,
        });
    }
    Ok(CheckValues { columns, rows: out })
}

pub fn parse_signals(name: &str, bytes: &[u8]) -> Result<Vec<SignalRow>, Refusal> {
    let (header, rows) = lines(name, bytes)?;
    if header != SIGNAL_HEADER {
        return Err(bad(name, 0, format!("header {header:?}")));
    }
    let mut out = Vec::with_capacity(rows.len());
    for (row, line) in rows {
        let c: Vec<&str> = line.split(',').collect();
        if c.len() != 10 {
            return Err(bad(name, row, format!("{} columns, want 10", c.len())));
        }
        let (ts_ist, key, timeframe) = key_ts(name, row, &c)?;
        let signal = match c[4] {
            "none" => SignalKind::None,
            "entry" => SignalKind::Entry,
            "exit" => SignalKind::Exit,
            other => return Err(bad(name, row, format!("signal {other:?}"))),
        };
        let side = c[5].to_owned();
        if !matches!(side.as_str(), "" | "buy" | "sell") {
            return Err(bad(name, row, format!("side {side:?}")));
        }
        let qty_lots = c[6]
            .parse::<u32>()
            .map_err(|_| bad(name, row, "qty_lots"))?;
        out.push(SignalRow {
            row,
            ts_ist,
            key,
            timeframe,
            signal,
            side,
            qty_lots,
            entry_price: f(name, row, "entry_price", c[7])?,
            stop_loss: f(name, row, "stop_loss", c[8])?,
            target: f(name, row, "target", c[9])?,
        });
    }
    Ok(out)
}
// O(1) EXEMPT: end
