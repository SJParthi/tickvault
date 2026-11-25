//! Order decision path in memory — Criterion benchmarks (2026-10-30).
//!
//! Owner, 30 Oct 2026: every in-memory step of the order decision path must
//! take under 1 microsecond, proven by benches that fail the build if it
//! slips. This file measures each in-memory step the order runtime and the
//! OMS take per event, in paper mode, WITHOUT the network call (out of scope:
//! a broker round trip is milliseconds and never under 1 µs).
//!
//! Budget key `order_path` in `quality/benchmark-budgets.toml` (1,000 ns)
//! substring-matches every id below, so every step here is gated.
//!
//! # Measured with the production metrics recorder installed
//!
//! The production process installs the Prometheus recorder, so every
//! `counter!` / `gauge!` / `histogram!` on these paths does a registry
//! lookup (and a histogram sample can allocate a bucket block). A bench with
//! no recorder would time no-ops and read low. This harness installs the same
//! recorder (`PrometheusBuilder::build_recorder`) before the first sample.
//!
//! # Logging
//!
//! Most benches run with no tracing subscriber, so a span or line costs only
//! its enabled check. Production installs JSON log layers at INFO, and two
//! steps here emit at INFO: `place_order` opens a span on every call, and the
//! risk check opens one too. The paper placement's "PAPER TRADE" line is no
//! longer written on the order step: since 2026-10-30 a helper thread writes
//! it after the order (owner: "Write it after the order"). The
//! `_with_info_log` benches install a JSON INFO subscriber writing to a sink
//! (formatting cost, never disk), so the gate also covers what production
//! pays. The refusal paths that log at WARN are marked `_refused`.
//!
//! # The correlation-id pool
//!
//! Each fresh book here holds 256 pre-drawn correlation ids, and no chunk
//! places more than 200, so `oms_place_paper` always takes the pooled id. A
//! day's 257th placement onward draws its id on demand instead;
//! `correlation_generate_id` gates that extra cost (about 290 ns of OS random
//! call plus one allocation).
//!
//! # The p99 gate
//!
//! Criterion and `bench-gate.sh` read the MEDIAN. After Criterion, under
//! `cargo bench` only, `order_path_tail_gate` times the steady-state steps
//! one call at a time (the house `tail_gate`, also used by the side-store
//! benches) and exits 1 when any p99 is over the `order_path` budget times
//! `BENCH_BUDGET_MULTIPLIER`. Every timed call must give its case's outcome,
//! so a case cannot quietly time a refusal.
//!
//! Run: `cargo bench --bench order_path -p tickvault-app`

use std::future::Future;
use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

#[path = "../../core/benches/support/tail_gate.rs"]
mod tail_gate;

use criterion::{BatchSize, Criterion, criterion_group};
use metrics_exporter_prometheus::PrometheusBuilder;

use tickvault_common::order_types::{
    OrderStatus, OrderType, OrderUpdate, OrderValidity, ProductType, TransactionType,
};
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::oms::circuit_breaker::OrderCircuitBreaker;
use tickvault_trading::oms::engine::OrderManagementSystem;
use tickvault_trading::oms::idempotency::CorrelationTracker;
use tickvault_trading::oms::rate_limiter::{OrderBudget, OrderRateLimiter};
use tickvault_trading::oms::state_machine::{is_valid_transition, parse_order_status};
use tickvault_trading::oms::types::PlaceOrderRequest;
use tickvault_trading::oms::{OmsError, OrderApiClient, TokenProvider};
use tickvault_trading::risk::engine::RiskEngine;

fn install_production_recorder() {
    let recorder = PrometheusBuilder::new().build_recorder();
    // A second install (another bench group in the same process) is refused;
    // the first one stays in place, which is what every group needs.
    let _ = metrics::set_global_recorder(recorder);
}

struct BenchTokenProvider;
impl TokenProvider for BenchTokenProvider {
    fn get_access_token(&self) -> Result<secrecy::SecretString, OmsError> {
        Ok(secrecy::SecretString::from("bench-token"))
    }
}

