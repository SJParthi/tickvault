//! DHAT allocation gate for the in-memory order path WITH the production
//! metrics recorder installed (2026-10-30).
//!
//! Owner, 30 Oct 2026, on the placement-duration histogram: "Yes, fixed
//! counters (Recommended)", DHAT-gated with the production recorder. A test
//! with no recorder sees every metric handle as a no-op and cannot catch the
//! recorder's own allocations (the lesson `dhat_ws_lag_recorder.rs` records):
//! `metrics-exporter-prometheus` stores histogram samples in 64-slot blocks
//! and allocates a new block about every 64 samples.
//!
//! Gated here, over 10,000 calls each unless noted:
//! - `PlacementLatency::record_ns` (the fixed buckets): zero allocation.
//! - `OrderRateLimiter::check`, `OrderCircuitBreaker::check`,
//!   `OrderBudget::try_consume` (cached handles, literal labels): zero.
//! - `placement_telemetry::log_paper_trade` (the queue hand-off of the
//!   "PAPER TRADE" line, owner: "Write it after the order"): zero, over 512
//!   calls so the queue never fills.
//! - A paper placement: exactly the order record's own `String`s, at most
//!   [`PLACEMENT_BLOCKS_PER_ORDER`] blocks per placement, over 200 placements.
//!
//! Run: `cargo test -p tickvault-app --test dhat_order_path_recorder`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tickvault_common::order_types::{OrderType, OrderValidity, ProductType, TransactionType};
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::oms::circuit_breaker::OrderCircuitBreaker;
use tickvault_trading::oms::engine::OrderManagementSystem;
use tickvault_trading::oms::placement_telemetry::{
    PaperTradeRecord, PlacementLatency, flush_paper_trade_log, log_paper_trade,
};
use tickvault_trading::oms::rate_limiter::{OrderBudget, OrderRateLimiter};
use tickvault_trading::oms::types::PlaceOrderRequest;
use tickvault_trading::oms::{OmsError, OrderApiClient, TokenProvider};

mod dhat_support;

const BUDGET_BYTES: u64 = 1024;
const BUDGET_BLOCKS: u64 = 8;

/// A paper placement's own heap: the correlation id handed out by the pool,
/// the `PAPER-<n>` id and its three copies (the record's own field, the order
/// book key, the correlation tracker's value). Nothing else may allocate.
const PLACEMENT_BLOCKS_PER_ORDER: u64 = 5;
/// Allocated once per book on its first placements, whatever their number
/// (measured 2026-10-30: +3 at 64, 130 and 200 placements alike).
const PLACEMENT_BLOCKS_PER_BOOK: u64 = 3;
/// Below the broker's 250-per-minute tier, so no placement is refused.
const PLACEMENTS: u64 = 200;

/// The production `*_duration_ns` buckets (`observability.rs`).
const PROD_DURATION_NS_BUCKETS: &[f64] = &[
    100.0,
    500.0,
    1_000.0,
    5_000.0,
    10_000.0,
    50_000.0,
    100_000.0,
    500_000.0,
    1_000_000.0,
    10_000_000.0,
    100_000_000.0,
    1_000_000_000.0,
    10_000_000_000.0,
];

struct DhatTokenProvider;
impl TokenProvider for DhatTokenProvider {
    fn get_access_token(&self) -> Result<secrecy::SecretString, OmsError> {
        Ok(secrecy::SecretString::from("dhat-token"))
    }
}

fn make_oms() -> OrderManagementSystem {
    OrderManagementSystem::new(
        OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        ),
        OrderRateLimiter::new(u32::MAX),
        Box::new(DhatTokenProvider),
        "100".to_owned(),
    )
}

fn paper_request() -> PlaceOrderRequest {
    PlaceOrderRequest {
        security_id: 13,
        exchange_segment: ExchangeSegment::IdxI,
        transaction_type: TransactionType::Buy,
        order_type: OrderType::Market,
        product_type: ProductType::Intraday,
        validity: OrderValidity::Day,
        quantity: 1,
        price: 0.0,
        trigger_price: 0.0,
        lot_size: 1,
        expiry_date: None,
    }
}

fn poll_ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a dry-run OMS call awaited"),
    }
}

