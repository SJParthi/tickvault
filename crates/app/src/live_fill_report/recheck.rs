//! Re-prices one paper order from the stored 1 s candles with the chain's own
//! pricing (`live_fill::fill_window::price_held_window`), plan item LF-7.
//!
//! The chain priced each order from its own 1 s bars; this re-pricing reads
//! the stored `candles_1s` of the same seconds. A late tick the chain never
//! saw can widen a stored bar, so a difference is a finding to read, not
//! proof of a defect.
//!
//! Cold, once a day, at most [`RECHECK_MAX_ORDERS`] orders and
//! [`RECHECK_WALL_BUDGET_SECS`] in all (flagged: not O(1)). Each order is one
//! query whose REPLY is at most `max_fill_seconds + 3` rows, but whose COST
//! on QuestDB is larger (LF-7 review finding 8): in `candles_1s`
//! `security_id` is an unindexed LONG and `segment` / `feed` are
//! low-cardinality symbols, so QuestDB finds the time slice
//! `[decision_sec, horizon_end]` and then filters EVERY instrument's 1 s rows
//! in it, O(instruments that traded in the window × window seconds) per
//! order (Assumed, not measured: about 1e5 to 3e5 rows for a 62 s window,
//! about 3e6 at the 600 s ceiling). The 10 s client timeout drops the
//! request but does not cancel the query on the server, so a slow read still
//! costs QuestDB its whole scan; only the wall budget bounds the total. The
//! pricing itself is O(seconds) per order.

use std::time::Duration;

use tickvault_common::config::LIVE_FILL_MAX_FILL_SECONDS_CEILING;
use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::brutex_host::worst_fill::{
    BPS_DENOMINATOR, FillSchedule, SecondBar, rupees_to_paise,
};

use super::rows::{EventRow, json_i64, parse_ts_nanos};
use crate::live_fill::fill_window::{HeldPricing, beyond_limit, price_held_window};

/// Most orders re-priced per report; later orders read `recheck_skipped`.
pub const RECHECK_MAX_ORDERS: usize = 500;

/// Wall budget for all re-pricing of one report.
pub const RECHECK_WALL_BUDGET_SECS: u64 = 120;

/// One order's second read bound.
pub const RECHECK_QUERY_TIMEOUT_SECS: u64 = 10;

/// Fill-second offsets the histogram keeps (0..=60); later seconds fold into
/// the last bucket.
pub const FILL_OFFSET_BUCKETS: usize = 61;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// What the chain priced an order from, read off its Decision row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderInputs {
    pub signal_id: i64,
    pub security_id: i64,
    pub segment: String,
    pub side: Side,
    /// Shares.
    pub qty: u64,
    pub tick_paise: i64,
    pub lot_size: u64,
    pub participation_bps: u32,
    pub max_fill_seconds: u32,
    pub limit_paise: i64,
    /// The candle's last second (IST epoch seconds).
    pub decision_sec: i64,
    /// The candle's close, paise (the decision second's price when nothing
    /// traded in it).
    pub ltp_paise: i64,
    /// Best price and quantity on our side at the decision.
    pub book: Option<(i64, u64)>,
}

