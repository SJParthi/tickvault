//! Benchmark: OMS state machine transition validation latency.
//!
//! Measures `is_valid_transition()` performance — must be O(1) with no allocation.
//! Budget from quality/benchmark-budgets.toml: < 100ns per transition check.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::order_types::OrderStatus;
use tickvault_trading::oms::state_machine::is_valid_transition;

/// All valid transitions in the order lifecycle DAG.
const VALID_TRANSITIONS: &[(OrderStatus, OrderStatus)] = &[
    (OrderStatus::Transit, OrderStatus::Pending),
    (OrderStatus::Transit, OrderStatus::Rejected),
    (OrderStatus::Pending, OrderStatus::Confirmed),
    (OrderStatus::Pending, OrderStatus::PartTraded),
    (OrderStatus::Pending, OrderStatus::Traded),
    (OrderStatus::Pending, OrderStatus::Cancelled),
    (OrderStatus::Pending, OrderStatus::Rejected),
    (OrderStatus::Pending, OrderStatus::Expired),
    (OrderStatus::Confirmed, OrderStatus::PartTraded),
    (OrderStatus::Confirmed, OrderStatus::Traded),
    (OrderStatus::Confirmed, OrderStatus::Cancelled),
    (OrderStatus::Confirmed, OrderStatus::Expired),
    (OrderStatus::PartTraded, OrderStatus::Traded),
    (OrderStatus::PartTraded, OrderStatus::Cancelled),
    (OrderStatus::PartTraded, OrderStatus::Expired),
    (OrderStatus::Triggered, OrderStatus::Pending),
    (OrderStatus::Triggered, OrderStatus::Traded),
    (OrderStatus::Triggered, OrderStatus::Cancelled),
    (OrderStatus::Triggered, OrderStatus::Rejected),
];

fn bench_valid_transition(c: &mut Criterion) {
    c.bench_function("oms/state_transition", |b| {
        b.iter(|| {
            for &(from, to) in VALID_TRANSITIONS {
                black_box(is_valid_transition(black_box(from), black_box(to)));
            }
        });
    });
}

fn bench_invalid_transition(c: &mut Criterion) {
    c.bench_function("oms/invalid_transition", |b| {
        b.iter(|| {
            // Terminal states: all outgoing transitions are invalid
            black_box(is_valid_transition(
                black_box(OrderStatus::Traded),
                black_box(OrderStatus::Pending),
            ));
            black_box(is_valid_transition(
                black_box(OrderStatus::Rejected),
                black_box(OrderStatus::Pending),
            ));
            black_box(is_valid_transition(
                black_box(OrderStatus::Cancelled),
                black_box(OrderStatus::Pending),
            ));
            black_box(is_valid_transition(
                black_box(OrderStatus::Expired),
                black_box(OrderStatus::Pending),
            ));
        });
    });
}

criterion_group!(benches, bench_valid_transition, bench_invalid_transition);
criterion_main!(benches);
