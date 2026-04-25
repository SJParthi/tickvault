//! Phase 2 wiring integration tests (slice 3 — 2026-04-20).
//!
//! Verifies the boot-time snapshotter classifier + the
//! [`Phase2InstrumentsSource`] enum behave correctly when fed
//! synthetic ticks and synthetic universes. The pure-logic core
//! (`phase2_delta`, `preopen_price_buffer`) has its own unit tests;
//! these tests exercise the **wiring contracts** between them.
//!
//! Scope:
//! - Snapshotter classifier — segment / F&O / minute filters.
//! - `Phase2InstrumentsSource::Static` vs `Phase2InstrumentsSource::Dynamic`
//!   selection at trigger time.
//!
//! These tests live in `tests/` (integration) so they can only touch
//! the public API of `tickvault-core` — same surface a real consumer
//! sees.

#![allow(clippy::field_reassign_with_default)]

use std::collections::HashMap;
use std::time::Duration;

use chrono::{FixedOffset, TimeZone, Utc};
use tickvault_common::config::{NseHolidayEntry, TradingConfig};
use tickvault_common::instrument_types::{
    ExpiryCalendar, FnoUnderlying, FnoUniverse, OptionChain, OptionChainEntry, OptionChainKey,
    UnderlyingKind, UniverseBuildMetadata,
};
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::types::ExchangeSegment;

use tickvault_core::instrument::phase2_delta::compute_phase2_stock_subscriptions;
use tickvault_core::instrument::preopen_price_buffer::{
    PreOpenCloses, SnapshotterFilterReason, SnapshotterOutcome, build_fno_stock_lookup,
    classify_tick, new_shared_preopen_buffer, snapshot,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn empty_metadata() -> UniverseBuildMetadata {
    UniverseBuildMetadata {
        csv_source: String::new(),
        csv_row_count: 0,
        parsed_row_count: 0,
        index_count: 0,
        equity_count: 0,
        underlying_count: 0,
        derivative_count: 0,
        option_chain_count: 0,
        build_duration: Duration::from_millis(0),
        build_timestamp: FixedOffset::east_opt(0)
            .expect("zero offset is valid")
            .from_utc_datetime(&Utc::now().naive_utc()),
    }
}

fn empty_calendar() -> TradingCalendar {
    let cfg = TradingConfig {
        market_open_time: "09:15:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![NseHolidayEntry {
            date: "2026-01-26".to_string(),
            name: "Republic Day".to_string(),
        }],
        muhurat_trading_dates: vec![],
        nse_mock_trading_dates: vec![],
    };
    TradingCalendar::from_config(&cfg).expect("trading calendar must build")
}

fn make_stock_underlying(
    symbol: &str,
    price_feed_id: u32,
    derivative_segment: ExchangeSegment,
) -> FnoUnderlying {
    FnoUnderlying {
        underlying_symbol: symbol.to_string(),
        underlying_security_id: 1_000,
        price_feed_security_id: price_feed_id,
        price_feed_segment: ExchangeSegment::NseEquity,
        derivative_segment,
        kind: UnderlyingKind::Stock,
        lot_size: 100,
        contract_count: 1,
    }
}

fn make_index_underlying(symbol: &str, price_feed_id: u32) -> FnoUnderlying {
    FnoUnderlying {
        underlying_symbol: symbol.to_string(),
        underlying_security_id: 26_000,
        price_feed_security_id: price_feed_id,
        price_feed_segment: ExchangeSegment::IdxI,
        derivative_segment: ExchangeSegment::NseFno,
        kind: UnderlyingKind::NseIndex,
        lot_size: 50,
        contract_count: 1,
    }
}

