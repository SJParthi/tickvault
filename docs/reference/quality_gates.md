# Quality Gates — Full Reference

> Rules summary: `.claude/rules/testing.md` + `.claude/rules/hot-path.md` (auto-loaded).
> This doc is for deep implementation detail only — CI pipeline, lint config, benchmark baselines.

## Gate 1: Compile-Time Lint Gates (every lib.rs)
```rust
#![deny(clippy::unwrap_used)]          // .unwrap() → compiler error
#![deny(clippy::print_stdout)]         // println! → compiler error
#![deny(clippy::print_stderr)]         // eprintln! → compiler error
#![deny(clippy::dbg_macro)]            // dbg! → compiler error
#![deny(clippy::todo)]                 // todo!() → compiler error
#![deny(clippy::unimplemented)]        // unimplemented!() → compiler error
#![deny(clippy::expect_used)]          // .expect() → compiler error
#![deny(clippy::panic)]               // panic!() → compiler error
#![deny(clippy::large_futures)]        // Large futures detection
#![deny(clippy::large_stack_arrays)]   // Stack overflow protection
#![warn(clippy::pedantic)]             // Pedantic warnings
#![warn(clippy::nursery)]              // Nursery warnings
#![warn(missing_docs)]                 // Missing doc comments
```
**Exceptions:** `app` crate main.rs may use `unwrap`. Test modules may use `unwrap`/`panic`.

## Gate 2: Test Coverage & Types

Coverage thresholds and required test types: see `.claude/rules/testing.md` (auto-loaded).

**Tool:** cargo-llvm-cov (source-based LLVM instrumentation) with `--fail-under-lines 99`. Coverage exclusions ONLY on generated code/FFI.
**Heap profiling:** dhat for all hot-path functions (in addition to Criterion benchmarks).

## Gate 4: CI Pipeline (6 stages)
```
Stage 1 — Compile:   cargo build --release + cross-compile x86_64
Stage 2 — Lint:      cargo fmt --check + clippy -D warnings + clippy perf
Stage 3 — Test:      cargo test + --ignored
Stage 4 — Security:  cargo audit + git-secrets --scan
Stage 5 — Perf:      cargo bench (>5% regression = FAIL) + dhat heap check
Stage 6 — Coverage:  cargo-llvm-cov (<99% = FAIL, per-crate thresholds in quality/crate-coverage-thresholds.toml)
```
Any stage fails = build is RED. No exceptions.

**Branch Protection:** main requires all checks + PR review. develop requires compile+lint+test.
**CI Caching:** Cache cargo registry/git/target, key on Cargo.lock hash.
**Auto-Cancel:** Stale workflows via concurrency group.

## Gate 5a: Benchmark Baselines
| Operation | Budget | Crate |
|-----------|--------|-------|
| Tick binary parse | <10ns | core |
| Tick pipeline routing | <100ns | core |
| papaya lookup | <50ns | core |
| Full tick processing | <10μs | core |
| Valkey cache read | <100μs | storage |
| QuestDB ILP write | <1ms | storage |
| OMS state transition | <100ns | trading |
| Market hour validation | <50ns | common |
| Config TOML load | <10ms | common |
| REST API round-trip | 5-50ms | core |

>5% regression = automatic build failure. Benchmarks run with `--release` + `criterion::black_box`.

## Gate 5: Pre-Deployment

All CI green + Docker image builds (<50MB binary) + health checks pass + smoke test (1 tick end-to-end) + Parthiban approves.

## Gate 5b: Release Checklist

Full checklist: `docs/templates/release_checklist.md` (read ONLY at release time).

## Gate 6: Runtime Monitoring — Post-Deployment

After deployment, verify:

1. All Grafana dashboards showing data — no gaps, no stale panels
2. Prometheus scrape targets all UP — zero down targets
3. Jaeger traces flowing — spans visible for tick processing
4. Loki logs streaming — application logs arriving via Alloy
5. Telegram test alert fires — confirm alerting pipeline works
6. Tick latency within budget — <10ns parse, <10μs processing confirmed in metrics
