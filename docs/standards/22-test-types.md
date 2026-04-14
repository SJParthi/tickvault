# 22 Test Type Standards — Canonical Definition

> **Single source of truth** for all test type categories in tickvault.
> Every other document (testing-standards.md, testing.md rule, quality-taxonomy.md) references THIS file.
>
> **Enforcement:** `scripts/test-coverage-guard.sh` (mechanical, blocks push)
> **Scope:** Per-crate enforcement by default. Full workspace on explicit request or CI PR merge.

## Three Principles (Every Test Must Honor)

1. Zero allocation on hot path
2. O(1) or fail at compile time
3. Every version pinned

---

## The 22 Test Types

### Type 1: Smoke Tests
**What:** Basic construction, default states, struct creation.
**Why:** Catches broken constructors, missing defaults, type mismatches.
**Pattern:** `fn test_` + any of: `_new`, `_default`, `_create`, `_init`, `_construct`, or prefix `smoke_`
**Applies to:** ALL crates (core, trading, common, storage, api, app)

### Type 2: Happy-Path Functionality
**What:** Core business logic works correctly with valid inputs.
**Why:** Proves the system does what it's designed to do.
**Pattern:** `fn test_` + any of: `_success`, `_valid`, `_happy`, `_correct`, `_parse`
**Applies to:** ALL crates

### Type 3: Error Scenario Tests
**What:** Every `Result::Err` and `Option::None` path has a test.
**Why:** Untested error paths panic in production. For a trading system, panic = lost money.
**Pattern:** `fn test_` + any of: `_error`, `_fail`, `_invalid`, `_reject`; OR `is_err()` assertion in test
**Applies to:** ALL crates

### Type 4: Edge Case Tests
**What:** Empty inputs, single elements, exact boundaries, off-by-one.
**Why:** Edge cases are where bugs live. Batch of 100 at exactly 100, 0, and 101.
**Pattern:** `fn test_` + any of: `_empty`, `_single`, `_edge`, `_boundary`, `_exact`
**Applies to:** ALL crates

### Type 5: Stress/Boundary Tests
**What:** MAX values, zero values, overflow, capacity limits (25K instruments, 5 connections).
**Why:** Production runs at scale. Tests must prove behavior at limits.
**Pattern:** `fn test_` + any of: `_max`, `_overflow`, `_zero_`, `_capacity`; OR prefix `stress_`, `boundary_`
**Applies to:** core, trading, common, storage

### Type 6: Property-Based Tests (proptest)
**What:** Randomized invariant verification across input ranges.
**Why:** Finds edge cases humans miss. Essential for parsers and financial math.
**Pattern:** `proptest!` macro block
**Applies to:** core, trading, common

### Type 7: DHAT Allocation Tests
**What:** Proves zero heap allocations on hot-path operations.
**Why:** Principle #1: Zero allocation on hot path. DHAT provides runtime proof.
**Pattern:** `dhat::Profiler::builder().testing().build()` in a `#[test]` function
**Applies to:** core (mandatory), trading (if hot-path code exists)

### Type 8: Snapshot/Golden Tests
**What:** Output comparison against stored baselines (binary data, JSON, ILP format).
**Why:** Detects accidental format changes that break Dhan API integration.
**Pattern:** `fn snapshot_` OR `fn golden_` prefix in test function name
**Applies to:** core

### Type 9: Deterministic Replay Tests
**What:** Same input produces identical output, every time.
**Why:** Reproducibility is essential for debugging trading incidents.
**Pattern:** `fn ` + any of: `replay_`, `deterministic_`; OR `fn test_` + `_idempotent`
**Applies to:** core

### Type 10: Backpressure Tests
**What:** System behavior when channels are full, buffers are saturated.
**Why:** Market open floods data. System must degrade gracefully, not crash.
**Pattern:** `fn ` + `backpressure_` prefix; OR `fn test_` + `_backpressure`
**Applies to:** core

### Type 11: Timeout/Backoff Tests
**What:** Formula verification for retry delays, overflow safety, exhaustion detection.
**Why:** Wrong backoff = DH-904 flood = account suspension. Wrong timeout = hung connection.
**Pattern:** `fn test_` + any of: `_timeout`, `_backoff`, `_retry`, `_delay`
**Applies to:** core, trading

### Type 12: Graceful Degradation Tests
**What:** System continues operating (reduced capacity) when components fail.
**Why:** QuestDB down shouldn't stop tick processing. WS disconnect shouldn't crash OMS.
**Pattern:** `fn ` + `degradation_` prefix; OR `fn test_` + `_degradation`
**Applies to:** core

### Type 13: Panic Safety Tests
**What:** Functions that must never panic, even with garbage input.
**Why:** Panic in production = system crash = no trading = money lost.
**Pattern:** `#[should_panic]` attribute; OR `fn ` + any of: `no_panic`, `does_not_panic`, `doesnt_panic`, `panic_safety`
**Applies to:** ALL crates

### Type 14: Financial Overflow/Boundary Tests
**What:** Price arithmetic, lot sizes, P&L calculations at extreme values.
**Why:** Integer overflow in financial math = catastrophic incorrect order.
**Pattern:** `fn ` + any of: `overflow_`, `boundary_`, `financial_` prefix in `crates/trading/tests/`
**Applies to:** trading

### Type 15: Loom Concurrency Tests
**What:** Exhaustive interleaving verification for shared-state code.
**Why:** Race conditions in OMS = duplicate orders, lost fills.
**Pattern:** `#[cfg(loom)]` block with `loom::model` call
**Applies to:** trading

