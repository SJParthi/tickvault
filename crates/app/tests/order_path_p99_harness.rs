//! Per-call latency distribution (p50 / p99 / p99.9 / max) for the in-memory
//! order decision path, with the production metrics recorder installed.
//!
//! Criterion (`benches/order_path.rs`) reports a median over batches and is
//! what the bench gate reads. The owner's target is a p99, which a median
//! cannot show, so this harness times every call on its own and prints the
//! tail. `#[ignore]`d, like every wall-clock harness in this workspace: a
//! timing assertion on a shared CI runner would be a flaky gate. Run:
//!
//! `cargo test --release -p tickvault-app --test order_path_p99_harness -- --ignored --nocapture`
//!
//! Each call is bracketed by two `Instant::now()` reads, which add about
//! 20-40 ns to every sample on an x86 VM; the printed figures include that.

use std::future::Future;
use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use metrics_exporter_prometheus::PrometheusBuilder;
use tickvault_common::order_types::{
    OrderStatus, OrderType, OrderUpdate, OrderValidity, ProductType, TransactionType,
};
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::oms::circuit_breaker::OrderCircuitBreaker;
use tickvault_trading::oms::engine::OrderManagementSystem;
use tickvault_trading::oms::idempotency::CorrelationTracker;
use tickvault_trading::oms::rate_limiter::{OrderBudget, OrderRateLimiter};
use tickvault_trading::oms::state_machine::is_valid_transition;
use tickvault_trading::oms::types::PlaceOrderRequest;
use tickvault_trading::oms::{OmsError, OrderApiClient, TokenProvider};
use tickvault_trading::risk::engine::RiskEngine;

const SAMPLES: usize = 100_000;
/// Below the broker's 250-per-minute tier (see the bench for why).
const PLACEMENTS_PER_BOOK: usize = 200;

struct HarnessTokenProvider;
impl TokenProvider for HarnessTokenProvider {
    fn get_access_token(&self) -> Result<secrecy::SecretString, OmsError> {
        Ok(secrecy::SecretString::from("harness-token"))
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
        Box::new(HarnessTokenProvider),
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

fn traded_update(order_no: &str) -> OrderUpdate {
    let mut update: OrderUpdate = serde_json::from_str("{}").expect("serde-default"); // APPROVED: test
    update.exchange = "NSE".to_owned();
    update.segment = "D".to_owned();
    update.order_no = order_no.to_owned();
    update.status = "TRADED".to_owned();
    update.traded_qty = 1;
    update.avg_traded_price = 123.45;
    update.source = "P".to_owned();
    update
}

fn poll_ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a dry-run OMS call awaited"),
    }
}

/// Times `call` once per sample and returns the samples in nanoseconds.
fn sample(mut call: impl FnMut()) -> Vec<u64> {
    for _ in 0..1_000 {
        call();
    }
    let mut out = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        call();
        out.push(start.elapsed().as_nanos() as u64);
    }
    out
}

