---
paths:
  - "crates/**/tests/**"
  - "crates/**/*test*"
  - "crates/**/benches/**"
---

# Testing Rules

## Before Every Commit
1. `cargo fmt` — format all code
2. `cargo clippy -- -D warnings` — zero warnings
3. `cargo test` — 100% pass
4. `cargo audit` — zero known CVEs

## CI Pipeline Order
Compile → Lint → Test → Security → Performance → Coverage
Any failure = RED. No exceptions.

## Test Naming
```
fn test_<module>_<function>_<scenario>_<expected_outcome>()
```

## Coverage Thresholds
- ALL crates: 95% minimum (no exceptions)
- Enforced by: `quality/crate-coverage-thresholds.toml` + CI + `make coverage` (99% local)

## Required Test Types (22 Categories — Mechanical Enforcement, SCOPED)
**Canonical definition:** `docs/standards/22-test-types.md` (single source of truth).
**Enforcement:** `scripts/test-coverage-guard.sh` (consolidated guard).
**Called by:** `.claude/hooks/pre-push-gate.sh` Gate 11.

**SCOPED BY DEFAULT:** On `git push`, only changed crates are checked.
Full workspace check only when explicitly requested or on CI PR merge.

1. Smoke Tests — basic construction/defaults (ALL crates)
2. Happy-Path Functionality — core logic works correctly (ALL crates)
3. Error Scenario Tests — every `Result::Err` and `Option::None` path (ALL crates)
4. Edge Case Tests — empty, single, exact boundaries (ALL crates)
5. Stress/Boundary Tests — MAX, zero, overflow, capacity limits
6. Property-Based Tests — `proptest` for invariant verification
7. DHAT Allocation Tests — zero heap allocs on hot path (core)
8. Snapshot/Golden Tests — output comparison against baselines (core)
9. Deterministic Replay — same input, same output (core)
10. Backpressure Tests — channel full, buffer saturated (core)
11. Timeout/Backoff Tests — formula verification
12. Graceful Degradation — system continues when components fail (core)
13. Panic Safety Tests — functions must not panic on garbage input (ALL crates)
14. Financial Overflow/Boundary — price arithmetic at extremes (trading)
15. Loom Concurrency — exhaustive interleaving verification (trading)
16. Serialization/Deserialization — JSON, TOML, CSV round-trip
17. Round-Trip Tests — data survives encode/decode cycle
18. Display/Debug Format — format trait correctness
19. Security Tests — no secrets in logs/errors/messages (core)
20. Deduplication Tests — 4-layer dedup chain verification (core)
21. Schema Validation — Dhan API schema drift detection (common)
22. Integration Tests — end-to-end in `tests/` directory (ALL crates)

**Per-crate requirements:** core=19, trading=13, common=12, storage=8, api=5, app=2.
**Scoped mode:** Missing type in a CHANGED crate blocks push.
**Full mode:** Missing any type across workspace blocks push.

## Documentation
- Every `pub fn/struct/enum/trait` gets doc comment (WHAT and WHY)
- Hot-path items: include `# Performance` section
- `cargo doc --no-deps` must build with zero warnings

## Deep Reference
- **22 test types (canonical):** `docs/standards/22-test-types.md` (single source of truth)
- Testing standards: `docs/standards/testing-standards.md`
- Quality gates: `docs/standards/quality-gates.md` (read for CI/benchmarks)
- Quality taxonomy: `docs/standards/quality-taxonomy.md` (98 dimensions)
