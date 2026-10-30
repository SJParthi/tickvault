//! The 2026-10-30 order-path speed work changed HOW five order-path metrics
//! are emitted (handles cached at first use; literal labels instead of a
//! label variable). This test proves WHAT is emitted did not change: under the
//! production Prometheus recorder it drives the real OMS, rate limiter, order
//! budget, circuit breaker and risk engine, renders the exposition text, and
//! checks every affected series by name, labels and count.
//!
//! A cached handle binds to the recorder active at its FIRST use. Each object
//! here is built and first used inside `with_local_recorder`, which is the
//! same order production follows (the recorder is installed at boot, before
//! the order runtime exists).

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use metrics_exporter_prometheus::PrometheusBuilder;
use tickvault_common::order_types::{OrderType, OrderValidity, ProductType, TransactionType};
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::oms::circuit_breaker::OrderCircuitBreaker;
use tickvault_trading::oms::engine::OrderManagementSystem;
use tickvault_trading::oms::rate_limiter::{OrderBudget, OrderRateLimiter};
use tickvault_trading::oms::types::PlaceOrderRequest;
use tickvault_trading::oms::{OmsError, OrderApiClient, TokenProvider};
use tickvault_trading::risk::engine::RiskEngine;

struct ParityTokenProvider;
impl TokenProvider for ParityTokenProvider {
    fn get_access_token(&self) -> Result<secrecy::SecretString, OmsError> {
        Ok(secrecy::SecretString::from("parity-token"))
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

/// The value of the exposition line that starts with `series` + a space.
fn series_value(rendered: &str, series: &str) -> Option<f64> {
    rendered.lines().find_map(|line| {
        line.strip_prefix(series)
            .and_then(|rest| rest.strip_prefix(' '))
            .and_then(|value| value.trim().parse().ok())
    })
}

#[test]
fn order_path_metrics_keep_their_names_labels_and_counts() {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();

    metrics::with_local_recorder(&recorder, || {
        // Three paper placements: placed counter, duration histogram, and
        // the limiter's allowed counter, all through the cached handles.
        let mut oms = OrderManagementSystem::new(
            OrderApiClient::new(
                reqwest::Client::new(),
                "https://api.dhan.co/v2".to_owned(),
                "100".to_owned(),
            ),
            OrderRateLimiter::new(1_000),
            Box::new(ParityTokenProvider),
            "100".to_owned(),
        );
        for _ in 0..3 {
            let placed = poll_ready(oms.place_order(PlaceOrderRequest {
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
            }));
            assert!(placed.is_ok(), "paper placement is admitted");
        }

        // The breaker's state gauge, set by a closed check.
        let breaker = OrderCircuitBreaker::new();
        assert!(breaker.check().is_ok());

        // One refusal per budget tier, each on its own literal label.
        let mut minute = OrderBudget::new();
        for _ in 0..OrderBudget::MAX_PER_MINUTE {
            assert!(minute.try_consume(1_000).is_ok());
        }
        assert!(minute.try_consume(1_000).is_err());
        // 250 a minute for four minutes fills the hour tier (1,000) while
        // every minute window still has room.
        let mut hour = OrderBudget::new();
        let base = 10_000_u64;
        for minute_index in 0..4_u64 {
            for _ in 0..OrderBudget::MAX_PER_MINUTE {
                assert!(hour.try_consume(base + minute_index * 60).is_ok());
            }
        }
        assert!(hour.try_consume(base + 4 * 60).is_err());
        // 1,000 an hour for seven hours fills the day tier (7,000).
        let mut day = OrderBudget::new();
        for hour_index in 0..7_u64 {
            for minute_index in 0..4_u64 {
                for _ in 0..OrderBudget::MAX_PER_MINUTE {
                    let now = base + hour_index * 3_600 + minute_index * 60;
                    assert!(day.try_consume(now).is_ok());
                }
            }
        }
        assert!(day.try_consume(base + 7 * 3_600).is_err());

        // Rejected marks, one per literal reason.
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        risk.update_market_price_in_segment(1, ExchangeSegment::NseFno, f64::NAN);
        risk.update_market_price_in_segment(1, ExchangeSegment::NseFno, f64::INFINITY);
        risk.update_market_price_in_segment(1, ExchangeSegment::NseFno, -1.0);
        risk.update_market_price_in_segment(1, ExchangeSegment::NseFno, 0.0);
    });

    let rendered = handle.render();
    let expect = |series: &str, value: f64| {
        assert_eq!(
            series_value(&rendered, series),
            Some(value),
            "series `{series}` missing or wrong in:\n{rendered}"
        );
    };
    expect("tv_orders_placed_total{mode=\"paper\"}", 3.0);
    expect("tv_rate_limiter_allowed_total", 3.0);
    expect("tv_order_placement_duration_ns_count", 3.0);
    expect("tv_circuit_breaker_state", 0.0);
    expect("tv_oms_order_budget_refused_total{tier=\"minute\"}", 1.0);
    expect("tv_oms_order_budget_refused_total{tier=\"hour\"}", 1.0);
    expect("tv_oms_order_budget_refused_total{tier=\"day\"}", 1.0);
    expect("tv_risk_mark_rejected_total{reason=\"nan\"}", 1.0);
    expect("tv_risk_mark_rejected_total{reason=\"infinite\"}", 1.0);
    expect("tv_risk_mark_rejected_total{reason=\"non_positive\"}", 2.0);
}