### Type 16: Serialization/Deserialization Tests
**What:** JSON, TOML, CSV round-trip correctness.
**Why:** Wrong serde attributes = silent data loss in Dhan API calls.
**Pattern:** `fn test_` + any of: `_json`, `_serial`, `_deserial`, `_toml`, `_csv`; OR `serde` usage in test
**Applies to:** common, core, trading

### Type 17: Round-Trip Tests
**What:** Data survives encode → decode cycle without loss.
**Why:** Config load → save → load must be identical. ILP encode → parse must match.
**Pattern:** `fn test_` + any of: `_round_trip`, `_roundtrip`, `_flow`, `_pipeline`
**Applies to:** core, common

### Type 18: Display/Debug Format Tests
**What:** Every `Display` and `Debug` impl produces expected output.
**Why:** Secret must print `[REDACTED]`. Error messages must be actionable.
**Pattern:** `fn test_` + any of: `_display`, `_debug`, `_format`, `_to_string`
**Applies to:** ALL crates (that have Display/Debug impls)

### Type 19: Security Tests (Secret Handling)
**What:** Tokens never leak into logs, errors, Debug output, or serialized messages.
**Why:** Leaked Dhan token = full trading account access by attacker.
**Pattern:** `fn test_` + any of: `_secret`, `_redact`; OR `Secret<` type usage in test code
**Applies to:** core

### Type 20: Deduplication Tests
**What:** 4-layer dedup chain verification. No duplicate instruments in subscriptions.
**Why:** Duplicate subscriptions = wasted bandwidth, incorrect tick counts.
**Pattern:** `fn test_` + any of: `_dedup`, `_duplicate`
**Applies to:** core

### Type 21: Schema Validation Tests
**What:** Dhan API response schemas validated against Rust types.
**Why:** Dhan can change API without notice. Schema drift = silent parse failure during market hours.
**Pattern:** `fn schema_` prefix; OR `fn test_` + `_schema`
**Applies to:** common

### Type 22: Integration Tests
**What:** End-to-end data flow tests in `crates/*/tests/*.rs` files.
**Why:** Unit tests pass individually but integration reveals wiring bugs.
**Pattern:** File count in `crates/<crate>/tests/*.rs` (excluding `mod.rs`) >= threshold
**Thresholds:** core >= 5, trading >= 3, common >= 2, storage >= 2, api >= 1, app >= 1
**Applies to:** ALL crates

---

## Per-Crate Requirements Matrix

| Type | core | trading | common | storage | api | app |
|------|------|---------|--------|---------|-----|-----|
| 1. Smoke | REQ | REQ | REQ | REQ | REQ | REQ |
| 2. Happy-Path | REQ | REQ | REQ | REQ | REQ | — |
| 3. Error Scenario | REQ | REQ | REQ | REQ | REQ | — |
| 4. Edge Case | REQ | REQ | REQ | REQ | — | — |
| 5. Stress/Boundary | REQ | REQ | REQ | REQ | — | — |
| 6. Property (proptest) | REQ | REQ | REQ | — | — | — |
| 7. DHAT Allocation | REQ | — | — | — | — | — |
| 8. Snapshot/Golden | REQ | — | — | — | — | — |
| 9. Deterministic Replay | REQ | — | — | — | — | — |
| 10. Backpressure | REQ | — | — | — | — | — |
| 11. Timeout/Backoff | REQ | REQ | — | — | — | — |
| 12. Graceful Degradation | REQ | — | — | — | — | — |
| 13. Panic Safety | REQ | REQ | REQ | REQ | REQ | — |
| 14. Financial Overflow | — | REQ | — | — | — | — |
| 15. Loom Concurrency | — | REQ | — | — | — | — |
| 16. Serde Tests | REQ | REQ | REQ | — | — | — |
| 17. Round-Trip | REQ | — | REQ | — | — | — |
| 18. Display/Debug | REQ | REQ | REQ | REQ | — | — |
| 19. Security (secrets) | REQ | — | — | — | — | — |
| 20. Deduplication | REQ | — | — | — | — | — |
| 21. Schema Validation | — | — | REQ | — | — | — |
| 22. Integration Tests | REQ | REQ | REQ | REQ | REQ | REQ |

**Required types per crate:**
- core: 20/22
- trading: 13/22
- common: 12/22
- storage: 7/22
- api: 5/22
- app: 2/22

---

## Scoping Rules

1. **Default (git push):** Only changed crates are checked against their required types.
2. **Full workspace:** Only when explicitly requested, `/quality` skill, or CI PR merge.
3. **Done = Done:** Once a crate passes ALL its required types and is NOT modified, it is NOT re-checked.
4. **New pub fn rule:** Every session that adds pub fns must add corresponding tests. Net untested count cannot increase.

## Enforcement Chain

```
Layer 1: scripts/test-coverage-guard.sh    → blocks push if types missing
Layer 2: .claude/hooks/pre-push-gate.sh    → calls test-coverage-guard.sh
Layer 3: .github/workflows/ci.yml          → runs full workspace on PR to main
```

## Cross-References

- Enforcement hook: `.claude/hooks/pre-push-gate.sh` (Gate 11)
- Rule: `.claude/rules/project/testing.md`
- Quality taxonomy: `docs/standards/quality-taxonomy.md` (Category 1)
- Guarantee statement: `docs/standards/guarantee-statement.md`