struct Row {
    name: &'static str,
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

fn summarize(name: &'static str, mut samples: Vec<u64>) -> Row {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize];
    Row {
        name,
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: samples[samples.len() - 1],
    }
}

#[test]
#[ignore = "wall-clock harness; run by hand with --release --ignored --nocapture"]
fn order_path_per_call_latency_distribution() {
    let _ = metrics::set_global_recorder(PrometheusBuilder::new().build_recorder());
    let mut rows = Vec::new();

    rows.push(summarize(
        "transition_valid",
        sample(|| {
            black_box(is_valid_transition(
                black_box(OrderStatus::Pending),
                black_box(OrderStatus::Traded),
            ));
        }),
    ));

    let limiter = OrderRateLimiter::new(u32::MAX);
    rows.push(summarize(
        "rate_limiter_admit",
        sample(|| {
            black_box(limiter.check().is_ok());
        }),
    ));

    let fresh = OrderBudget::new();
    rows.push(summarize(
        "order_budget_admit",
        sample(|| {
            let mut budget = black_box(fresh);
            black_box(budget.try_consume(1_000).is_ok());
        }),
    ));

    let breaker = OrderCircuitBreaker::new();
    rows.push(summarize(
        "circuit_breaker_check_closed",
        sample(|| {
            black_box(breaker.check().is_ok());
        }),
    ));

    let tracker = CorrelationTracker::new();
    rows.push(summarize(
        "correlation_generate_id",
        sample(|| {
            black_box(tracker.generate_id());
        }),
    ));

    let mut risk = RiskEngine::new(2.0, 1_000_000, 1_000_000_000.0);
    for sid in 0..1_000_u64 {
        risk.record_fill_in_segment(sid, ExchangeSegment::NseFno, 1, 100.0, 25);
        risk.update_market_price_in_segment(sid, ExchangeSegment::NseFno, 101.0);
    }
    rows.push(summarize(
        "risk_check_approved",
        sample(|| {
            black_box(
                risk.check_order_in_segment(500, ExchangeSegment::NseFno, 1)
                    .is_approved(),
            );
        }),
    ));
    let mut flip = 1_i32;
    rows.push(summarize(
        "risk_record_fill_held",
        sample(|| {
            risk.record_fill_in_segment(500, ExchangeSegment::NseFno, flip, 100.5, 25);
            flip = -flip;
        }),
    ));
    let mut price = 100.0_f64;
    rows.push(summarize(
        "risk_mark_held",
        sample(|| {
            price = if price > 110.0 { 100.0 } else { price + 0.05 };
            risk.update_market_price_in_segment(500, ExchangeSegment::NseFno, price);
        }),
    ));
    rows.push(summarize(
        "risk_daily_loss_eval",
        sample(|| {
            black_box(risk.evaluate_daily_loss_halt());
        }),
    ));

    // Placement and fill: a fresh book every PLACEMENTS_PER_BOOK calls (built
    // outside the timed region), so every timed placement is admitted.
    let mut place = Vec::with_capacity(SAMPLES);
    // The first order into a book created just before it: fresh hash-table
    // memory, touched for the first time. In production this is the first
    // order after the process starts (once per process), so it is reported
    // on its own row instead of inside the warm-book distribution.
    let mut first_into_fresh_book = Vec::with_capacity(SAMPLES / PLACEMENTS_PER_BOOK + 1);
    let mut fill = Vec::with_capacity(SAMPLES);
    while place.len() < SAMPLES {
        let mut oms = make_oms();
        let mut ids = Vec::with_capacity(PLACEMENTS_PER_BOOK);
        for index in 0..PLACEMENTS_PER_BOOK {
            let request = paper_request();
            let start = Instant::now();
            let placed = poll_ready(oms.place_order(request));
            let elapsed = start.elapsed().as_nanos() as u64;
            if index == 0 {
                first_into_fresh_book.push(elapsed);
            } else {
                place.push(elapsed);
            }
            ids.push(placed.expect("paper placement admits inside the tier")); // APPROVED: test
        }
        let updates: Vec<OrderUpdate> = ids.iter().map(|id| traded_update(id)).collect();
        for update in &updates {
            let start = Instant::now();
            let outcome = oms.handle_order_update(update);
            fill.push(start.elapsed().as_nanos() as u64);
            assert!(
                matches!(outcome, Ok(Some(_))),
                "a TRADED update yields a fill"
            );
        }
    }
    rows.push(summarize("oms_place_paper_warm_book", place));
    rows.push(summarize(
        "oms_place_first_into_fresh_book",
        first_into_fresh_book,
    ));
    rows.push(summarize("oms_order_update_fill", fill));

    // The same warm placement and risk check with a JSON log subscriber at
    // INFO writing to a sink. Production logs one INFO line per paper
    // placement ("PAPER TRADE") and opens an INFO span per risk check, so the
    // rows above (no subscriber) leave that cost out. The sink drops the
    // bytes, so this measures the formatting, never the disk.
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::sink)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut logged = Vec::with_capacity(SAMPLES);
        while logged.len() < SAMPLES {
            let mut oms = make_oms();
            let _ = poll_ready(oms.place_order(paper_request()));
            for _ in 1..PLACEMENTS_PER_BOOK {
                let request = paper_request();
                let start = Instant::now();
                let placed = poll_ready(oms.place_order(request));
                logged.push(start.elapsed().as_nanos() as u64);
                assert!(placed.is_ok(), "paper placement admits inside the tier");
            }
        }
        rows.push(summarize("oms_place_paper_with_info_log", logged));
        rows.push(summarize(
            "risk_check_with_info_log",
            sample(|| {
                black_box(
                    risk.check_order_in_segment(500, ExchangeSegment::NseFno, 1)
                        .is_approved(),
                );
            }),
        ));
    });

    let header = format!(
        "{:<30} {:>8} {:>8} {:>8} {:>10}",
        "step (ns)", "p50", "p99", "p99.9", "max"
    );
    let mut report = String::from(&header);
    for row in &rows {
        report.push('\n');
        report.push_str(&format!(
            "{:<30} {:>8} {:>8} {:>8} {:>10}",
            row.name, row.p50, row.p99, row.p999, row.max
        ));
    }
    // The report is the deliverable of this harness; it is read by hand.
    #[allow(clippy::print_stdout)]
    // APPROVED: wall-clock harness output, run by hand with --nocapture
    {
        println!("{report}");
    }
}
