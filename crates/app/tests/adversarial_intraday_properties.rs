//! Adversarial properties of the Dhan intraday REST body parser
//! (`dhan_intraday_parse::parse_intraday_1m_candles`).
//!
//! That body is the only outside record the daily 1-minute cross-check
//! compares our candles against, so a parser that invents, drops or shifts a
//! candle turns into a false `clean` or a false `diverged` verdict.
//!
//! Properties:
//!
//! 1. Generated columnar bodies whose cells are numbers, nulls, strings,
//!    booleans, objects, huge integers, exponent floats and out-of-range
//!    float timestamps: the output equals a reference assembled here row by
//!    row from the same JSON (a row is kept only when open, high, low, close
//!    are numbers and the timestamp is an integer or a finite float in
//!    [1e9, 4e9]; the volume is the integer, else the rounded finite float
//!    below 9.2e18, else 0 and counted). `skipped_rows` and
//!    `coerced_volumes` equal the reference counts exactly.
//! 2. Arrays of different lengths, a missing column, an empty body, or a
//!    body that is not a JSON object give no candles and no faults.
//! 3. Arbitrary text, duplicate keys and nesting far past serde_json's
//!    recursion limit never panic; the output never holds more candles than
//!    the shortest column.
//!
//! Honest limit: the reference reads cells through serde_json's own `Value`
//! (as the parser does), so serde_json's float reading itself is not
//! checked here; it is shared by every JSON surface and pinned in the
//! order-update properties.

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use serde_json::{Map, Value, json};
use tickvault_app::dhan_intraday_parse::{
    IntradayCellFaults, MinuteCandle, parse_intraday_1m_candles,
};

const COLUMNS: [&str; 6] = ["open", "high", "low", "close", "volume", "timestamp"];
/// IST is UTC + 5:30 (19,800 s), restated so the reference does not borrow
/// the module's constant.
const IST_OFFSET_SECS: i128 = 19_800;

/// Reference minute stamp: IST, floored to the minute, in nanoseconds,
/// computed in i128 and clamped where the parser saturates.
fn ref_minute_nanos(utc_secs: i64) -> i64 {
    let clamp = |x: i128| {
        i64::try_from(x.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).expect("clamped")
    };
    let ist = i128::from(clamp(i128::from(utc_secs) + IST_OFFSET_SECS));
    let floor = ist - ist.rem_euclid(60);
    clamp(floor * 1_000_000_000)
}

fn ref_timestamp(v: &Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    let f = v.as_f64()?;
    // In range, so the cast is exact after rounding.
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: bounded by the 1e9..=4e9 check on the same line.
    (f.is_finite() && (1.0e9..=4.0e9).contains(&f)).then(|| f.round() as i64)
}

/// `(volume, coerced)`.
fn ref_volume(v: &Value) -> (i64, bool) {
    if let Some(i) = v.as_i64() {
        return (i, false);
    }
    match v.as_f64() {
        #[allow(clippy::cast_possible_truncation)]
        // APPROVED: |f| < 9.2e18 < i64::MAX, checked in the guard.
        Some(f) if f.is_finite() && f.abs() < 9.2e18 => (f.round() as i64, false),
        _ => (0, true),
    }
}

/// The reference parse, assembled from the body's own JSON.
fn reference(body: &str) -> (Vec<MinuteCandle>, IntradayCellFaults) {
    let mut faults = IntradayCellFaults::default();
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return (Vec::new(), faults);
    };
    let cols: Vec<&Vec<Value>> = COLUMNS
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_array))
        .collect();
    if cols.len() != COLUMNS.len() {
        return (Vec::new(), faults);
    }
    let n = cols[5].len();
    if n == 0 || cols.iter().any(|c| c.len() != n) {
        return (Vec::new(), faults);
    }
    let mut out = Vec::new();
    for i in 0..n {
        let prices: Vec<Option<f64>> = cols[..4].iter().map(|c| c[i].as_f64()).collect();
        let ts = ref_timestamp(&cols[5][i]);
        let (Some(o), Some(h), Some(l), Some(c), Some(t)) =
            (prices[0], prices[1], prices[2], prices[3], ts)
        else {
            faults.skipped_rows += 1;
            continue;
        };
        let (volume, coerced) = ref_volume(&cols[4][i]);
        if coerced {
            faults.coerced_volumes += 1;
        }
        out.push(MinuteCandle {
            minute_ts_ist_nanos: ref_minute_nanos(t),
            open: o,
            high: h,
            low: l,
            close: c,
            volume,
        });
    }
    (out, faults)
}

