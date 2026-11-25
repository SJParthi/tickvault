#![cfg(test)]
//! Speed fix 6 (2026-11-17): the raw tick rebuilds exactly the row the old
//! converted path built, and counts exactly what it counted.

use super::*;
use crate::tick_persistence::count_nonfinite_tick_floats;
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Every field of two rows, floats compared by their bits (`-0.0` and `0.0`,
/// or two NaNs, must not pass for each other).
fn same_bits(a: &TickRow, b: &TickRow) -> bool {
    let f = |v: f64| v.to_bits();
    let o = |v: Option<f64>| v.map(f64::to_bits);
    a.security_id == b.security_id
        && a.segment == b.segment
        && f(a.ltp) == f(b.ltp)
        && a.volume == b.volume
        && a.ts_ist_nanos == b.ts_ist_nanos
        && a.capture_seq == b.capture_seq
        && o(a.open) == o(b.open)
        && o(a.high) == o(b.high)
        && o(a.low) == o(b.low)
        && o(a.close) == o(b.close)
        && a.oi == b.oi
        && a.oi_day_high == b.oi_day_high
        && a.oi_day_low == b.oi_day_low
        && o(a.avg_price) == o(b.avg_price)
        && a.last_trade_qty == b.last_trade_qty
        && a.total_buy_qty == b.total_buy_qty
        && a.total_sell_qty == b.total_sell_qty
        && a.received_at_ist_nanos == b.received_at_ist_nanos
        && a.payload_hash == b.payload_hash
}

/// A recorder that keeps every counter, scoped with
/// `metrics::with_local_recorder` (never a global install).
#[derive(Default)]
struct Counters {
    map: std::sync::Mutex<BTreeMap<String, Arc<AtomicU64>>>,
}

impl Counters {
    fn values(&self) -> BTreeMap<String, u64> {
        self.map
            .lock()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
                    .filter(|(_, v)| *v > 0)
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl metrics::Recorder for Counters {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        let mut labels: Vec<String> = key
            .labels()
            .map(|l| format!("{}={}", l.key(), l.value()))
            .collect();
        labels.sort();
        let name = format!("{}{{{}}}", key.name(), labels.join(","));
        let Ok(mut map) = self.map.lock() else {
            return metrics::Counter::noop();
        };
        metrics::Counter::from_arc(Arc::clone(map.entry(name).or_default()))
    }
    fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::noop()
    }
    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

/// The counters each builder moves for one tick: (old converted row, raw).
fn counted(tick: &ParsedTick) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
    let old = Counters::default();
    let raw = Counters::default();
    metrics::with_local_recorder(&old, || {
        let _ = TickRow::from_parsed_tick(tick, 1);
    });
    metrics::with_local_recorder(&raw, || {
        let _ = RawTick::from_parsed_tick(tick, 1);
    });
    (old.values(), raw.values())
}

fn price() -> impl Strategy<Value = f32> {
    prop_oneof![
        Just(f32::NAN),
        Just(-f32::NAN),
        Just(f32::INFINITY),
        Just(f32::NEG_INFINITY),
        Just(0.0),
        Just(-0.0),
        Just(f32::from_bits(1)),
        Just(-f32::from_bits(0x007f_ffff)),
        Just(f32::MIN_POSITIVE),
        Just(f32::MAX),
        Just(f32::MIN),
        Just(131_071.99),
        Just(131_072.0),
        any::<f32>(),
        (1_u32..4_000_000).prop_map(|p| p as f32 * 0.05),
        (1_i32..20_000_000).prop_map(|c| c as f32 / 100.0),
        (1_i32..20_000_000).prop_map(|c| c as f32 / 1_000.0),
    ]
}

fn tick() -> impl Strategy<Value = ParsedTick> {
    (
        (
            prop_oneof![
                any::<u64>(),
                Just(1_u64 << 63),
                Just(u64::MAX),
                0_u64..100_000
            ],
            any::<u8>(),
            price(),
            any::<u16>(),
            prop_oneof![Just(0_u32), Just(315_532_800_u32), any::<u32>()],
            prop_oneof![Just(0_i64), any::<i64>(), Just(i64::MAX), Just(i64::MIN)],
        ),
        (price(), any::<u32>(), any::<u32>(), any::<u32>()),
        (price(), price(), price(), price()),
        (any::<u32>(), any::<u32>(), any::<u32>()),
    )
        .prop_map(
            |(
                (id, seg, ltp, ltq, ltt, rcpt),
                (atp, vol, sell, buy),
                (o, c, h, l),
                (oi, oh, ol),
            )| {
                ParsedTick {
                    security_id: id,
                    exchange_segment_code: seg,
                    last_traded_price: ltp,
                    last_trade_quantity: ltq,
                    exchange_timestamp: ltt,
                    received_at_nanos: rcpt,
                    average_traded_price: atp,
                    volume: vol,
                    total_sell_quantity: sell,
                    total_buy_quantity: buy,
                    day_open: o,
                    day_close: c,
                    day_high: h,
                    day_low: l,
                    open_interest: oi,
                    oi_day_high: oh,
                    oi_day_low: ol,
                    ..ParsedTick::default()
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4_096, ..ProptestConfig::default() })]

    /// `RawTick::from_parsed_tick(t).to_row()` is `TickRow::from_parsed_tick(t)`
    /// bit for bit, the refusals are the same, the `tick_quotes` row is the old
    /// quote row, and the tripwire count is the converted row's.
    #[test]
    fn proptest_raw_tick_to_row_and_to_quote_row_equal_the_converted_row(
        t in tick(),
        seq in any::<i64>(),
    ) {
        let old = TickRow::from_parsed_tick(&t, seq);
        let raw = RawTick::from_parsed_tick(&t, seq);
        match (old, raw) {
            (Ok(old), Ok(raw)) => {
                let row = raw.to_row();
                prop_assert!(same_bits(&old, &row), "{old:?} vs {row:?}");
                prop_assert_eq!(raw.ts_ist_nanos(), old.ts_ist_nanos);
                prop_assert_eq!(raw.received_at_ist_nanos(), old.received_at_ist_nanos);
                prop_assert_eq!(raw.capture_seq(), seq);
                prop_assert_eq!(raw.nonfinite_floats(), count_nonfinite_tick_floats(&old));
                prop_assert_eq!(raw.nonfinite_floats(), 0, "a drain row is never non-finite");
                if let Some(arrival) = old.received_at_ist_nanos {
                    let want = TickRow { ts_ist_nanos: arrival, ..old };
                    let want_ex = (t.exchange_timestamp != 0)
                        .then(|| i64::from(t.exchange_timestamp) * 1_000_000_000);
                    let (quote, ex) = raw.to_quote_row();
                    prop_assert!(same_bits(&want, &quote), "{want:?} vs {quote:?}");
                    prop_assert_eq!(want_ex, ex);
                }
            }
            (Err(a), Err(b)) => prop_assert_eq!(a, b),
            (a, b) => prop_assert!(false, "verdicts differ: {a:?} vs {b:?}"),
        }
    }

    /// Every counter the old converted row moved, the raw tick moves by the
    /// same amount, on the drain, when it is built.
    #[test]
    fn proptest_raw_tick_from_parsed_tick_counts_what_the_converted_row_counted(t in tick()) {
        let (old, raw) = counted(&t);
        prop_assert_eq!(old, raw);
    }
}

/// The counter comparison is not vacuous: each refusal and the optional-price
/// count really move, on both builders, by the same amount.
#[test]
fn test_raw_tick_from_parsed_tick_counts_each_refusal_like_the_converted_row() {
    let good = ParsedTick {
        security_id: 1_333,
        exchange_segment_code: 1,
        last_traded_price: 245.35,
        exchange_timestamp: 1_793_178_000,
        received_at_nanos: 1_793_158_200_000_000_000,
        ..ParsedTick::default()
    };
    let wide = ParsedTick {
        security_id: 1 << 63,
        ..good
    };
    let nan_ltp = ParsedTick {
        last_traded_price: f32::NAN,
        ..good
    };
    let bad_optionals = ParsedTick {
        day_open: f32::NAN,
        day_high: f32::INFINITY,
        average_traded_price: f32::NEG_INFINITY,
        ..good
    };
    for (t, name, want) in [
        (
            &wide,
            "tv_tick_rows_refused_total{reason=security_id_width}",
            1,
        ),
        (
            &nan_ltp,
            "tv_tick_rows_refused_total{reason=price_not_finite}",
            1,
        ),
        (&bad_optionals, "tv_tick_optional_price_dropped_total{}", 3),
    ] {
        let (old, raw) = counted(t);
        assert_eq!(old.get(name).copied(), Some(want), "{name}: {old:?}");
        assert_eq!(old, raw, "{name}");
    }
    let (old, raw) = counted(&good);
    assert!(
        old.is_empty() && raw.is_empty(),
        "a clean tick counts nothing"
    );
}

#[test]
fn test_raw_tick_is_small_and_ts_ist_nanos_received_at_ist_nanos_capture_seq_nonfinite_floats_read_back()
 {
    assert!(std::mem::size_of::<RawTick>() <= 88);
    let t = ParsedTick {
        security_id: 42,
        exchange_segment_code: 2,
        last_traded_price: 101.25,
        exchange_timestamp: 1_793_178_000,
        received_at_nanos: 0,
        ..ParsedTick::default()
    };
    let raw = RawTick::from_parsed_tick(&t, 77).expect("raw");
    assert_eq!(raw.capture_seq(), 77);
    assert_eq!(raw.received_at_ist_nanos(), None, "no receipt, no column");
    assert_eq!(raw.ts_ist_nanos(), 1_793_178_000 * 1_000_000_000);
    let row = raw.to_row();
    assert_eq!(row.ltp.to_bits(), 101.25_f64.to_bits());
    assert_eq!(row.segment, "NSE_FNO");
    assert_eq!(row.open, None, "a Ticker field not carried is NULL");
    assert_eq!(raw.nonfinite_floats(), 0);
    let (quote, ex) = raw.to_quote_row();
    assert_eq!(ex, Some(1_793_178_000 * 1_000_000_000));
    assert_eq!(quote.ltp.to_bits(), row.ltp.to_bits());
}