fn make_oms() -> OrderManagementSystem {
    make_oms_with(reqwest::Client::new())
}

/// A paper book over a shared HTTP client (a clone is an `Arc` bump), so the
/// p99 gate can build its thousand books without a thousand TLS setups.
fn make_oms_with(http: reqwest::Client) -> OrderManagementSystem {
    let api_client =
        OrderApiClient::new(http, "https://api.dhan.co/v2".to_owned(), "100".to_owned());
    OrderManagementSystem::new(
        api_client,
        OrderRateLimiter::new(u32::MAX),
        Box::new(BenchTokenProvider),
        "100".to_owned(),
    )
}

fn paper_request(security_id: u64) -> PlaceOrderRequest {
    PlaceOrderRequest {
        security_id,
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

fn traded_update(order_no: &str, traded_qty: i64) -> OrderUpdate {
    let mut update: OrderUpdate = match serde_json::from_str("{}") {
        Ok(update) => update,
        Err(err) => panic!("OrderUpdate fields are serde-default: {err}"),
    };
    update.exchange = "NSE".to_owned();
    update.segment = "D".to_owned();
    update.order_no = order_no.to_owned();
    update.status = "TRADED".to_owned();
    update.traded_qty = traded_qty;
    update.avg_traded_price = 123.45;
    update.source = "P".to_owned();
    update
}

fn bench_oms_gates(c: &mut Criterion) {
    install_production_recorder();

    c.bench_function("order_path/transition_valid", |b| {
        b.iter(|| {
            black_box(is_valid_transition(
                black_box(OrderStatus::Pending),
                black_box(OrderStatus::Traded),
            ))
        });
    });
    c.bench_function("order_path/parse_status_part_traded", |b| {
        b.iter(|| black_box(parse_order_status(black_box("PART_TRADED"))));
    });

    // Per-second GCRA, admit arm (quota large enough never to refuse).
    let limiter = OrderRateLimiter::new(u32::MAX);
    c.bench_function("order_path/rate_limiter_admit", |b| {
        b.iter(|| black_box(limiter.check().is_ok()));
    });
    // Refusal arm (one-per-second quota, spent once before timing).
    let spent = OrderRateLimiter::new(1);
    let _ = spent.check();
    c.bench_function("order_path/rate_limiter_refused", |b| {
        b.iter(|| black_box(spent.check().is_err()));
    });

    // Minute / hour / day tiers: admit from a fresh budget (Copy, so each
    // iteration starts from the same state without a batch setup).
    let fresh = OrderBudget::new();
    c.bench_function("order_path/order_budget_admit", |b| {
        b.iter(|| {
            let mut budget = black_box(fresh);
            black_box(budget.try_consume(black_box(1_000)).is_ok())
        });
    });
    // Minute tier full: the all-or-nothing refusal.
    let mut full_minute = OrderBudget::new();
    for _ in 0..OrderBudget::MAX_PER_MINUTE {
        let _ = full_minute.try_consume(1_000);
    }
    c.bench_function("order_path/order_budget_refused", |b| {
        b.iter(|| {
            let mut budget = black_box(full_minute);
            black_box(budget.try_consume(black_box(1_000)).is_err())
        });
    });

    let breaker = OrderCircuitBreaker::new();
    c.bench_function("order_path/circuit_breaker_check_closed", |b| {
        b.iter(|| black_box(breaker.check().is_ok()));
    });
    c.bench_function("order_path/circuit_breaker_record_success", |b| {
        b.iter(|| breaker.record_success());
    });
}

fn bench_idempotency(c: &mut Criterion) {
    install_production_recorder();

    let mut tracker = CorrelationTracker::new();
    let ids: Vec<String> = (0..1_000).map(|i| format!("{i:030}")).collect();
    for id in &ids {
        tracker.track(id.clone(), format!("PAPER-{id}"));
    }
    let mut i = 0_usize;
    c.bench_function("order_path/correlation_get_hit", |b| {
        b.iter(|| {
            i = (i + 7) % ids.len();
            black_box(tracker.get_order_id(black_box(&ids[i])).is_some())
        });
    });
    // The on-demand draw a placement makes once the day's 256 pooled ids are
    // used up (`CorrelationTracker::next_id` falls back to this).
    let drawer = CorrelationTracker::new();
    c.bench_function("order_path/correlation_generate_id", |b| {
        b.iter(|| black_box(drawer.generate_id()));
    });
    c.bench_function("order_path/correlation_get_miss", |b| {
        b.iter(|| black_box(tracker.get_order_id(black_box("not-a-tracked-correlation-id"))));
    });
    // Replace arm: the caller's two Strings are built outside the timing.
    c.bench_function("order_path/correlation_track_replace", |b| {
        b.iter_batched(
            || (ids[3].clone(), "PAPER-replaced".to_owned()),
            |(corr, order)| black_box(tracker.track(corr, order)),
            BatchSize::SmallInput,
        );
    });
}

fn bench_risk(c: &mut Criterion) {
    install_production_recorder();

    let mut risk = RiskEngine::new(2.0, 1_000_000, 1_000_000_000.0);
    for sid in 0..1_000_u64 {
        risk.record_fill_in_segment(sid, ExchangeSegment::NseFno, 1, 100.0, 25);
        risk.update_market_price_in_segment(sid, ExchangeSegment::NseFno, 101.0);
    }

    c.bench_function("order_path/risk_check_approved", |b| {
        b.iter(|| {
            black_box(
                risk.check_order_in_segment(black_box(500), ExchangeSegment::NseFno, black_box(1))
                    .is_approved(),
            )
        });
    });

    // The same check with production's log shape (the `#[instrument]` span
    // on `check_order_in_segment` is created and formatted).
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::sink)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        c.bench_function("order_path/risk_check_approved_with_info_log", |b| {
            b.iter(|| {
                black_box(
                    risk.check_order_in_segment(black_box(500), ExchangeSegment::NseFno, 1)
                        .is_approved(),
                )
            });
        });
    });

    // A fill that adds to a held position, then the reducing fill, so net lots
    // return to the same value every iteration pair (the average price and
    // realized P&L drift, bounded).
    c.bench_function("order_path/risk_record_fill_held", |b| {
        b.iter(|| {
            risk.record_fill_in_segment(black_box(500), ExchangeSegment::NseFno, 1, 100.5, 25);
            risk.record_fill_in_segment(black_box(500), ExchangeSegment::NseFno, -1, 100.5, 25);
        });
    });

    let mut price = 100.0_f64;
    c.bench_function("order_path/risk_mark_held", |b| {
        b.iter(|| {
            price = if price > 110.0 { 100.0 } else { price + 0.05 };
            risk.update_market_price_in_segment(
                black_box(500),
                ExchangeSegment::NseFno,
                black_box(price),
            );
        });
    });

    c.bench_function("order_path/risk_unrealized_total", |b| {
        b.iter(|| black_box(risk.total_unrealized_pnl()));
    });
    c.bench_function("order_path/risk_daily_loss_eval", |b| {
        b.iter(|| black_box(risk.evaluate_daily_loss_halt()));
    });
    c.bench_function("order_path/risk_net_lots_probe", |b| {
        b.iter(|| black_box(risk.net_lots_for_in_segment(black_box(500), ExchangeSegment::NseFno)));
    });
}

