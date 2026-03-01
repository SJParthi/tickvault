# Testing Standards — dhan-live-trader

> Permanent quality bible. Every test must pass all three principles:
> (1) Zero allocation on hot path (2) O(1) or fail at compile time (3) Every version pinned.

## 19 Test Categories

### 1. Smoke Tests
Verify basic construction and default states. Every module must have at least one.

### 2. Functionality Tests — Happy Path
Core business logic: subscriptions, connections, parsing, persistence.

### 3. Error Scenario Tests
Every `Result::Err` and `Option::None` path must have a test.

### 4. Stress / Boundary Tests
MAX values, zero values, overflow, capacity limits (25K instruments, 5 connections).

### 5. Property-Based Tests (proptest)
Use `proptest` for invariant verification across input ranges (e.g., instrument counts 0..=25000).

### 6. Performance / O(1) Verification
Code review confirms hot-path operations are O(1). No `.clone()`, `DashMap`, or `dyn` on hot path.

### 7. Security Tests
No secrets in logs, errors, or serialized messages. Token never stored in struct fields.

### 8. Deduplication Tests
Verify 4-layer dedup chain: Planner HashSet -> Registry HashMap -> Pool round-robin -> Cached messages.

### 9. Concurrency Tests
Verify `Mutex`, `AtomicU64`, `ArcSwap`, `mpsc` usage is safe and contention-free.

### 10. Configuration Validation Tests
Every config field with a constraint must have a validation test (zero, negative, overflow).

### 11. Serialization / Deserialization Tests
JSON subscription messages, TOML config, CSV instrument parsing.

### 12. Round-Trip Tests
Data flows from config -> planner -> pool -> connection -> channel without loss.

### 13. Determinism Tests
Same input must produce identical output. Sort orders must be deterministic.

### 14. Edge Case Tests
Empty inputs, single elements, exact boundaries (100 batch, 5000 per conn, 25000 total).

### 15. Regression Tests
Every bug fix must include a test that would have caught the original bug.

### 16. Display / Debug Tests
Every `Display` and `Debug` impl must have format verification tests.

### 17. Timeout / Backoff Tests
Formula verification, overflow safety, exhaustion detection.

### 18. Monitoring Tests
Health snapshots, reconnection counts, capacity utilization percentages.

### 19. Integration Smoke Tests
End-to-end data flow from config to channel (without real WebSocket connections).

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