/// Build a universe with one Stock (RELIANCE, NSE_EQ id=2885), one
/// Index (NIFTY, IDX_I id=13), and a chain for RELIANCE.
fn universe_with_one_stock_and_one_index(today: chrono::NaiveDate) -> FnoUniverse {
    let mut underlyings = HashMap::new();
    underlyings.insert(
        "RELIANCE".to_string(),
        make_stock_underlying("RELIANCE", 2885, ExchangeSegment::NseFno),
    );
    underlyings.insert("NIFTY".to_string(), make_index_underlying("NIFTY", 13));

    let expiry = today + chrono::Duration::days(7);
    let mut expiry_calendars = HashMap::new();
    expiry_calendars.insert(
        "RELIANCE".to_string(),
        ExpiryCalendar {
            underlying_symbol: "RELIANCE".to_string(),
            expiry_dates: vec![expiry],
        },
    );

    let chain = OptionChain {
        underlying_symbol: "RELIANCE".to_string(),
        expiry_date: expiry,
        calls: (0..51)
            .map(|i| OptionChainEntry {
                security_id: 40_000 + i as u32,
                strike_price: 2800.0 + 50.0 * f64::from(i),
                lot_size: 100,
            })
            .collect(),
        puts: (0..51)
            .map(|i| OptionChainEntry {
                security_id: 50_000 + i as u32,
                strike_price: 2800.0 + 50.0 * f64::from(i),
                lot_size: 100,
            })
            .collect(),
        future_security_id: Some(60_000),
    };
    let mut option_chains = HashMap::new();
    option_chains.insert(
        OptionChainKey {
            underlying_symbol: "RELIANCE".to_string(),
            expiry_date: expiry,
        },
        chain,
    );

    FnoUniverse {
        underlyings,
        derivative_contracts: HashMap::new(),
        instrument_info: HashMap::new(),
        option_chains,
        expiry_calendars,
        subscribed_indices: Vec::new(),
        build_metadata: empty_metadata(),
    }
}

/// IST hh:mm:ss → exchange_timestamp (IST epoch seconds), per Dhan
/// `live-market-feed.md` rule 14 ("LTT fields are IST epoch seconds").
fn ist_to_exchange_ts(date: chrono::NaiveDate, hour: u32, minute: u32, second: u32) -> u32 {
    let ist_naive = date
        .and_hms_opt(hour, minute, second)
        .expect("valid hh:mm:ss");
    // Treat the IST wall-clock as if it were UTC, then take the
    // resulting epoch — that IS the IST epoch second per Dhan's
    // convention (their LTT field encodes IST as seconds).
    u32::try_from(ist_naive.and_utc().timestamp()).expect("fits u32 for 2026-era ts")
}

fn tick_at(security_id: u32, segment_code: u8, ts: u32, ltp: f32) -> ParsedTick {
    let mut tick = ParsedTick::default();
    tick.security_id = security_id;
    tick.exchange_segment_code = segment_code;
    tick.exchange_timestamp = ts;
    tick.last_traded_price = ltp;
    tick
}

fn next_weekday(date: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    let mut d = date;
    while matches!(d.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
        d = d.succ_opt().expect("date arithmetic must not overflow");
    }
    d
}

// ---------------------------------------------------------------------------
// 1. Snapshotter classifier — minute window
// ---------------------------------------------------------------------------

#[test]
fn test_snapshotter_records_ticks_only_during_preopen_window() {
    let today = next_weekday(chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap());
    let universe = universe_with_one_stock_and_one_index(today);
    let lookup = build_fno_stock_lookup(&universe);

    // Inside window (09:12:15 IST).
    let inside = tick_at(2885, 1, ist_to_exchange_ts(today, 9, 12, 15), 2847.5);
    assert!(matches!(
        classify_tick(&inside, &lookup),
        SnapshotterOutcome::Buffered { .. }
    ));

    // Outside window — 09:15 (market open) is past 09:13 cutoff.
    let outside = tick_at(2885, 1, ist_to_exchange_ts(today, 9, 15, 0), 2847.5);
    assert_eq!(
        classify_tick(&outside, &lookup),
        SnapshotterOutcome::Filtered(SnapshotterFilterReason::WrongMinute)
    );

    // Outside window — 09:07:59 (one second before window opens).
    let before = tick_at(2885, 1, ist_to_exchange_ts(today, 9, 7, 59), 2847.5);
    assert_eq!(
        classify_tick(&before, &lookup),
        SnapshotterOutcome::Filtered(SnapshotterFilterReason::WrongMinute)
    );
}