impl OrderInputs {
    /// The inputs of a Decision row, or `None` when any pricing input is
    /// missing (a row from before LF-7, or an order refused before pricing).
    /// O(1).
    #[must_use]
    pub fn from_decision(row: &EventRow) -> Option<Self> {
        let side = match row.side.as_str() {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => return None,
        };
        let qty = u64::try_from(row.qty).ok().filter(|q| *q > 0)?;
        let tick_paise = row.tick_paise.filter(|t| *t > 0)?;
        let lot_size = row
            .lot_size
            .and_then(|l| u64::try_from(l).ok())
            .filter(|l| *l > 0)?;
        let participation_bps = row
            .participation_bps
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| *p > 0 && *p <= BPS_DENOMINATOR)?;
        let max_fill_seconds = row
            .max_fill_seconds
            .and_then(|m| u32::try_from(m).ok())
            // The config ceiling: the value sizes the fill schedule and the
            // read LIMIT, so a stored value past it is refused, never trusted.
            .filter(|m| (1..=LIVE_FILL_MAX_FILL_SECONDS_CEILING).contains(m))?;
        let limit_paise = row.limit_paise.filter(|l| *l > 0)?;
        let decision_sec = row
            .candle_end_ns?
            .div_euclid(NANOS_PER_SEC)
            .checked_sub(1)?;
        let ltp_paise = row.ltp_paise.filter(|p| *p > 0)?;
        let (price, size) = match side {
            Side::Buy => (row.best_ask_paise, row.best_ask_qty),
            Side::Sell => (row.best_bid_paise, row.best_bid_qty),
        };
        let book = match (price, size) {
            (Some(p), Some(q)) if p > 0 && q > 0 => Some((p, u64::try_from(q).ok()?)),
            _ => None,
        };
        Some(Self {
            signal_id: row.signal_id,
            security_id: row.security_id,
            segment: row.segment.clone(),
            side,
            qty,
            tick_paise,
            lot_size,
            participation_bps,
            max_fill_seconds,
            limit_paise,
            decision_sec,
            ltp_paise,
            book,
        })
    }

    /// The last fill second the chain could use. O(1).
    #[must_use]
    pub fn horizon_end(&self) -> i64 {
        self.decision_sec
            .saturating_add(1)
            .saturating_add(i64::from(self.max_fill_seconds))
    }
}

/// The re-priced fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecheckKind {
    /// Priced. `filled_qty` is floored to whole lots like the chain's; the
    /// average is the full model's, as the chain books it.
    Priced {
        filled_qty: u64,
        avg_paise: Option<i64>,
        model_paise: Option<i64>,
        beyond_limit_qty: u64,
        /// Shares per fill-second offset (0 = the first second after the
        /// decision), summing to `filled_qty`.
        fill_offsets: Vec<(u32, u64)>,
    },
    /// The model refused (for example no traded second at all).
    Refused(String),
    /// The order could not be re-priced (inputs or stored seconds missing).
    Unavailable(&'static str),
    /// Not attempted (a cap was reached).
    Skipped(&'static str),
}

/// One order's re-pricing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recheck {
    /// Stored fill seconds found inside the order's window.
    pub seconds_seen: usize,
    pub kind: RecheckKind,
}

impl Recheck {
    /// A re-pricing that was not attempted or could not run.
    #[must_use]
    pub const fn not_run(kind: RecheckKind) -> Self {
        Self {
            seconds_seen: 0,
            kind,
        }
    }
}