/// Polls a future once with a no-op waker. Every dry-run OMS call returns
/// before its first `.await` (no network leg exists on that branch), so one
/// poll completes it and the bench times the OMS, not an executor.
fn poll_ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a dry-run OMS call awaited; the bench premise is broken"),
    }
}

/// Orders per fresh book. Below the broker's 250-per-minute tier, so no
/// placement in a chunk is refused by `OrderBudget` (the minute tier reads the
/// wall clock, and a refused placement would time the refusal instead).
const PLACEMENTS_PER_BOOK: u64 = 200;

fn bench_oms_engine(c: &mut Criterion) {
    install_production_recorder();

    // Paper placement: every gate above, the correlation id, the order record.
    // A fresh book per chunk (built outside the timing) keeps every placement
    // inside the per-minute tier; the timing covers placements only.
    c.bench_function("order_path/oms_place_paper", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut done = 0_u64;
            while done < iters {
                let chunk = (iters - done).min(PLACEMENTS_PER_BOOK);
                let mut oms = make_oms();
                let start = Instant::now();
                for _ in 0..chunk {
                    black_box(poll_ready(oms.place_order(paper_request(13))).is_ok());
                }
                total += start.elapsed();
                done += chunk;
            }
            total
        });
    });

    // A TRADED update for a tracked paper order: the fill event path.
    c.bench_function("order_path/oms_order_update_fill", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut done = 0_u64;
            while done < iters {
                let chunk = (iters - done).min(PLACEMENTS_PER_BOOK);
                let mut oms = make_oms();
                let updates: Vec<OrderUpdate> = (0..chunk)
                    .map(|_| match poll_ready(oms.place_order(paper_request(13))) {
                        Ok(order_id) => traded_update(&order_id, 1),
                        Err(err) => panic!("paper placement: {err}"),
                    })
                    .collect();
                let start = Instant::now();
                for update in &updates {
                    black_box(oms.handle_order_update(update).is_ok());
                }
                total += start.elapsed();
                done += chunk;
            }
            total
        });
    });

    // An update for an order this book never placed (the ignore path).
    let mut oms = make_oms();
    let unknown = traded_update("UNKNOWN-1", 1);
    // The same placement with production's log shape: a JSON subscriber at
    // INFO writing to a sink, set for this thread only. Covers the
    // `place_order` span; the "PAPER TRADE" line goes to the helper thread.
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::sink)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        c.bench_function("order_path/oms_place_paper_with_info_log", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                let mut done = 0_u64;
                while done < iters {
                    let chunk = (iters - done).min(PLACEMENTS_PER_BOOK);
                    let mut oms = make_oms();
                    let start = Instant::now();
                    for _ in 0..chunk {
                        black_box(poll_ready(oms.place_order(paper_request(13))).is_ok());
                    }
                    total += start.elapsed();
                    done += chunk;
                }
                total
            });
        });
    });

    c.bench_function("order_path/oms_order_update_unknown", |b| {
        b.iter(|| black_box(oms.handle_order_update(black_box(&unknown)).is_ok()));
    });
}