// ---------------------------------------------------------------------------
// 2. Snapshotter — non-NSE_EQ segments are filtered
// ---------------------------------------------------------------------------

#[test]
fn test_snapshotter_filters_non_nse_eq_ticks() {
    let today = next_weekday(chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap());
    let universe = universe_with_one_stock_and_one_index(today);
    let lookup = build_fno_stock_lookup(&universe);

    // Same security_id (2885) but segment_code = 2 (NSE_FNO). Must
    // be filtered — F&O legs of the same ID must not corrupt the
    // pre-open price.
    let fno = tick_at(2885, 2, ist_to_exchange_ts(today, 9, 12, 15), 2847.5);
    assert_eq!(
        classify_tick(&fno, &lookup),
        SnapshotterOutcome::Filtered(SnapshotterFilterReason::WrongSegment)
    );

    // Non-whitelisted IDX_I (unknown security_id 999) — filtered as
    // NotFnoStock because the combined lookup doesn't contain it.
    let idx = tick_at(999, 0, ist_to_exchange_ts(today, 9, 12, 15), 23500.0);
    assert_eq!(
        classify_tick(&idx, &lookup),
        SnapshotterOutcome::Filtered(SnapshotterFilterReason::NotFnoStock)
    );

    // Unknown segment byte — also filtered (with WrongSegment).
    let unknown = tick_at(2885, 99, ist_to_exchange_ts(today, 9, 12, 15), 2847.5);
    assert_eq!(
        classify_tick(&unknown, &lookup),
        SnapshotterOutcome::Filtered(SnapshotterFilterReason::WrongSegment)
    );
}

// ---------------------------------------------------------------------------
// 3. Snapshotter — non-F&O stocks are filtered
// ---------------------------------------------------------------------------

#[test]
fn test_snapshotter_filters_non_fno_stocks() {
    let today = next_weekday(chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap());
    let universe = universe_with_one_stock_and_one_index(today);
    let lookup = build_fno_stock_lookup(&universe);

    // RELIANCE (id=2885) is in the universe — should buffer.
    let yes = tick_at(2885, 1, ist_to_exchange_ts(today, 9, 12, 0), 2847.5);
    assert!(matches!(
        classify_tick(&yes, &lookup),
        SnapshotterOutcome::Buffered { .. }
    ));

    // Some random NSE_EQ id NOT in the F&O universe — must be
    // NotFnoStock, even though segment + minute pass.
    let no = tick_at(99_999, 1, ist_to_exchange_ts(today, 9, 12, 0), 100.0);
    assert_eq!(
        classify_tick(&no, &lookup),
        SnapshotterOutcome::Filtered(SnapshotterFilterReason::NotFnoStock)
    );
}

// ---------------------------------------------------------------------------
// 4. Snapshotter — buckets-by-minute end-to-end (write through buffer)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_snapshotter_buckets_by_minute() {
    let today = next_weekday(chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap());
    let universe = universe_with_one_stock_and_one_index(today);
    let lookup = build_fno_stock_lookup(&universe);
    let buffer = new_shared_preopen_buffer();

    // Five ticks across the five minutes.
    for (minute, price) in [
        (8, 2840.0_f32),
        (9, 2841.0),
        (10, 2842.0),
        (11, 2843.0),
        (12, 2847.5),
    ] {
        let tick = tick_at(2885, 1, ist_to_exchange_ts(today, 9, minute, 30), price);
        match classify_tick(&tick, &lookup) {
            SnapshotterOutcome::Buffered {
                symbol,
                minute_index,
            } => {
                let mut g = buffer.write().await;
                g.entry(symbol)
                    .or_default()
                    .record(minute_index, f64::from(price));
            }
            other => panic!("expected Buffered, got {other:?}"),
        }
    }

    let snap = snapshot(&buffer).await;
    let r = snap.get("RELIANCE").expect("RELIANCE bucket exists");
    assert_eq!(r.closes[0], Some(2840.0));
    assert_eq!(r.closes[1], Some(2841.0));
    assert_eq!(r.closes[2], Some(2842.0));
    assert_eq!(r.closes[3], Some(2843.0));
    assert_eq!(r.closes[4], Some(2847.5));
    assert_eq!(r.backtrack_latest(), Some(2847.5));
}