/// Re-prices one order exactly as the chain does: the decision bar is the
/// stored second at `decision_sec` (else the candle close with no volume, as
/// `chain.rs` builds it), the fill seconds are those inside the window, cut
/// at the second after the one whose running volume share first covers the
/// quantity (the chain finishes the order there), then
/// [`price_held_window`] and the whole-lot floor. `seconds` may hold any
/// stored seconds of the instrument; they are sorted and filtered here.
/// `out` is reused (capacity at least `max_fill_seconds + 1`). O(seconds log
/// seconds).
pub fn recheck_order(
    inp: &OrderInputs,
    decision: Option<SecondBar>,
    seconds: &mut Vec<SecondBar>,
    out: &mut FillSchedule,
) -> Recheck {
    let first = inp.decision_sec.saturating_add(1);
    let horizon_end = inp.horizon_end();
    seconds.retain(|s| s.sec >= first && s.sec <= horizon_end);
    seconds.sort_unstable_by_key(|s| s.sec);
    seconds.dedup_by_key(|s| s.sec);
    let seconds_seen = seconds.len();
    // The chain finishes the order once the running share of the seconds
    // before the horizon end covers the quantity, with the next second final.
    let mut cap_sum: u64 = 0;
    let mut trigger: Option<i64> = None;
    for s in seconds.iter() {
        if s.sec >= horizon_end {
            break;
        }
        let cap =
            u128::from(s.volume) * u128::from(inp.participation_bps) / u128::from(BPS_DENOMINATOR);
        cap_sum = cap_sum.saturating_add(u64::try_from(cap).unwrap_or(u64::MAX));
        if cap_sum >= inp.qty {
            trigger = Some(s.sec);
            break;
        }
    }
    if let Some(t) = trigger {
        let last = t.saturating_add(1);
        seconds.retain(|s| s.sec <= last);
    }
    let decision_bar = decision
        .filter(|d| d.sec == inp.decision_sec)
        .unwrap_or(SecondBar {
            sec: inp.decision_sec,
            high_paise: inp.ltp_paise,
            low_paise: inp.ltp_paise,
            volume: 0,
        });
    let pricing = HeldPricing {
        side: inp.side,
        qty: inp.qty,
        tick_paise: inp.tick_paise,
        participation_bps: inp.participation_bps,
        max_fill_seconds: inp.max_fill_seconds,
        limit_paise: inp.limit_paise,
        decision_bar,
        book: inp.book,
    };
    let kind = match price_held_window(&pricing, seconds, out) {
        Err(refusal) => RecheckKind::Refused(refusal.to_string()),
        Ok((summary, beyond_limit_qty)) => {
            let lot = inp.lot_size.max(1);
            let filled_qty = (summary.filled_qty / lot) * lot;
            let mut left = filled_qty;
            let mut fill_offsets: Vec<(u32, u64)> = Vec::new();
            for slice in out.slices() {
                if left == 0 {
                    break;
                }
                if beyond_limit(inp.side, slice.price_paise, inp.limit_paise) {
                    continue;
                }
                let take = slice.qty.min(left);
                left -= take;
                let off = u32::try_from(slice.sec.saturating_sub(first).max(0))
                    .unwrap_or(u32::MAX)
                    .min(u32::try_from(FILL_OFFSET_BUCKETS - 1).unwrap_or(60));
                match fill_offsets.last_mut() {
                    Some((o, q)) if *o == off => *q = q.saturating_add(take),
                    _ => fill_offsets.push((off, take)),
                }
            }
            RecheckKind::Priced {
                filled_qty,
                avg_paise: if filled_qty > 0 {
                    summary.vwap_paise
                } else {
                    None
                },
                model_paise: summary.vwap_paise,
                beyond_limit_qty,
                fill_offsets,
            }
        }
    };
    Recheck { seconds_seen, kind }
}

/// IST epoch seconds as QuestDB timestamp text (IST written as Z). Pure.
#[must_use]
pub fn ts_text(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map_or_else(
        || "1970-01-01T00:00:00.000000Z".to_owned(),
        |d| d.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
    )
}

/// The second-read SQL for one order: its instrument's stored 1 s candles
/// from the decision second through the horizon end, a reply of at most
/// `max_s + 3` rows. Server cost: every instrument's rows in that time slice
/// are filtered (`security_id` is not indexed); see the module docs. `None` when the segment is not a plain `[A-Z_]` name (it comes from
/// the database and goes into SQL). Pure.
#[must_use]
pub fn seconds_sql(inp: &OrderInputs) -> Option<String> {
    if inp.segment.is_empty()
        || !inp
            .segment
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'_')
    {
        return None;
    }
    Some(format!(
        "SELECT ts, high, low, volume FROM candles_1s WHERE security_id = {} AND segment = '{}' \
         AND feed = 'dhan' AND ts >= '{}' AND ts <= '{}' ORDER BY ts LIMIT {}",
        inp.security_id,
        inp.segment,
        ts_text(inp.decision_sec),
        ts_text(inp.horizon_end()),
        u64::from(inp.max_fill_seconds).saturating_add(3)
    ))
}

/// Parses the [`seconds_sql`] body: `(ts, high, low, volume)` rows to
/// seconds (rupees to paise as the chain converts them; the signed candle
/// volume is taken as its magnitude). A row with a bad price is skipped.
/// O(rows).
///
/// # Errors
/// The body is not JSON or has no `dataset` array.
pub fn parse_seconds_body(body: &str) -> Result<Vec<SecondBar>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "response is not valid JSON".to_owned())?;
    let dataset = value
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "response has no `dataset` array".to_owned())?;
    let mut out = Vec::with_capacity(dataset.len());
    for raw in dataset {
        let Some(c) = raw.as_array() else {
            continue;
        };
        let (Some(ts), Some(high), Some(low), Some(vol)) = (
            c.first().and_then(parse_ts_nanos),
            c.get(1).and_then(serde_json::Value::as_f64),
            c.get(2).and_then(serde_json::Value::as_f64),
            c.get(3).and_then(json_i64),
        ) else {
            continue;
        };
        let (Ok(high_paise), Ok(low_paise)) = (rupees_to_paise(high), rupees_to_paise(low)) else {
            continue;
        };
        out.push(SecondBar {
            sec: ts.div_euclid(NANOS_PER_SEC),
            high_paise,
            low_paise,
            volume: vol.unsigned_abs(),
        });
    }
    Ok(out)
}

