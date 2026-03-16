//! Benchmark: TokenHandle (ArcSwap) read latency.
//!
//! Measures `handle.load()` performance for JWT token reads on the hot path.
//! AUTH-GAP-01: Every tick processor reads the current JWT via `handle.load()`.
//! Budget: < 50ns per read (same tier as registry.get()).

use std::hint::black_box;
use std::sync::Arc;

use arc_swap::ArcSwap;
use criterion::{Criterion, criterion_group, criterion_main};

/// Simulates the `TokenHandle` type used in auth module:
/// `Arc<ArcSwap<Option<TokenState>>>` — here simplified to `Option<String>`.
type TokenHandle = Arc<ArcSwap<Option<String>>>;

fn bench_token_handle_load(c: &mut Criterion) {
    let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(Some(
        "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzUxMiJ9.test-jwt-token".to_string(),
    ))));

    // Warmup: first load initializes hazard pointer (one-time per thread).
    let _warmup = handle.load();

    c.bench_function("token_handle/load", |b| {
        b.iter(|| {
            let guard = handle.load();
            let token = guard.as_ref().as_ref();
            black_box(token);
        });
    });
}

fn bench_token_handle_load_none(c: &mut Criterion) {
    let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
    let _warmup = handle.load();

    c.bench_function("token_handle/load_none", |b| {
        b.iter(|| {
            let guard = handle.load();
            let is_some = guard.as_ref().is_some();
            black_box(is_some);
        });
    });
}

fn bench_token_handle_clone_arc(c: &mut Criterion) {
    let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(Some(
        "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzUxMiJ9.test-jwt-token".to_string(),
    ))));

    c.bench_function("token_handle/clone_arc", |b| {
        b.iter(|| {
            let cloned = Arc::clone(&handle);
            black_box(cloned);
        });
    });
}

criterion_group!(
    benches,
    bench_token_handle_load,
    bench_token_handle_load_none,
    bench_token_handle_clone_arc
);
criterion_main!(benches);