// ---------------------------------------------------------------------------
// 5. Phase 2 source Dynamic — computes from buffer at trigger time, not
//    spawn time
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_phase2_source_dynamic_computes_at_trigger_not_spawn_time() {
    let today = next_weekday(chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap());
    let universe = universe_with_one_stock_and_one_index(today);
    let calendar = empty_calendar();
    let buffer = new_shared_preopen_buffer();

    // Pretend boot fires the Dynamic source NOW with the buffer
    // empty. Compute against the empty snapshot — must yield zero
    // instruments because the snapshotter hasn't published yet.
    let snap_empty = snapshot(&buffer).await;
    let plan_empty =
        compute_phase2_stock_subscriptions(&universe, &snap_empty, &calendar, today, 25);
    assert!(plan_empty.instruments.is_empty());

    // Now the snapshotter publishes a 09:12 close.
    {
        let mut g = buffer.write().await;
        let mut closes = PreOpenCloses::default();
        closes.record(4, 2847.5);
        g.insert("RELIANCE".to_string(), closes);
    }

    // Re-snapshot at "trigger time" — the Dynamic source must now
    // produce a non-empty list.
    let snap_full = snapshot(&buffer).await;
    let plan_full = compute_phase2_stock_subscriptions(&universe, &snap_full, &calendar, today, 25);
    assert!(
        !plan_full.instruments.is_empty(),
        "trigger-time recompute must reflect newly buffered prices"
    );
    assert_eq!(plan_full.stocks_subscribed, vec!["RELIANCE".to_string()]);
}

// ---------------------------------------------------------------------------
// 6. Phase 2 source Static — backward-compat path is preserved
// ---------------------------------------------------------------------------

#[test]
fn test_phase2_source_static_backward_compat() {
    use tickvault_core::instrument::phase2_scheduler::Phase2InstrumentsSource;
    use tickvault_core::websocket::types::InstrumentSubscription;

    // The legacy alert-only call site passed `Some(Vec<...>)` as the
    // instrument list. The new Static variant carries the same
    // payload; we assert it round-trips and the count is preserved.
    let list = vec![
        InstrumentSubscription::new(ExchangeSegment::NseFno, 12345),
        InstrumentSubscription::new(ExchangeSegment::NseFno, 67890),
    ];
    let src = Phase2InstrumentsSource::Static(list.clone());
    match src {
        Phase2InstrumentsSource::Static(inner) => {
            assert_eq!(inner.len(), list.len());
            assert_eq!(inner[0].security_id, "12345");
            assert_eq!(inner[1].security_id, "67890");
        }
        Phase2InstrumentsSource::Dynamic { .. } => panic!("expected Static variant"),
    }
}

// ---------------------------------------------------------------------------
// 7. Phase 2 source Dynamic with empty buffer → empty list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_phase2_source_dynamic_with_empty_buffer_produces_empty_list() {
    let today = next_weekday(chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap());
    let universe = universe_with_one_stock_and_one_index(today);
    let calendar = empty_calendar();
    let buffer = new_shared_preopen_buffer();

    let snap = snapshot(&buffer).await;
    let plan = compute_phase2_stock_subscriptions(&universe, &snap, &calendar, today, 25);
    assert!(plan.instruments.is_empty());
    assert_eq!(plan.stocks_skipped_no_price, vec!["RELIANCE".to_string()]);
}
