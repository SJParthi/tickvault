# Test Coverage Gap Analysis

**Date:** 2026-03-16
**Total tests:** 2,832 across 6 crates
**Test type guard:** 59/59 checks passing

## Summary by Crate

| Crate | Src Lines | Tests | Ratio/100L | Integration Files | Threshold Met? |
|-------|-----------|-------|------------|-------------------|----------------|
| common | 9,376 | 307 | 3.2 | 1 (need >=2) | **NO** |
| core | 40,700 | 1,378 | 3.3 | 14 | YES |
| trading | 9,782 | 486 | 4.9 | 9 | YES |
| storage | 8,419 | 222 | 2.6 | 2 | YES |
| api | 2,641 | 41 | 1.5 | 3 | YES |
| app | 3,410 | 23 | 0.6 | 0 (need >=1) | **NO** |

## Critical Gaps (Priority Order)

### 1. app::trading_pipeline — 583 lines, 0 tests

The main pipeline orchestrating the entire boot sequence has zero test coverage.
This is the most critical integration point in the system. Needs:
- Boot sequence smoke tests (CryptoProvider -> Config -> ... -> Shutdown)
- Graceful shutdown tests
- Error cascade tests (what happens when a subsystem fails to init)

### 2. trading::oms::api_client — 1,005 lines, 4 tests

The REST client that places real orders with real money. 4 tests for 1000 lines.
Needs:
- DH-901 token rotation test
- DH-904 exponential backoff test
- DH-905 no-retry test
- DH-906 order error handling test
- Timeout and network error tests
- Rate limit exhaustion tests

### 3. trading::oms::engine — 1,414 lines, 13 tests

Order management engine. ~1 test per 100 lines. Needs:
- Full order lifecycle tests (place -> fill -> position update)
- Error cascade tests (what happens when API client fails mid-order)
- Concurrent order tests
- State machine transition exhaustiveness

### 4. core::pipeline::tick_processor — 2,144 lines, 13 tests

Hot-path tick processing. Needs:
- Property-based tests with fuzzed binary packets
- Throughput stress tests
- Malformed packet handling tests
- Backpressure under sustained high-rate ticks

### 5. common crate — 1 integration test file (needs >=2)

Only `schema_validation.rs`. Needs a second integration test file.
Candidates: instrument_registry round-trip, config validation e2e.

### 6. app crate — 0 integration test files (needs >=1)

No `crates/app/tests/` directory exists. Needs at least one integration test.

### 7. api::middleware — 377 lines, 7 tests

Auth middleware and rate limiting. Security-critical. Needs:
- Auth rejection scenarios (expired token, malformed header, missing header)
- Rate limiting under concurrent requests
- Header sanitization tests

## Systemic Gaps

### Async Test Coverage in trading

52 async functions vs 16 async tests. The OMS engine, API client, and risk
engine all have async paths that are undertested — particularly error handling
in async contexts (timeouts, connection failures, rate limit backoff).

### Property-Based Testing Spread

Only 14 proptest blocks in core, 1 each in trading/storage/api, 0 in app.
Key candidates for proptest:
- `oms/rate_limiter.rs` — invariants under arbitrary request patterns
- `risk/engine.rs` — position sizing with arbitrary inputs
- `pipeline/tick_processor.rs` — binary packet fuzzing

### Loom Concurrency Tests

Only `circuit_breaker.rs` has loom tests. Missing:
- `connection_pool.rs` (1,063 lines, manages multiple WS connections)
- `tick_processor.rs` (concurrent tick processing)

### Error Path / Negative Testing

Only 3 `#[should_panic]` tests across the entire workspace. Dhan API has 10+
error codes (DH-901 through DH-910) plus data API errors (800-814). Error
handling paths need adversarial testing.

## Zero-Test Files

| File | Lines | Risk |
|------|-------|------|
| `app/src/trading_pipeline.rs` | 583 | **CRITICAL** — boot sequence |
| `storage/src/calendar_persistence.rs` | 213 | MEDIUM — market calendar |
| `trading/src/strategy/hot_reload.rs` | 181 | MEDIUM — config reload |
| `common/src/constants.rs` | ~50 | LOW — compile-time constants |
| `core/src/parser/read_helpers.rs` | ~30 | LOW — utility functions |