fn record(counter: u64) -> PaperTradeRecord {
    PaperTradeRecord {
        paper_order_counter: counter,
        security_id: 13,
        transaction_type: TransactionType::Buy,
        quantity: 1,
        price: 0.0,
    }
}

fn assert_within(name: &str, bytes: u64, blocks: u64, max_bytes: u64, max_blocks: u64) {
    assert!(
        bytes <= max_bytes && blocks <= max_blocks,
        "{name} allocated {bytes} B / {blocks} blocks WITH the production recorder \
         (budget {max_bytes} B / {max_blocks} blocks)"
    );
}

#[test]
fn dhat_order_path_with_the_production_recorder() {
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Suffix("_duration_ns".to_owned()),
            PROD_DURATION_NS_BUCKETS,
        )
        .expect("valid bucket set") // APPROVED: test
        .build_recorder();
    metrics::set_global_recorder(recorder).expect("no recorder installed yet"); // APPROVED: test

    // Warm-up outside every window: each handle resolves once, the paper-trade
    // queue and its helper thread start once (in `OrderManagementSystem::new`).
    let latency = PlacementLatency::new();
    latency.record_ns(650);
    latency.publish();
    let limiter = OrderRateLimiter::new(u32::MAX);
    let _ = limiter.check();
    let breaker = OrderCircuitBreaker::new();
    let _ = breaker.check();
    let fresh_budget = OrderBudget::new();
    let mut warm = make_oms();
    let _ = poll_ready(warm.place_order(paper_request()));
    flush_paper_trade_log();

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u64 {
                latency.record_ns(std::hint::black_box(i * 97));
            }
        },
    );
    assert_within(
        "PlacementLatency::record_ns",
        bytes,
        blocks,
        BUDGET_BYTES,
        BUDGET_BLOCKS,
    );

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for _ in 0..10_000 {
                std::hint::black_box(limiter.check().is_ok());
                std::hint::black_box(breaker.check().is_ok());
                let mut budget = fresh_budget;
                std::hint::black_box(budget.try_consume(1_000).is_ok());
            }
        },
    );
    assert_within("the OMS gates", bytes, blocks, BUDGET_BYTES, BUDGET_BLOCKS);

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        flush_paper_trade_log,
        || {
            for i in 0..512_u64 {
                log_paper_trade(record(i));
            }
        },
    );
    assert_within(
        "log_paper_trade",
        bytes,
        blocks,
        BUDGET_BYTES,
        BUDGET_BLOCKS,
    );
    flush_paper_trade_log();

    // One fresh book per attempt (built here, outside every window), so a
    // retry after a cross-thread phantom places into an empty book again.
    let mut books: Vec<OrderManagementSystem> = (0..3).map(|_| make_oms()).collect();
    let mut placed_books = Vec::with_capacity(3);
    let max_blocks = PLACEMENTS * PLACEMENT_BLOCKS_PER_ORDER + PLACEMENT_BLOCKS_PER_BOOK;
    let (_, blocks) = dhat_support::measure_with_phantom_retry(
        u64::MAX,
        max_blocks,
        || {},
        || {
            if let Some(mut oms) = books.pop() {
                for _ in 0..PLACEMENTS {
                    let placed = poll_ready(oms.place_order(paper_request()));
                    assert!(placed.is_ok(), "paper placement admits inside the tier");
                }
                // Kept, not dropped: freeing is not what is measured.
                placed_books.push(oms);
            }
        },
    );
    assert!(
        blocks <= max_blocks,
        "{PLACEMENTS} paper placements allocated {blocks} blocks WITH the production \
         recorder; budget {max_blocks} = {PLACEMENT_BLOCKS_PER_ORDER} per placement (the \
         order record's own Strings) + {PLACEMENT_BLOCKS_PER_BOOK} once per book. A \
         recorder histogram (one 64-slot block per 64 samples) or an inline log line on \
         this path adds blocks."
    );
    assert!(
        blocks >= PLACEMENTS * PLACEMENT_BLOCKS_PER_ORDER,
        "{blocks} blocks is below the {PLACEMENT_BLOCKS_PER_ORDER} Strings each placement \
         must allocate: the measurement did not see the placements"
    );
    flush_paper_trade_log();
}