/// Candles compared field by field, floats by bits (NaN cannot occur: JSON
/// has no NaN).
fn same(a: &[MinuteCandle], b: &[MinuteCandle]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.minute_ts_ist_nanos == y.minute_ts_ist_nanos
                && x.volume == y.volume
                && [x.open, x.high, x.low, x.close]
                    .iter()
                    .zip([y.open, y.high, y.low, y.close])
                    .all(|(p, q)| p.to_bits() == q.to_bits())
        })
}

/// One hostile or ordinary cell.
fn cell() -> impl Strategy<Value = Value> {
    prop_oneof![
        4 => (1_700_000_000i64..1_900_000_000).prop_map(Value::from),
        3 => (0u32..10_000_000).prop_map(|p| json!(f64::from(p) / 100.0)),
        1 => any::<i64>().prop_map(Value::from),
        1 => any::<u64>().prop_map(Value::from),
        1 => any::<f64>().prop_filter("JSON has no NaN/inf", |f| f.is_finite()).prop_map(|f| json!(f)),
        1 => prop_oneof![Just(1.0e9), Just(4.0e9), Just(999_999_999.4), Just(4_000_000_000.6), Just(9.3e18), Just(-9.3e18), Just(9.1e18)].prop_map(|f| json!(f)),
        1 => Just(Value::Null),
        1 => "[ -~]{0,6}".prop_map(Value::String),
        1 => any::<bool>().prop_map(Value::Bool),
        1 => Just(json!({"v": 1})),
        1 => Just(json!([1])),
    ]
}

