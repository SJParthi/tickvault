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
- core/trading crates: 95%
- common/storage/api crates: 90%
- app/integration: 80%

## Required Test Types
- Unit (every fn), Integration (module boundaries), Property-based (parsers, math)
- Boundary (numerics), Error path (every Result fn), Concurrency (Loom for shared state)
- Benchmark (Criterion for hot-path), Fuzz (cargo-fuzz for external input)

## Documentation
- Every `pub fn/struct/enum/trait` gets doc comment (WHAT and WHY)
- Hot-path items: include `# Performance` section
- `cargo doc --no-deps` must build with zero warnings

## Deep Reference
Read `docs/reference/quality_gates.md` ONLY when setting up CI or benchmarks.
