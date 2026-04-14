# Testing Standards — tickvault

> Permanent quality bible. Every test must pass all three principles:
> (1) Zero allocation on hot path (2) O(1) or fail at compile time (3) Every version pinned.
>
> **Canonical 22 test types:** `docs/standards/22-test-types.md` (single source of truth).
> **Enforcement:** `scripts/test-coverage-guard.sh` (mechanical, blocks push).

## 22 Test Categories

### 1. Smoke Tests
Verify basic construction and default states. Every crate must have at least one.

### 2. Happy-Path Functionality
Core business logic: subscriptions, connections, parsing, persistence. Valid inputs produce correct outputs.

### 3. Error Scenario Tests
Every `Result::Err` and `Option::None` path must have a test.

### 4. Edge Case Tests
Empty inputs, single elements, exact boundaries (100 batch, 5000 per conn, 25000 total).

### 5. Stress / Boundary Tests
MAX values, zero values, overflow, capacity limits (25K instruments, 5 connections).

### 6. Property-Based Tests (proptest)
Use `proptest` for invariant verification across input ranges (e.g., instrument counts 0..=25000).

### 7. DHAT Allocation Tests
Runtime proof of zero heap allocations on hot-path operations via `dhat::Profiler`.

### 8. Snapshot / Golden Tests
Output comparison against stored baselines (binary data, JSON format, ILP format).

### 9. Deterministic Replay Tests
Same input must produce identical output. Reproducibility for debugging trading incidents.

### 10. Backpressure Tests
System behavior when channels are full, buffers are saturated. Market open flood scenarios.

### 11. Timeout / Backoff Tests
Formula verification, overflow safety, exhaustion detection. DH-904 backoff correctness.

### 12. Graceful Degradation Tests
System continues operating (reduced capacity) when components fail. QuestDB down, WS disconnect.

### 13. Panic Safety Tests
Functions must never panic, even with garbage input. `catch_unwind`, `should_panic` tests.

### 14. Financial Overflow / Boundary Tests
Price arithmetic, lot sizes, P&L calculations at extreme values. Integer overflow in financial math.

### 15. Loom Concurrency Tests
Exhaustive interleaving verification for shared-state code via `loom::model`.

### 16. Serialization / Deserialization Tests
JSON subscription messages, TOML config, CSV instrument parsing. Round-trip correctness.

### 17. Round-Trip Tests
Data flows from config -> planner -> pool -> connection -> channel without loss.

### 18. Display / Debug Format Tests
Every `Display` and `Debug` impl must have format verification tests. Secret prints `[REDACTED]`.

### 19. Security Tests (Secret Handling)
No secrets in logs, errors, or serialized messages. Token never stored in struct fields.

### 20. Deduplication Tests
Verify 4-layer dedup chain: Planner HashSet -> Registry HashMap -> Pool round-robin -> Cached messages.

### 21. Schema Validation Tests
Dhan API response schemas validated against Rust types. Detects schema drift.

### 22. Integration Tests
End-to-end data flow tests in `crates/*/tests/*.rs` files. Per-crate thresholds enforced.

---

## Quality Gates

| Gate | Requirement |
|------|------------|
| `cargo fmt` | Zero diff |
| `cargo clippy -- -D warnings` | Zero warnings |
| `cargo test --workspace` | All tests pass |
| `cargo doc --no-deps` | Zero warnings |
| No `println!` / `dbg!` | `tracing` macros only |
| No `.unwrap()` in production | `?` with `thiserror` / `anyhow` |
| No unbounded channels | All channels have explicit capacity |

## O(1) Hot-Path Checklist

- [ ] `read.next().await` — O(1) per frame
- [ ] `tokio::time::timeout` — O(1) per iteration
- [ ] `mpsc::send()` — O(1) per frame
- [ ] `HashMap::get()` — O(1) per lookup
- [ ] No `.clone()` on hot path (except frame `data.to_vec()` — unavoidable)
- [ ] No `DashMap` or `dyn Trait` on hot path
- [ ] No sorting, filtering, or allocation per tick

## Security Checklist

- [ ] Token not in any `Debug` / `Display` impl
- [ ] Token not in any error variant message
- [ ] Token not in any `tracing::debug!` / `info!` / `warn!` / `error!`
- [ ] `authenticated_url` is local variable, never stored in struct
- [ ] Cached subscription messages contain only ExchangeSegment + SecurityId
- [ ] `grep expose_secret` shows no new exposure

## Deduplication Guarantee

```
Layer 1: Planner HashSet<u32>      -> seen_ids.insert() blocks dupes
Layer 2: Registry HashMap<u32,...> -> insert() overwrites dupes (safety)
Layer 3: Pool round-robin          -> idx % N assigns each to exactly 1 conn
Layer 4: Cached messages           -> built from per-conn list (unique)
```

Verified by `test_no_duplicate_security_ids` and `test_plan_no_duplicates_progressive_fill`.