/// The per-call p99 gate (the house `tail_gate`, also used by the
/// side-store benches): each case is timed one call at a time over
/// `tail_gate::PERCENTILE_SAMPLES` calls with the production recorder
/// installed, and a p99 over the `order_path` budget (1,000 ns, times
/// `BENCH_BUDGET_MULTIPLIER`) exits 1. Each timed call must also give its
/// case's outcome, so a case cannot quietly time a refusal.
fn order_path_tail_gate() -> Result<(), String> {
    install_production_recorder();
    let mut gate = tail_gate::Gate::new("order_path", "order_path")?;

    let limiter = OrderRateLimiter::new(u32::MAX);
    gate.case("rate_limiter_admit", |_| limiter.check().is_ok());
    let breaker = OrderCircuitBreaker::new();
    gate.case("circuit_breaker_check_closed", |_| breaker.check().is_ok());
    let fresh = OrderBudget::new();
    gate.case("order_budget_admit", |_| {
        let mut budget = black_box(fresh);
        budget.try_consume(1_000).is_ok()
    });
    gate.case("transition_valid", |_| {
        is_valid_transition(
            black_box(OrderStatus::Pending),
            black_box(OrderStatus::Traded),
        )
    });

    let mut risk = RiskEngine::new(2.0, 1_000_000, 1_000_000_000.0);
    for sid in 0..1_000_u64 {
        risk.record_fill_in_segment(sid, ExchangeSegment::NseFno, 1, 100.0, 25);
        risk.update_market_price_in_segment(sid, ExchangeSegment::NseFno, 101.0);
    }
    gate.case("risk_check_approved", |_| {
        risk.check_order_in_segment(500, ExchangeSegment::NseFno, 1)
            .is_approved()
    });
    gate.case("risk_record_fill_held", |n| {
        let lots = if n % 2 == 0 { 1 } else { -1 };
        risk.record_fill_in_segment(500, ExchangeSegment::NseFno, lots, 100.5, 25);
        true
    });
    gate.case("risk_mark_held", |n| {
        let price = 100.0 + (n % 200) as f64 * 0.05;
        risk.update_market_price_in_segment(500, ExchangeSegment::NseFno, price);
        true
    });
    gate.case("risk_daily_loss_eval", |_| !risk.evaluate_daily_loss_halt());

    // Paper placement and its fill update, one book at a time. Each book is
    // built before its timing, takes PLACEMENTS_PER_BOOK - 1 timed
    // placements (below the broker's per-minute tier, so none is refused)
    // and then a timed TRADED update for each, and is freed before the next
    // book is built, so the next one reuses that memory: the steady state of
    // a long-running process. The first placement into each book is made
    // before the timing: it first-touches the book's fresh heap memory, a
    // cost production pays once per process.
    let http = reqwest::Client::new();
    let per_book = PLACEMENTS_PER_BOOK as usize - 1;
    let mut place = Vec::with_capacity(tail_gate::PERCENTILE_SAMPLES + per_book);
    let mut fill = Vec::with_capacity(tail_gate::PERCENTILE_SAMPLES + per_book);
    let mut off_path = 0_u64;
    let mut ids: Vec<String> = Vec::with_capacity(per_book);
    while place.len() < tail_gate::PERCENTILE_SAMPLES {
        let mut oms = make_oms_with(http.clone());
        off_path += u64::from(poll_ready(oms.place_order(paper_request(13))).is_err());
        ids.clear();
        for _ in 0..per_book {
            let request = paper_request(13);
            let start = Instant::now();
            let placed = poll_ready(oms.place_order(request));
            place.push(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
            match placed {
                Ok(order_id) => ids.push(order_id),
                Err(_) => off_path += 1,
            }
        }
        let updates: Vec<OrderUpdate> = ids.iter().map(|id| traded_update(id, 1)).collect();
        for update in &updates {
            let start = Instant::now();
            let outcome = oms.handle_order_update(update);
            fill.push(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
            off_path += u64::from(!matches!(outcome, Ok(Some(_))));
        }
        drop(updates);
        drop(oms);
        tickvault_trading::oms::placement_telemetry::flush_paper_trade_log();
    }
    let mut failures = Vec::new();
    if off_path > 0 {
        failures.push(format!(
            "{off_path} placement or fill calls took another path"
        ));
    }
    let ceiling = tail_gate::ceiling_ns("order_path")?;
    for (name, mut samples) in [
        ("oms_place_paper_warm_book", place),
        ("oms_order_update_fill", fill),
    ] {
        samples.sort_unstable();
        let p99 = tail_gate::quantile(&samples, 99, 100);
        let ok = tail_gate::within(&samples, ceiling);
        println!(
            "order_path/{name}: p50 = {} ns, p99 = {p99} ns, p99.9 = {} ns, max = {} ns over {} \
             calls (one Instant pair inside each sample); p99 ceiling {ceiling:.0} ns: {}",
            tail_gate::quantile(&samples, 1, 2),
            tail_gate::quantile(&samples, 999, 1_000),
            samples.last().copied().unwrap_or(0),
            samples.len(),
            if ok { "PASS" } else { "FAIL" },
        );
        if !ok {
            failures.push(format!("{name} p99 {p99} ns"));
        }
    }
    let book_cases = if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "order_path over {ceiling:.0} ns: {}",
            failures.join(", ")
        ))
    };
    match (gate.finish(), book_cases) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(a), Ok(())) | (Ok(()), Err(a)) => Err(a),
        (Err(a), Err(b)) => Err(format!("{a}; {b}")),
    }
}

criterion_group!(
    benches,
    bench_oms_gates,
    bench_idempotency,
    bench_risk,
    bench_oms_engine
);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    tail_gate::run_under_bench("order_path", order_path_tail_gate);
}
