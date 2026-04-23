//! Benchmark: Phase 2 dispatch path — `select_depth_instruments`.
//!
//! Plan item Q (unified 09:12 subscription): the real-C dispatcher at
//! 09:13 IST must compute the per-underlying ATM ± 24 selection once
//! per day. Although this is a cold path (runs once, not per tick),
//! we still want a fast budget so that (a) the dispatcher never
//! drifts into hundred-ms territory that would delay depth sockets
//! past 09:13:30 IST, and (b) any O(n²) regression in the option-chain
//! binary-search path is caught at review time.
//!
//! # Budget
//!
//! `phase2_compute_subscriptions < 1 ms` for a realistic universe
//! (4 depth underlyings × 200 strikes per chain).
//!
//! In practice the function is dominated by `find_atm_security_ids`
//! (one binary search per underlying) and an ATM ± 24 slice of the
//! sorted calls/puts vectors, so the real cost is ≤ a few µs. The
//! 1 ms budget is a generous ceiling that catches accidental
//! worst-case O(n²) regressions without being so tight that a
//! debug-build noise spike fails CI.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;

use chrono::{FixedOffset, NaiveDate, Utc};
use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::instrument_types::{
    ExpiryCalendar, FnoUniverse, OptionChain, OptionChainEntry, OptionChainKey,
    UniverseBuildMetadata,
};
use tickvault_core::instrument::depth_strike_selector::{
    DEPTH_ATM_STRIKES_EACH_SIDE, select_depth_instruments,
};

/// Build a synthetic option chain centered around `atm_strike` with
/// `strikes` entries on each side (total = 2 * strikes + 1 CE + same PE).
/// Strike spacing is fixed at 100.0 to mimic NIFTY-style chains.
fn make_chain(
    underlying: &str,
    expiry: NaiveDate,
    atm_strike: f64,
    strikes_each_side: usize,
    base_security_id: u32,
) -> OptionChain {
    let total = 2 * strikes_each_side + 1;
    let mut calls = Vec::with_capacity(total);
    let mut puts = Vec::with_capacity(total);
    for i in 0..total {
        let offset = i as f64 - strikes_each_side as f64;
        let strike = atm_strike + offset * 100.0;
        let sid_ce = base_security_id + (i as u32) * 2;
        let sid_pe = sid_ce + 1;
        calls.push(OptionChainEntry {
            security_id: sid_ce,
            strike_price: strike,
            lot_size: 50,
        });
        puts.push(OptionChainEntry {
            security_id: sid_pe,
            strike_price: strike,
            lot_size: 50,
        });
    }
    OptionChain {
        underlying_symbol: underlying.to_string(),
        expiry_date: expiry,
        calls,
        puts,
        future_security_id: None,
    }
}

/// Build a realistic FnoUniverse with 4 depth underlyings, each with
/// 200 strikes per chain and the nearest-expiry entry populated.
fn make_depth_universe() -> FnoUniverse {
    let today = Utc::now()
        .with_timezone(&FixedOffset::east_opt(19_800).unwrap())
        .date_naive();
    let nearest_expiry = today + chrono::Duration::days(7);

    let mut option_chains: HashMap<OptionChainKey, OptionChain> = HashMap::with_capacity(4);
    let mut expiry_calendars: HashMap<String, ExpiryCalendar> = HashMap::with_capacity(4);

    // 200 strikes per chain = 100 each side of ATM.
    let strikes_each_side = 100;

    let specs = [
        ("NIFTY", 22_500.0, 100_000_u32),
        ("BANKNIFTY", 50_000.0, 200_000_u32),
        ("FINNIFTY", 23_000.0, 300_000_u32),
        ("MIDCPNIFTY", 12_500.0, 400_000_u32),
    ];

    for (symbol, atm, base_sid) in specs {
        let chain = make_chain(symbol, nearest_expiry, atm, strikes_each_side, base_sid);
        option_chains.insert(
            OptionChainKey {
                underlying_symbol: symbol.to_string(),
                expiry_date: nearest_expiry,
            },
            chain,
        );
        expiry_calendars.insert(
            symbol.to_string(),
            ExpiryCalendar {
                underlying_symbol: symbol.to_string(),
                expiry_dates: vec![nearest_expiry],
            },
        );
    }

    FnoUniverse {
        underlyings: HashMap::new(),
        derivative_contracts: HashMap::new(),
        instrument_info: HashMap::new(),
        option_chains,
        expiry_calendars,
        subscribed_indices: Vec::new(),
        build_metadata: UniverseBuildMetadata {
            csv_source: "bench".to_string(),
            csv_row_count: 0,
            parsed_row_count: 0,
            index_count: 0,
            equity_count: 0,
            underlying_count: 0,
            derivative_count: 0,
            option_chain_count: 0,
            build_duration: Duration::ZERO,
            build_timestamp: Utc::now().with_timezone(&FixedOffset::east_opt(19_800).unwrap()),
        },
    }
}

fn make_spot_prices() -> HashMap<String, f64> {
    let mut map = HashMap::with_capacity(4);
    map.insert("NIFTY".to_string(), 22_510.5);
    map.insert("BANKNIFTY".to_string(), 50_075.2);
    map.insert("FINNIFTY".to_string(), 22_980.0);
    map.insert("MIDCPNIFTY".to_string(), 12_540.0);
    map
}

fn bench_phase2_compute_subscriptions(c: &mut Criterion) {
    let universe = make_depth_universe();
    let spot_prices = make_spot_prices();
    let today = Utc::now()
        .with_timezone(&FixedOffset::east_opt(19_800).unwrap())
        .date_naive();
    let underlyings: [&str; 4] = ["NIFTY", "BANKNIFTY", "FINNIFTY", "MIDCPNIFTY"];

    c.bench_function("phase2/compute_subscriptions", |b| {
        b.iter(|| {
            let selections = select_depth_instruments(
                black_box(&universe),
                black_box(&underlyings),
                black_box(&spot_prices),
                black_box(today),
                black_box(DEPTH_ATM_STRIKES_EACH_SIDE),
            );
            black_box(selections);
        });
    });
}

fn bench_phase2_compute_subscriptions_single_underlying(c: &mut Criterion) {
    // Isolates per-underlying cost so regressions in binary search or
    // strike-range slicing are easy to pinpoint.
    let universe = make_depth_universe();
    let spot_prices = make_spot_prices();
    let today = Utc::now()
        .with_timezone(&FixedOffset::east_opt(19_800).unwrap())
        .date_naive();
    let underlyings: [&str; 1] = ["NIFTY"];

    c.bench_function("phase2/compute_subscriptions_single", |b| {
        b.iter(|| {
            let selections = select_depth_instruments(
                black_box(&universe),
                black_box(&underlyings),
                black_box(&spot_prices),
                black_box(today),
                black_box(DEPTH_ATM_STRIKES_EACH_SIDE),
            );
            black_box(selections);
        });
    });
}

criterion_group!(
    benches,
    bench_phase2_compute_subscriptions,
    bench_phase2_compute_subscriptions_single_underlying,
);
criterion_main!(benches);