/// A columnar body: six equal-length columns, sometimes with an extra key.
fn body() -> impl Strategy<Value = String> {
    (1usize..12).prop_flat_map(|n| {
        (
            prop::collection::vec(prop::collection::vec(cell(), n), 6),
            any::<bool>(),
        )
            .prop_map(|(columns, extra)| {
                let mut map = Map::new();
                for (name, column) in COLUMNS.iter().zip(columns) {
                    map.insert((*name).to_owned(), Value::Array(column));
                }
                if extra {
                    map.insert("open_interest".to_owned(), json!([0]));
                }
                Value::Object(map).to_string()
            })
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// Property 1.
    #[test]
    fn prop_parse_matches_the_row_reference(body in body()) {
        let (got, got_faults) = parse_intraday_1m_candles(&body);
        let (want, want_faults) = reference(&body);
        prop_assert!(same(&got, &want), "{}\n got {:?}\nwant {:?}", body, got, want);
        prop_assert_eq!(got_faults, want_faults, "{}", body);
    }

    /// Property 2: one column one cell shorter or longer gives nothing.
    #[test]
    fn prop_unequal_columns_give_nothing(body in body(), which in 0usize..6, grow in any::<bool>()) {
        let mut v: Value = serde_json::from_str(&body).expect("generated body");
        let column = v[COLUMNS[which]].as_array_mut().expect("column");
        if grow {
            column.push(json!(1));
        } else {
            column.pop();
        }
        let text = v.to_string();
        let (got, faults) = parse_intraday_1m_candles(&text);
        prop_assert!(got.is_empty());
        prop_assert_eq!(faults, IntradayCellFaults::default());
    }

    /// Property 3: arbitrary text never panics and never yields more candles
    /// than the shortest column could hold.
    #[test]
    fn prop_arbitrary_text_never_panics(text in prop_oneof!["(?s).{0,300}", "[\\[\\]{}\":,0-9a-z. -]{0,300}"]) {
        let (got, _) = parse_intraday_1m_candles(&text);
        let (want, _) = reference(&text);
        prop_assert_eq!(got.len(), want.len());
    }
}

#[test]
fn test_missing_column_and_non_object_bodies_give_nothing() {
    let full = json!({
        "open": [1.0], "high": [1.0], "low": [1.0], "close": [1.0],
        "volume": [1], "timestamp": [1_793_000_000]
    });
    let (got, _) = parse_intraday_1m_candles(&full.to_string());
    assert_eq!(got.len(), 1, "the control body parses");
    for name in COLUMNS {
        let mut v = full.clone();
        v.as_object_mut().expect("object").remove(name);
        let (got, faults) = parse_intraday_1m_candles(&v.to_string());
        assert!(got.is_empty(), "without {name}");
        assert_eq!(faults, IntradayCellFaults::default());
        // The column present but not an array.
        let mut v = full.clone();
        v[name] = json!("1");
        assert!(
            parse_intraday_1m_candles(&v.to_string()).0.is_empty(),
            "{name} as text"
        );
    }
    for body in [
        "",
        "null",
        "[]",
        "{}",
        "1",
        "\"x\"",
        "{\"open\":[]}",
        "\u{feff}{}",
    ] {
        let (got, faults) = parse_intraday_1m_candles(body);
        assert!(got.is_empty(), "{body:?}");
        assert_eq!(faults, IntradayCellFaults::default(), "{body:?}");
    }
    // Every column empty: no candles, no faults.
    let empty = json!({
        "open": [], "high": [], "low": [], "close": [], "volume": [], "timestamp": []
    });
    assert!(parse_intraday_1m_candles(&empty.to_string()).0.is_empty());
}

#[test]
fn test_duplicate_keys_and_deep_nesting_never_panic() {
    // Duplicate key: whatever serde_json keeps, the parse matches the reference.
    let dup = r#"{"open":[1],"open":[2,3],"high":[1],"low":[1],"close":[1],"volume":[1],"timestamp":[1793000000]}"#;
    let (got, faults) = parse_intraday_1m_candles(dup);
    let (want, want_faults) = reference(dup);
    assert!(same(&got, &want));
    assert_eq!(faults, want_faults);
    // Far past serde_json's recursion limit, in a column and at the top.
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(parse_intraday_1m_candles(&deep).0.is_empty());
    let nested = format!("{}1{}", "[".repeat(5_000), "]".repeat(5_000));
    let deep_cell = format!(
        r#"{{"open":[{nested}],"high":[1],"low":[1],"close":[1],"volume":[1],"timestamp":[1793000000]}}"#
    );
    assert!(parse_intraday_1m_candles(&deep_cell).0.is_empty());
}

#[test]
fn test_timestamp_and_volume_edges() {
    let one = |ts: Value, vol: Value| {
        let body = json!({
            "open": [1.5], "high": [2.0], "low": [1.0], "close": [1.75],
            "volume": [vol], "timestamp": [ts]
        })
        .to_string();
        parse_intraday_1m_candles(&body)
    };
    // Integer timestamps at the extremes saturate, never wrap or panic.
    for ts in [i64::MAX, i64::MIN, 0, -1] {
        let (got, faults) = one(json!(ts), json!(1));
        assert_eq!(got.len(), 1, "{ts}");
        assert_eq!(got[0].minute_ts_ist_nanos, ref_minute_nanos(ts), "{ts}");
        assert_eq!(faults, IntradayCellFaults::default());
    }
    // A float timestamp outside [1e9, 4e9], or a u64 above i64, skips the row.
    for ts in [
        json!(999_999_999.4),
        json!(4.000_000_001e9),
        json!(u64::MAX),
        json!(1e300),
    ] {
        let (got, faults) = one(ts.clone(), json!(1));
        assert!(got.is_empty(), "{ts}");
        assert_eq!(faults.skipped_rows, 1, "{ts}");
    }
    // A float timestamp in range rounds to the nearest second.
    let (got, _) = one(json!(1_793_000_059.6), json!(1));
    assert_eq!(got[0].minute_ts_ist_nanos, ref_minute_nanos(1_793_000_060));
    // Volume: integer kept, in-range float rounded, others 0 and counted.
    for (vol, want, coerced) in [
        (json!(i64::MAX), i64::MAX, 0),
        (json!(-5), -5, 0),
        (json!(19_095.7), 19_096, 0),
        (json!(9.3e18), 0, 1),
        (json!(u64::MAX), 0, 1),
        (json!("100"), 0, 1),
        (Value::Null, 0, 1),
    ] {
        let (got, faults) = one(json!(1_793_000_000), vol.clone());
        assert_eq!(got[0].volume, want, "{vol}");
        assert_eq!(faults.coerced_volumes, coerced, "{vol}");
    }
}
