//! Benchmark: TradingCalendar O(1) trading day validation.
//!
//! Measures `is_trading_day()` performance — O(1) HashSet lookup, no allocation.
//! Budget from quality/benchmark-budgets.toml: < 50ns per check.

use std::hint::black_box;

use chrono::NaiveDate;
use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::config::{NseHolidayEntry, TradingConfig};
use tickvault_common::trading_calendar::TradingCalendar;

/// Builds a realistic TradingCalendar with 15 holidays (typical NSE year).
fn build_calendar() -> TradingCalendar {
    let config = TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![
            NseHolidayEntry {
                date: "2026-01-26".to_string(),
                name: "Republic Day".to_string(),
            },
            NseHolidayEntry {
                date: "2026-03-03".to_string(),
                name: "Holi".to_string(),
            },
            NseHolidayEntry {
                date: "2026-03-26".to_string(),
                name: "Shri Ram Navami".to_string(),
            },
            NseHolidayEntry {
                date: "2026-03-31".to_string(),
                name: "Shri Mahavir Jayanti".to_string(),
            },
            NseHolidayEntry {
                date: "2026-04-03".to_string(),
                name: "Good Friday".to_string(),
            },
            NseHolidayEntry {
                date: "2026-04-14".to_string(),
                name: "Dr. Ambedkar Jayanti".to_string(),
            },
            NseHolidayEntry {
                date: "2026-05-01".to_string(),
                name: "Maharashtra Day".to_string(),
            },
            NseHolidayEntry {
                date: "2026-05-28".to_string(),
                name: "Bakri Eid".to_string(),
            },
            NseHolidayEntry {
                date: "2026-06-26".to_string(),
                name: "Muharram".to_string(),
            },
            NseHolidayEntry {
                date: "2026-09-14".to_string(),
                name: "Ganesh Chaturthi".to_string(),
            },
            NseHolidayEntry {
                date: "2026-10-02".to_string(),
                name: "Gandhi Jayanti".to_string(),
            },
            NseHolidayEntry {
                date: "2026-10-20".to_string(),
                name: "Dussehra".to_string(),
            },
            NseHolidayEntry {
                date: "2026-11-10".to_string(),
                name: "Diwali Balipratipada".to_string(),
            },
            NseHolidayEntry {
                date: "2026-11-24".to_string(),
                name: "Guru Nanak Jayanti".to_string(),
            },
            NseHolidayEntry {
                date: "2026-12-25".to_string(),
                name: "Christmas".to_string(),
            },
        ],
        muhurat_trading_dates: vec![NseHolidayEntry {
            date: "2026-11-08".to_string(),
            name: "Diwali 2026".to_string(),
        }],
        nse_mock_trading_dates: vec![],
    };
    // APPROVED: test/bench setup — calendar construction is cold path only.
    TradingCalendar::from_config(&config).unwrap()
}

fn bench_is_trading_day_weekday(c: &mut Criterion) {
    let cal = build_calendar();
    // 2026-03-09 is a Monday, not a holiday
    let date = NaiveDate::from_ymd_opt(2026, 3, 9).unwrap();
    c.bench_function("calendar/is_trading_day", |b| {
        b.iter(|| cal.is_trading_day(black_box(date)));
    });
}

fn bench_is_trading_day_holiday(c: &mut Criterion) {
    let cal = build_calendar();
    // 2026-01-26 is Republic Day (Monday)
    let date = NaiveDate::from_ymd_opt(2026, 1, 26).unwrap();
    c.bench_function("calendar/is_trading_day_holiday", |b| {
        b.iter(|| cal.is_trading_day(black_box(date)));
    });
}

fn bench_is_trading_day_weekend(c: &mut Criterion) {
    let cal = build_calendar();
    // 2026-03-07 is a Saturday
    let date = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap();
    c.bench_function("calendar/is_trading_day_weekend", |b| {
        b.iter(|| cal.is_trading_day(black_box(date)));
    });
}

fn bench_is_holiday(c: &mut Criterion) {
    let cal = build_calendar();
    let date = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap(); // Dussehra
    c.bench_function("calendar/is_holiday", |b| {
        b.iter(|| cal.is_holiday(black_box(date)));
    });
}

criterion_group!(
    benches,
    bench_is_trading_day_weekday,
    bench_is_trading_day_holiday,
    bench_is_trading_day_weekend,
    bench_is_holiday,
);
criterion_main!(benches);