/// Reads one order's stored seconds through the shared probe client, under
/// [`RECHECK_QUERY_TIMEOUT_SECS`].
///
/// # Errors
/// A text reason (bad segment, client, timeout, request, status, body).
// TEST-EXEMPT: one bounded QuestDB GET; the SQL (`seconds_sql`) and the parse (`parse_seconds_body`) are unit-tested.
pub async fn read_seconds(exec_url: &str, inp: &OrderInputs) -> Result<Vec<SecondBar>, String> {
    let sql = seconds_sql(inp).ok_or_else(|| "segment not a plain name".to_owned())?;
    let client = tickvault_storage::http_client::shared_probe_client()
        .map_err(|e| format!("client build: {e}"))?;
    let limit = Duration::from_secs(RECHECK_QUERY_TIMEOUT_SECS);
    let resp = tokio::time::timeout(
        limit,
        client
            .get(exec_url)
            .timeout(limit)
            .query(&[("query", sql.as_str())])
            .send(),
    )
    .await
    .map_err(|_| "timeout".to_owned())?
    .map_err(|e| format!("request: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("status {status}"));
    }
    let body = tokio::time::timeout(limit, resp.text())
        .await
        .map_err(|_| "body timeout".to_owned())?
        .map_err(|e| format!("body: {e}"))?;
    parse_seconds_body(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_fill::fill_window::{Finished, HeldOrder, PendingBook};
    use tickvault_trading::brutex_host::InstrumentKey;

    const DEC: i64 = 1_794_441_600 + 36_000;

    fn inputs(side: Side, qty: u64, lot: u64, limit: i64) -> OrderInputs {
        OrderInputs {
            signal_id: 1,
            security_id: 2885,
            segment: "NSE_FNO".to_owned(),
            side,
            qty,
            tick_paise: 5,
            lot_size: lot,
            participation_bps: 2_500,
            max_fill_seconds: 60,
            limit_paise: limit,
            decision_sec: DEC,
            ltp_paise: 10_000,
            book: Some((10_005, 40)),
        }
    }

    fn bar(sec: i64, low: i64, high: i64, vol: u64) -> SecondBar {
        SecondBar {
            sec,
            high_paise: high,
            low_paise: low,
            volume: vol,
        }
    }

    /// Runs the chain's own `PendingBook` on `secs` (fed up to the trigger's
    /// next second, as the chain finishes it) and returns its fill.
    fn chain_fill(
        inp: &OrderInputs,
        decision: SecondBar,
        secs: &[SecondBar],
    ) -> (u64, Option<i64>) {
        let mut book = PendingBook::<u32>::new(inp.max_fill_seconds, 1);
        let order = HeldOrder {
            key_index: 0,
            key: InstrumentKey {
                security_id: 2885,
                exchange_segment: 2,
            },
            frame_index: 1,
            candle_open_secs: DEC - 59,
            candle_end_secs: DEC + 1,
            side: inp.side,
            qty: inp.qty,
            tick_paise: inp.tick_paise,
            participation_bps: inp.participation_bps,
            decision_bar: decision,
            book: inp.book,
            limit_paise: inp.limit_paise,
            meta: 0u32,
        };
        book.insert(order).expect("room");
        // Feed until the chain's trigger second's successor, like collect_ready.
        let mut cap = 0u64;
        let mut stop = inp.horizon_end();
        for s in secs {
            if s.sec > stop {
                break;
            }
            book.on_second(0, *s);
            if s.sec < inp.horizon_end() && stop == inp.horizon_end() {
                cap += s.volume * u64::from(inp.participation_bps) / 10_000;
                if cap >= inp.qty {
                    stop = s.sec + 1;
                }
            }
        }
        match book.finish(0) {
            Some(Finished::Filled { summary, .. }) => {
                let lot = inp.lot_size.max(1);
                ((summary.filled_qty / lot) * lot, summary.vwap_paise)
            }
            Some(Finished::Refused { .. }) => (0, None),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// LF-7: re-pricing the chain's own seconds gives the chain's fill and
    /// model price, including when the LIMIT binds and with an odd-share lot
    /// floor, for both sides.
    #[test]
    fn test_recheck_matches_chain_on_same_seconds() {
        let secs: Vec<SecondBar> = (1..=70)
            .map(|i| {
                bar(
                    DEC + i,
                    9_990 + (i % 7) * 5,
                    10_010 + (i % 5) * 10,
                    40 + (i as u64 % 3) * 7,
                )
            })
            .collect();
        let decision = bar(DEC, 9_995, 10_005, 30);
        for side in [Side::Buy, Side::Sell] {
            for (qty, lot, limit) in [
                (50, 25, 10_100),  // limit loose
                (300, 75, 10_030), // limit binds for a buy
                (55, 25, 9_985),   // limit binds for a sell, odd shares
                (2_000, 1, 10_050),
            ] {
                let inp = inputs(side, qty, lot, limit);
                let (chain_qty, chain_model) = chain_fill(&inp, decision, &secs);
                let mut s = secs.clone();
                let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(60, 0));
                let r = recheck_order(&inp, Some(decision), &mut s, &mut out);
                match r.kind {
                    RecheckKind::Priced {
                        filled_qty,
                        model_paise,
                        fill_offsets,
                        ..
                    } => {
                        assert_eq!(filled_qty, chain_qty, "{side:?} {qty} {lot} {limit}");
                        assert_eq!(model_paise, chain_model, "{side:?} {qty} {lot} {limit}");
                        assert_eq!(fill_offsets.iter().map(|(_, q)| q).sum::<u64>(), filled_qty);
                    }
                    RecheckKind::Refused(_) => assert_eq!(chain_qty, 0),
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
    }

    #[test]
    fn test_recheck_order_with_no_stored_seconds_is_refused_and_counts_none() {
        let inp = inputs(Side::Buy, 50, 25, 10_100);
        let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(60, 0));
        let r = recheck_order(&inp, None, &mut Vec::new(), &mut out);
        assert_eq!(r.seconds_seen, 0);
        assert!(matches!(r.kind, RecheckKind::Refused(_)), "{r:?}");
        assert_eq!(
            Recheck::not_run(RecheckKind::Skipped("cap")).seconds_seen,
            0
        );
    }

    #[test]
    fn test_order_inputs_from_decision_and_horizon_end_need_every_pricing_input() {
        let row = EventRow {
            event: "decision".into(),
            signal_id: 1,
            side: "BUY".into(),
            qty: 50,
            segment: "NSE_FNO".into(),
            security_id: 2885,
            candle_end_ns: Some((DEC + 1) * NANOS_PER_SEC),
            ltp_paise: Some(10_000),
            limit_paise: Some(10_100),
            best_ask_paise: Some(10_005),
            best_ask_qty: Some(40),
            tick_paise: Some(5),
            lot_size: Some(25),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(60),
            ..EventRow::default()
        };
        let inp = OrderInputs::from_decision(&row).expect("inputs");
        assert_eq!(inp, inputs(Side::Buy, 50, 25, 10_100));
        assert_eq!(inp.horizon_end(), DEC + 61);
        for strip in [
            |r: &mut EventRow| r.tick_paise = None,
            |r: &mut EventRow| r.lot_size = Some(0),
            |r: &mut EventRow| r.participation_bps = Some(10_001),
            |r: &mut EventRow| r.max_fill_seconds = None,
            |r: &mut EventRow| r.limit_paise = None,
            |r: &mut EventRow| r.candle_end_ns = None,
            |r: &mut EventRow| r.side = "HOLD".into(),
            |r: &mut EventRow| r.qty = 0,
        ] {
            let mut r = row.clone();
            strip(&mut r);
            assert!(OrderInputs::from_decision(&r).is_none());
        }
        let mut sell = row;
        sell.side = "SELL".into();
        assert_eq!(
            OrderInputs::from_decision(&sell).map(|i| i.book),
            Some(None)
        );
    }

    #[test]
    fn test_seconds_sql_refuses_an_unsafe_segment_and_ts_text_is_iso() {
        let inp = inputs(Side::Buy, 50, 25, 10_100);
        let sql = seconds_sql(&inp).expect("sql");
        assert!(sql.contains("FROM candles_1s WHERE security_id = 2885 AND segment = 'NSE_FNO'"));
        assert!(sql.contains("ts >= '2026-11-12T10:00:00.000000Z'"));
        assert!(sql.contains("ts <= '2026-11-12T10:01:01.000000Z'"));
        assert!(sql.ends_with("LIMIT 63"));
        let mut bad = inp;
        bad.segment = "NSE'--".into();
        assert!(seconds_sql(&bad).is_none());
        bad.segment = String::new();
        assert!(seconds_sql(&bad).is_none());
        assert_eq!(ts_text(0), "1970-01-01T00:00:00.000000Z");
    }

    #[test]
    fn test_parse_seconds_body_takes_magnitude_and_skips_bad_prices() {
        let body = r#"{"dataset":[
            ["2026-11-12T10:00:01.000000Z", 100.05, 99.9, -120],
            ["2026-11-12T10:00:02.000000Z", "x", 99.9, 5],
            ["2026-11-12T10:00:03.000000Z", 0.0, 99.9, 5]
        ]}"#;
        let secs = parse_seconds_body(body).expect("parse");
        assert_eq!(secs, vec![bar(DEC + 1, 9_990, 10_005, 120)]);
        assert!(parse_seconds_body("nope").is_err());
        assert!(parse_seconds_body("{}").is_err());
    }

    #[tokio::test]
    async fn test_read_seconds_unreachable_or_bad_segment_is_an_error() {
        let inp = inputs(Side::Buy, 50, 25, 10_100);
        assert!(read_seconds("http://127.0.0.1:1/exec", &inp).await.is_err());
        let mut bad = inp;
        bad.segment = "x".into();
        assert!(read_seconds("http://127.0.0.1:1/exec", &bad).await.is_err());
    }

    /// Review finding 13 (LF-7): a stored `max_fill_seconds` past the config
    /// ceiling sized a `Vec` (about 51 GB at `i32::MAX`) and the read LIMIT.
    /// It now reads as no pricing inputs; the ceiling itself still prices.
    #[test]
    fn test_order_inputs_from_decision_refuses_max_fill_seconds_past_the_ceiling() {
        let row = EventRow {
            event: "decision".into(),
            signal_id: 1,
            side: "BUY".into(),
            qty: 50,
            segment: "NSE_FNO".into(),
            security_id: 2885,
            candle_end_ns: Some((DEC + 1) * NANOS_PER_SEC),
            ltp_paise: Some(10_000),
            limit_paise: Some(10_100),
            tick_paise: Some(5),
            lot_size: Some(25),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(i64::from(i32::MAX)),
            ..EventRow::default()
        };
        assert!(OrderInputs::from_decision(&row).is_none());
        let past = EventRow {
            max_fill_seconds: Some(i64::from(LIVE_FILL_MAX_FILL_SECONDS_CEILING) + 1),
            ..row.clone()
        };
        assert!(OrderInputs::from_decision(&past).is_none());
        let at = EventRow {
            max_fill_seconds: Some(i64::from(LIVE_FILL_MAX_FILL_SECONDS_CEILING)),
            ..row
        };
        let inp = OrderInputs::from_decision(&at).expect("the ceiling itself prices");
        assert_eq!(inp.max_fill_seconds, LIVE_FILL_MAX_FILL_SECONDS_CEILING);
    }

    /// Review finding 11: `Recheck::not_run` carries no seen seconds and the
    /// kind it was given.
    #[test]
    fn test_not_run_has_zero_seconds_seen_and_keeps_its_kind() {
        let r = Recheck::not_run(RecheckKind::Skipped("order_cap"));
        assert_eq!(r.seconds_seen, 0);
        assert_eq!(r.kind, RecheckKind::Skipped("order_cap"));
        let u = Recheck::not_run(RecheckKind::Unavailable("catch_up_rebuild"));
        assert_eq!(u.seconds_seen, 0);
        assert_eq!(u.kind, RecheckKind::Unavailable("catch_up_rebuild"));
    }
}
