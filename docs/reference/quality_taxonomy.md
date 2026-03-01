# Quality Taxonomy — Master Reference

> **Single source of truth** for all quality dimensions in dhan-live-trader.
> Open this file at the start of every session. Verify every applicable dimension before committing.
>
> **Last updated:** 2026-03-01 | **Total dimensions:** 98 | **Test baseline:** 477

## Status Legend

| Status | Meaning | Action Required |
|--------|---------|-----------------|
| `ENFORCED` | Mechanically blocked — hooks, CI, or compiler prevent violations | None — already gated |
| `CONFIGURED` | Tool/dependency available but not fully gated in CI | Wire into enforcement pipeline |
| `DOCUMENTED` | Written in docs/rules but no automated enforcement | Add tooling or accept as manual |
| `GAP` | Not present in any form | Decide: implement, defer, or mark N/A |
| `N/A` | Not applicable to this system | Skip |

## Summary

| Category | Dimensions | ENFORCED | CONFIGURED | DOCUMENTED | GAP | N/A |
|----------|-----------|----------|------------|------------|-----|-----|
| 1. Test Coverage | 19 | 12 | 2 | 2 | 3 | 0 |
| 2. Code Coverage | 8 | 0 | 3 | 2 | 2 | 1 |
| 3. Performance | 12 | 2 | 3 | 2 | 5 | 0 |
| 4. Quality Gates | 14 | 11 | 3 | 0 | 0 | 0 |
| 5. Security | 12 | 10 | 1 | 0 | 1 | 0 |
| 6. Monitoring | 10 | 3 | 4 | 1 | 2 | 0 |
| 7. Debugging | 8 | 6 | 0 | 0 | 2 | 0 |
| 8. Audit & Compliance | 8 | 4 | 2 | 2 | 0 | 0 |
| **Totals** | **98** | **52** | **18** | **12** | **15** | **1** |

---

## Category 1: Test Coverage Types

| # | Dimension | Description | Trading Relevance | Status | Enforcement | File/Tool |
|---|-----------|-------------|-------------------|--------|-------------|-----------|
| 1.1 | Unit tests | Per-function isolation tests | Core correctness of every operation | `ENFORCED` | Pre-commit gate 3, CI stage 3 | `cargo test --workspace` |
| 1.2 | Integration tests | Cross-crate data flow tests | Boot sequence, WS→parser→QuestDB pipeline | `ENFORCED` | `cargo test --workspace` (tests/ dirs) | `crates/*/tests/` |
| 1.3 | Property-based (proptest) | Randomized invariant verification | Malformed ticks, edge floats, instrument counts 0..25000 | `CONFIGURED` | proptest 1.6.0 in workspace dev-deps, tests exist | `.claude/rules/testing.md` category 5 |
| 1.4 | Fuzz testing | Long-running crash discovery on binary input | WebSocket binary protocol parser — #1 attack surface | `GAP` | Not configured | See Gap #1 |
| 1.5 | Mutation testing | Verify test suite catches code mutations | Ensures tests actually validate behavior, not just execute | `GAP` | Not configured | See Gap #2 |
| 1.6 | Snapshot/golden tests | Output comparison against stored baselines | WS subscription format, ILP format, JSON API response stability | `GAP` | Not configured | See Gap #3 |
| 1.7 | Concurrency tests (loom) | Exhaustive interleaving verification | ArcSwap token renewal, crossbeam channels, papaya map | `CONFIGURED` | loom 0.7.2 in workspace dev-deps, no tests yet | `.claude/rules/testing.md` category 9 |
| 1.8 | Benchmark tests (criterion) | Statistical latency measurement | <10ns parse, <100ns OMS, <10μs pipeline | `DOCUMENTED` | criterion 0.8.2 in workspace dev-deps, no benches yet | `quality_gates.md` Gate 5a |
| 1.9 | Smoke tests | Basic construction & default state | Boot sequence completes, /health returns OK | `ENFORCED` | Tests exist, required per testing_standards.md cat 1 | `testing_standards.md` |
| 1.10 | Regression tests | Bug-specific non-recurrence | Every bug fix includes a test that catches the original bug | `ENFORCED` | Tests exist, naming `test_*_regression_*` | `testing_standards.md` cat 15 |
| 1.11 | Stress/boundary tests | Capacity limits, overflow, zero/MAX | 25K instruments, 65536 channel buffer, batch 100 | `ENFORCED` | Tests exist in core crate | `testing_standards.md` cat 4 |
| 1.12 | Error path tests | Every `Err`/`None` path tested | Auth failure, WS disconnect, QuestDB down | `ENFORCED` | Tests exist | `testing_standards.md` cat 3 |
| 1.13 | Security tests | Secret handling, token non-exposure | Token not in Debug/Display/logs, grep expose_secret clean | `ENFORCED` | Tests exist, security checklist | `testing_standards.md` cat 7 |
| 1.14 | Dedup tests | Duplicate rejection verification | 4-layer dedup chain verified | `ENFORCED` | `test_no_duplicate_security_ids`, `test_plan_no_duplicates_*` | `testing_standards.md` cat 8 |
| 1.15 | Determinism tests | Same input → identical output | Parser byte-exact, instrument build reproducible | `ENFORCED` | Tests exist | `testing_standards.md` cat 13 |
| 1.16 | Round-trip tests | Serialize → deserialize identity | Config, ILP encoding, subscription messages | `ENFORCED` | Tests exist | `testing_standards.md` cat 12 |
| 1.17 | Display/Debug tests | Format trait correctness | Secret [REDACTED], error message format | `ENFORCED` | Tests exist | `testing_standards.md` cat 16 |
| 1.18 | Contract tests | API schema drift detection | Dhan REST/WS response format changes without notice | `DOCUMENTED` | Implicitly via serde tests, no formal framework | See Gap #4 |
| 1.19 | Chaos/fault-injection | Infrastructure failure simulation | QuestDB crash, network partition, OOM mid-session | `GAP` | Not configured | See Gap #5 |

---

## Category 2: Code Coverage Types

| # | Dimension | Description | Trading Relevance | Status | Enforcement | File/Tool |
|---|-----------|-------------|-------------------|--------|-------------|-----------|
| 2.1 | Line coverage | % of source lines executed | Untested error paths in order submission could panic | `CONFIGURED` | cargo-tarpaulin in CI stage 6, Xml output | `.github/workflows/ci.yml` |
| 2.2 | Branch coverage | % of if/else/match arms taken | 8-variant OrderState match with only 5 tested = 3 blind spots | `CONFIGURED` | tarpaulin `--branch` available, not enabled | Upgrade to llvm-cov for accuracy |
| 2.3 | Function coverage | % of functions called at least once | Dead code detection — uncalled functions are attack surface | `CONFIGURED` | Implied by tarpaulin default output | CI stage 6 |
| 2.4 | Condition coverage | Each boolean sub-expression T/F | `if size > limit && drawdown > max` — each condition independent | `GAP` | Not standard in Rust ecosystem | See Gap #6 (partial via llvm-cov) |
| 2.5 | Path coverage | All execution paths through function | 5 sequential ifs = 32 paths — approximated by proptest | `DOCUMENTED` | Approximated by property-based testing | Infeasible for full coverage |
| 2.6 | MC/DC | Modified Condition/Decision Coverage | Aviation/safety-critical standard — partial relevance for risk checks | `N/A` | Overkill for this system | DO-178C level, not required |
| 2.7 | Crate-level thresholds | Per-crate minimum coverage % | core/trading 95%, common/storage/api 90%, app 80% | `DOCUMENTED` | Thresholds defined in testing.md, NOT enforced in CI | See Gap #7 |
| 2.8 | Coverage ratcheting | Coverage % can only go up | Test count ratchets but coverage % doesn't | `GAP` | Test count guard exists, coverage % not gated | See Gap #7 |

---

## Category 3: Code Performance Types

| # | Dimension | Description | Trading Relevance | Status | Enforcement | File/Tool |
|---|-----------|-------------|-------------------|--------|-------------|-----------|
| 3.1 | Microbenchmarks (criterion) | Per-function latency: mean, median, p95, p99 | Tick parse <10ns, OMS <100ns, signal <10μs | `DOCUMENTED` | criterion 0.8.2 in dev-deps, no benches written yet | `quality_gates.md` Gate 5a |
| 3.2 | Throughput benchmarks | Operations/sec (ticks/sec, orders/sec) | 25K instruments at market open must not fall behind | `GAP` | Not configured | criterion `Throughput::Elements(n)` |
| 3.3 | Allocation tracking (dhat) | Heap alloc count/size/location per operation | Principle #1: zero allocation on hot path — runtime proof | `CONFIGURED` | dhat 0.3.3 in dev-deps, not integrated | `quality_gates.md` Gate 5 |
| 3.4 | Flamegraph profiling | CPU hotspot visualization via call stack | Reveals unexpected hotspots (e.g., 60% in TLS, not parsing) | `GAP` | cargo-flamegraph not configured | See Gap #8 |
| 3.5 | Memory profiling | RSS/heap growth over time (soak) | 7-hour trading day — leak = OOM kill mid-session | `GAP` | No tool configured | See Gap #9 |
| 3.6 | Cache performance | L1/L2/L3 hit rates, false sharing | papaya lookup cache miss = +40-80ns per access | `GAP` | perf stat not configured | Linux `perf stat` on AWS |
| 3.7 | Latency percentiles (HDR) | p50/p95/p99/p99.9 at runtime | Mean is misleading — p99 of 1ms when p50 is 10ns = problem | `CONFIGURED` | hdrhistogram 7.5.4 + quanta 0.12.5 in workspace deps | Prometheus histograms |
| 3.8 | Benchmark regression gate | >5% regression blocks CI | Documented but not enforced — regression could slip through | `DOCUMENTED` | criterion runs in CI stage 5, no comparison gate | See Gap #10 |
| 3.9 | Zero-alloc verification | Compile-time proof of no heap on hot path | Principle #1 — mechanically enforced | `ENFORCED` | banned-pattern-scanner.sh (category 2 hot-path) | `.claude/hooks/banned-pattern-scanner.sh` |
| 3.10 | O(1) complexity verification | No linear scans on hot path | .iter().find(), .sort(), .contains() on Vec blocked | `ENFORCED` | dedup-latency-scanner.sh | `.claude/hooks/dedup-latency-scanner.sh` |
| 3.11 | Struct size/alignment | Cache-line optimization for hot types | ParsedTick = 80 bytes, Copy trait, no heap | `CONFIGURED` | Manually verified, no automated gate | `#[repr(C)]` where needed |
| 3.12 | Async runtime metrics | Tokio task count, poll duration, scheduler depth | Slow poll blocks entire runtime thread — all async stalls | `GAP` | tokio-console not configured | `console-subscriber` crate |

---

## Category 4: Quality Gates

| # | Dimension | Description | Trading Relevance | Status | Enforcement | File/Tool |
|---|-----------|-------------|-------------------|--------|-------------|-----------|
| 4.1 | Compilation | `cargo build --release` zero errors | overflow-checks = true prevents silent integer wrap in financial math | `ENFORCED` | Pre-push gate, CI stage 1 | `Cargo.toml` profile.release |
| 4.2 | Formatting (rustfmt) | Zero diff from `cargo fmt --all` | Eliminates meaningless diffs, focuses review on logic | `ENFORCED` | Pre-commit gate 1, CI stage 2 | `.rustfmt.toml` |
| 4.3 | Linting (clippy) | Zero warnings with `-D warnings` | Catches correctness bugs, suspicious patterns | `ENFORCED` | Pre-commit gate 2, CI stage 2 | `clippy.toml` |
| 4.4 | Clippy perf lints | `-W clippy::perf` | Performance-specific lint warnings elevated | `ENFORCED` | CI stage 2 | `.github/workflows/ci.yml` |
| 4.5 | Compile-time #![deny()] | 14 deny-level lints in every lib.rs | unwrap, panic, todo, print → compiler error | `CONFIGURED` | Documented in quality_gates.md Gate 1, partially in lib.rs files | See Gap #12 |
| 4.6 | Doc coverage | `cargo doc --workspace --no-deps` clean | Missing docs on risk check = next dev can't verify correctness | `ENFORCED` | /quality skill step 4 | `/quality` skill |
| 4.7 | Dead code detection | Unused functions/imports/types | Dead code = attack surface with no benefit | `ENFORCED` | clippy + compiler warnings | Default Rust warnings |
| 4.8 | Unused deps detection | Declared but never-used Cargo deps | Unused dep with CVE still shows in audit | `CONFIGURED` | cargo-machete works on stable, not in CI | See Gap #13 |
| 4.9 | Cognitive complexity | Max 30 per function | Complex functions = guaranteed bug source | `ENFORCED` | clippy.toml threshold | `clippy.toml` |
| 4.10 | Too-many-arguments | Max 8 parameters | Forces struct grouping, improves API design | `ENFORCED` | clippy.toml threshold | `clippy.toml` |
| 4.11 | Type complexity | Max 300 | Prevents unreadable type signatures | `ENFORCED` | clippy.toml threshold | `clippy.toml` |
| 4.12 | Spelling (typos) | Code/comment/doc typo detection | `securi_id` vs `security_id` = silent field mismatch | `CONFIGURED` | typos.toml configured, manual run only | `typos.toml` |
| 4.13 | Conventional commits | `<type>(<scope>): <description>` format | Automated changelog, clear audit trail | `ENFORCED` | commit-msg hook, pre-PR gate 5 | `scripts/git-hooks/commit-msg` |
| 4.14 | Test count ratcheting | Test count can only increase, never decrease | Prevents accidental test deletion | `ENFORCED` | Pre-commit gate 8, pre-push gate 5 | `.claude/hooks/test-count-guard.sh` |

---

## Category 5: Security Gates

| # | Dimension | Description | Trading Relevance | Status | Enforcement | File/Tool |
|---|-----------|-------------|-------------------|--------|-------------|-----------|
| 5.1 | Dependency CVE audit | `cargo audit` scans Cargo.lock against RustSec DB | Compromised HTTP/TLS crate = order interception | `ENFORCED` | Pre-push gate 6, CI stage 4 | `cargo audit` |
| 5.2 | Weekly CVE scan | Catch post-merge CVEs | New vulnerability discovered after deploy | `ENFORCED` | security-audit.yml (Monday 06:00 UTC) | `.github/workflows/security-audit.yml` |
| 5.3 | License compliance | Allow-list: MIT, Apache-2.0, BSD, ISC, etc. | GPL dep = legal risk, forced open-sourcing | `ENFORCED` | `cargo deny check licenses` | `deny.toml` [licenses] |
| 5.4 | Banned crates | Block bincode (tombstoned), openssl (C FFI) | Known-bad deps with alternatives available | `ENFORCED` | `cargo deny check bans` | `deny.toml` [bans] deny list |
| 5.5 | Source verification | crates.io only — no unknown-git, no unknown-registry | Rogue registry = supply chain attack vector | `ENFORCED` | `cargo deny check sources` | `deny.toml` [sources] |
| 5.6 | Secret scanning (commit) | Block hardcoded API keys, tokens, passwords | Leaked Dhan token = full trading account access | `ENFORCED` | Pre-commit gate 6 (ALL staged files) | `.claude/hooks/secret-scanner.sh` |
| 5.7 | Secret scanning (CI) | Server-side secret check on push/PR | Redundant layer in case local hook bypassed | `ENFORCED` | secret-scan.yml workflow | `.github/workflows/secret-scan.yml` |
| 5.8 | .env file blocking | Prevent .env creation/commit | .env = plaintext secrets on disk | `ENFORCED` | block-env-files.sh hook | `.claude/hooks/block-env-files.sh` |
| 5.9 | Unsafe code audit | Block `unsafe` without `// SAFETY:` justification | Memory corruption = incorrect orders/data loss | `ENFORCED` | banned-pattern-scanner.sh | `.claude/hooks/banned-pattern-scanner.sh` |
| 5.10 | Supply chain (yanked) | Detect yanked crates in Cargo.lock | Yanked = maintainer retracted, likely defective | `ENFORCED` | `cargo audit --deny yanked` in security-audit.yml | `.github/workflows/security-audit.yml` |
| 5.11 | SAST (static analysis) | Deep code vulnerability scanning beyond clippy | SQL injection, command injection, path traversal | `GAP` | Clippy covers common patterns, no dedicated SAST | See Gap #14 |
| 5.12 | Duplicate dependencies | Multiple versions of same crate | Type incompatibility, binary bloat | `CONFIGURED` | `cargo deny check bans` with skip list for known transitive dupes | `deny.toml` [bans] skip list |

---

## Category 6: Monitoring & Observability

| # | Dimension | Description | Trading Relevance | Status | Enforcement | File/Tool |
|---|-----------|-------------|-------------------|--------|-------------|-----------|
| 6.1 | Application metrics (Prometheus) | Counters, gauges, histograms | Tick latency spike above 10μs triggers investigation | `CONFIGURED` | metrics 0.24.3 + prometheus exporter in deps, Docker running | `docker-compose.yml` prometheus service |
| 6.2 | Structured logging | JSON tracing events with 5W fields | Post-incident analysis: which instrument, what price, when, why | `ENFORCED` | tracing 0.1.44 + tracing-subscriber 0.3.22, JSON formatter | `.claude/rules/rust-code.md` |
| 6.3 | Log aggregation (Loki) | Centralized log storage + LogQL queries | Filter by security_id, time range, severity during investigation | `CONFIGURED` | Loki 3.6.6 + Alloy v1.8.0 in Docker | `deploy/docker/loki/` |
| 6.4 | Distributed tracing (Jaeger) | OpenTelemetry spans across pipeline stages | Reveals where latency spent: parse vs channel vs QuestDB write | `CONFIGURED` | Jaeger v2 in Docker, opentelemetry 0.31.0 in deps | `docker-compose.yml` jaeger service |
| 6.5 | Health checks | /health endpoint + Docker healthchecks | Traefik routing, automated recovery, monitoring | `ENFORCED` | Axum handler, all 8 Docker services have healthcheck | `crates/api/src/handlers/health.rs` |
| 6.6 | Dashboards (Grafana) | Visual monitoring panels | Single pane of glass for trading system | `CONFIGURED` | Grafana 12.3.3 with provisioned datasources, no dashboards yet | `deploy/docker/grafana/` |
| 6.7 | Alerting (Telegram) | ERROR-level → immediate notification | Sole operator needs instant notification during market hours | `ENFORCED` | teloxide 0.13.0 in deps, notify script exists | `scripts/notify-telegram.sh` |
| 6.8 | SLO tracking | p99 < 10μs, uptime > 99.9%, success > 99.99% | Convert performance budgets to monitorable targets | `GAP` | Performance budgets defined, no formal SLO monitoring | See Gap #15 |
| 6.9 | Error rate monitoring | Error % tracking over time | Rising error rate = degradation before failure | `GAP` | Not configured | Prometheus counter + Grafana alert |
| 6.10 | Resource monitoring | CPU/mem/disk/network per container | Disk full at 95% or OOM must be caught before impact | `DOCUMENTED` | Prometheus node exporter planned, Docker stats available | `quality_gates.md` Gate 6 |

---

## Category 7: Debugging Capabilities

| # | Dimension | Description | Trading Relevance | Status | Enforcement | File/Tool |
|---|-----------|-------------|-------------------|--------|-------------|-----------|
| 7.1 | Structured error types | thiserror per-crate error enums | Typed errors enable programmatic error handling | `ENFORCED` | thiserror 2.0.18 in deps | `crates/common/src/error.rs` |
| 7.2 | Error context chains | .context() for full backtrace | "QuestDB write failed → TCP refused → host unreachable" | `ENFORCED` | anyhow 1.0.99 in deps, `.context()` usage | `.claude/rules/rust-code.md` |
| 7.3 | Panic handling | Release: `panic = "abort"`, clippy denies panic!() | Fast termination + Docker restart, no catch_unwind complexity | `ENFORCED` | profile.release panic = "abort", clippy lint | `Cargo.toml` profile |
| 7.4 | Secret redaction | Debug/Display prints [REDACTED] for secrets | Prevents credential leaks in error messages and logs | `ENFORCED` | secrecy 0.10.3 + zeroize 1.8.2 in deps | `testing_standards.md` security checklist |
| 7.5 | Span-based tracing | #[instrument] on key functions | Function-level timing and context in traces | `ENFORCED` | tracing macros, Jaeger UI | `.claude/rules/rust-code.md` |
| 7.6 | Log levels | TRACE/DEBUG/INFO/WARN/ERROR hierarchy | Filter noise, focus on severity during investigation | `ENFORCED` | tracing macros only (println/dbg BANNED) | `.claude/hooks/banned-pattern-scanner.sh` |
| 7.7 | Tick replay | Replay historical ticks from QuestDB | Reproduce bug at 14:23 IST with exact same tick sequence | `GAP` | QuestDB stores ticks, memmap2/bitcode in deps, no replay tool | See Gap #16 |
| 7.8 | Core dumps | Post-mortem crash analysis | Exact state at crash time — what order in flight, what data | `GAP` | Not configured (ulimit, coredump collection) | See Gap #17 |

---

## Category 8: Audit & Compliance

| # | Dimension | Description | Trading Relevance | Status | Enforcement | File/Tool |
|---|-----------|-------------|-------------------|--------|-------------|-----------|
| 8.1 | Git history integrity | Conventional commits, branch protection | Regulatory audit trail — what changed, when, why | `ENFORCED` | commit-msg hook, pre-PR gate, branch protection | `scripts/git-hooks/commit-msg` |
| 8.2 | Change tracking | All changes via PRs with CI passing | Complete traceability from requirement to code | `ENFORCED` | pre-merge-gate.sh (CI status check) | `.claude/hooks/pre-merge-gate.sh` |
| 8.3 | Data retention (SEBI) | 5-year minimum for all trading records | Regulatory requirement — non-compliance = penalties | `DOCUMENTED` | Retention periods defined, QuestDB + S3 lifecycle not configured | `data_integrity.md` |
| 8.4 | Order audit trail | Every OMS state transition logged and persisted | Complete order lifecycle: New→Pending→Filled→Reconciled | `DOCUMENTED` | statig 0.3.0 in deps, architecture defined, implementation pending | Phase 2 (OMS) |
| 8.5 | Position reconciliation | Broker vs local position match after every fill | Mismatch = money at risk, halt trading + alert | `CONFIGURED` | Architecture in data_integrity.md, implementation pending | Phase 2 (OMS) |
| 8.6 | Access control | SSM-only secrets, IAM-based access | Only authorized access to trading credentials | `ENFORCED` | aws-sdk-ssm auth flow, secret-scanner blocks hardcoded | `crates/core/src/auth/` |
| 8.7 | Branch protection | main requires CI green + PR review | Prevents untested code from reaching production | `CONFIGURED` | GitHub settings + pre-merge-gate, local enforcement | `.claude/hooks/pre-merge-gate.sh` |
| 8.8 | Auto-merge governance | Only approved actors (renovate, SJParthi, claude/) | Zero human intervention — CI is sole gatekeeper | `ENFORCED` | auto-merge.yml conditional checks | `.github/workflows/auto-merge.yml` |

---

## Gap Implementation Plans

### Gap #1: Fuzz Testing — `P1` (Phase 1)

**Why:** The WebSocket binary protocol parser (`crates/core/src/ticker/`) processes untrusted binary data from Dhan. Malformed packets from network corruption or exchange bugs must not crash the system.

**Tool:** `cargo-fuzz` (libFuzzer backend)

**Implementation:**
```
# Install
cargo install cargo-fuzz

# Initialize fuzz target
cargo fuzz init --fuzz-dir crates/core/fuzz

# Create fuzz target for tick parser
# File: crates/core/fuzz/fuzz_targets/parse_tick.rs
# Feed random bytes to dispatch_frame() and parse functions
# Verify: no panics, no UB, graceful error handling
```

**CI integration:** Weekly scheduled job (not per-commit — too slow). Nightly run with 10-minute timeout.

---

### Gap #2: Mutation Testing — `P2` (Phase 2)

**Why:** Line coverage of 95% means nothing if tests don't actually validate behavior. A mutation changing `<` to `<=` in a risk check that goes undetected is catastrophic.

**Tool:** `cargo-mutants`

**Implementation:**
```
# Install
cargo install cargo-mutants

# Run on critical crates only (full workspace is too slow)
cargo mutants -p dlt-core -p dlt-trading --timeout 120

# Target: >80% mutation kill rate on core + trading crates
```

**CI integration:** Weekly scheduled job. Report mutation survival rate.

---

### Gap #3: Snapshot/Golden Tests — `P1` (Phase 1)

**Why:** WebSocket subscription messages, QuestDB ILP write format, and JSON API responses must remain stable. An accidental format change breaks Dhan API integration silently.

**Tool:** `insta` crate (from mitsuhiko, rust-analyzer team uses it)

**Implementation:**
- Add `insta = "1.41.1"` to workspace dev-deps (verify version in Bible first)
- Write snapshot tests for: subscription JSON messages, ILP line format, /health JSON response, instrument CSV parsing output
- `cargo insta test` to create snapshots, `cargo insta review` to approve changes
- Snapshots stored in `crates/*/src/snapshots/`

**CI integration:** Runs as part of `cargo test --workspace`. Failed snapshot = test failure.

---

### Gap #4: Contract Tests — `P1` (Phase 1)

**Why:** Dhan can change their REST API or WebSocket response format without notice. Contract tests detect schema drift before it causes production parsing failures during market hours.

**Tool:** Typed serde assertions + golden response files

**Implementation:**
- Store sample Dhan API responses as JSON fixtures in `crates/core/tests/fixtures/`
- Test: deserialize fixture into Rust types, verify all fields parsed
- When Dhan updates API: update fixture, review diff
- For WebSocket: store sample binary frames as hex fixtures

**CI integration:** Runs as part of `cargo test --workspace`.

---

### Gap #5: Chaos/Fault-Injection — `P2` (Phase 2)

**Why:** Every failure scenario in `docs/reference/failure_scenarios.md` (30+ scenarios) must be tested: WS disconnect mid-tick, QuestDB write failure, OOM, etc.

**Tool:** `toxiproxy` (network failure injection) + Docker pause/kill + `fail` crate

**Implementation:**
- Docker-based test harness: start QuestDB + app, inject failures via toxiproxy
- Verify: graceful degradation, no data corruption, proper alerting
- Scenarios: network timeout, connection reset, slow response, corrupt response

**CI integration:** Weekly integration test job (requires Docker-in-Docker).

---

### Gap #6: Condition/Branch Coverage — `P1` (Phase 1)

**Why:** Coverage % ratcheting prevents regression. Thresholds defined (core 95%) but not enforced in CI.

**Tool:** `cargo-tarpaulin` with threshold flags OR `cargo-llvm-cov` for branch accuracy

**Implementation:**
```
# Option A: tarpaulin with threshold enforcement
cargo tarpaulin --workspace --fail-under 90

# Option B: upgrade to llvm-cov for branch coverage
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --branch --fail-under-lines 90

# Per-crate thresholds would need a custom script
```

**CI integration:** CI stage 6 — add `--fail-under` flag to tarpaulin command.

---

### Gap #7: Benchmark Regression Gate — `P1` (Phase 1)

**Why:** >5% regression is documented as a build failure condition (`quality_gates.md` Gate 5a) but criterion results are not compared against baselines in CI.

**Tool:** criterion + `critcmp` or GitHub Actions benchmark comparison

**Implementation:**
```
# Save baseline on main branch
cargo bench -- --save-baseline main

# Compare PR against baseline
cargo bench -- --save-baseline pr
critcmp main pr --threshold 5

# Exit 1 if any benchmark regressed >5%
```

**CI integration:** CI stage 5 — save baseline on main, compare on PR branches.

---

### Gap #8: Flamegraph Profiling — `P2` (Phase 2)

**Why:** Reveals unexpected CPU hotspots. May show 60% of tick processing in TLS, not parsing.

**Tool:** `cargo-flamegraph` (wraps `perf` on Linux)

**Implementation:**
```
# Install
cargo install flamegraph

# Generate (requires perf on Linux, dtrace on macOS)
cargo flamegraph --bin dlt-app -- --config config/base.toml

# AWS c7i.2xlarge supports perf — run during staging deploy
```

**CI integration:** Manual / on-demand. Generate during performance investigation.

---

### Gap #9: Memory Profiling — `P2` (Phase 2)

**Why:** 7-hour trading day. Memory leak → OOM kill → lost state. Docker restart recovers but with data loss risk.

**Tool:** `dhat` (already in dev-deps) + Prometheus `process_resident_memory_bytes`

**Implementation:**
```rust
// In benchmark or soak test:
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn test_no_leak_over_time() {
    let _profiler = dhat::Profiler::new_heap();
    // Run 10,000 tick processing cycles
    // Assert: total_bytes < threshold
    let stats = dhat::HeapStats::get();
    assert!(stats.curr_bytes == 0, "memory leak detected");
}
```

**CI integration:** Run as part of benchmark suite (CI stage 5).

---

### Gap #10: Benchmark Regression Gate (duplicate of #7)

*Merged with Gap #7 above.*

---

### Gap #11: Unused Dependencies Detection — `P1` (Phase 1)

**Why:** Every unused dependency is attack surface + compile time cost. An unused dep with a CVE still shows in `cargo audit`.

**Tool:** `cargo-machete` (works on stable, unlike cargo-udeps)

**Implementation:**
```
# Install
cargo install cargo-machete

# Run
cargo machete

# Fix: remove unused deps from Cargo.toml
```

**CI integration:** Add to CI stage 2 (lint) or pre-push gate.

---

### Gap #12: Compile-Time #![deny()] in lib.rs — `P0` (Phase 1, highest priority)

**Why:** 14 deny-level clippy lints documented in `quality_gates.md` Gate 1 but not consistently present in all crate lib.rs files. This is the single highest-priority gap.

**Implementation:** Add to every `crates/*/src/lib.rs`:
```rust
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::panic)]
#![deny(clippy::large_futures)]
#![deny(clippy::large_stack_arrays)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
#![warn(missing_docs)]
```

**Exceptions:** `app` crate main.rs may use `unwrap` for startup. Test modules excluded via `#[cfg(test)]`.

**CI integration:** Already enforced by `cargo clippy -D warnings` once attributes are in lib.rs.

---

### Gap #13: Unused Dependencies in CI — merged with Gap #11

---

### Gap #14: SAST (Static Application Security Testing) — `P2` (Phase 2)

**Why:** Clippy catches common patterns but doesn't cover SQL injection, command injection, path traversal, or insecure deserialization. Relevant when API server accepts user input.

**Tool:** `semgrep` with Rust rules OR GitHub CodeQL

**Implementation:**
```yaml
# .github/workflows/ci.yml — add SAST stage
sast:
  name: "SAST"
  needs: test
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: returntocorp/semgrep-action@v1
      with:
        config: "p/rust"
```

**CI integration:** Parallel after test stage, advisory (non-blocking initially).

---

### Gap #15: SLO Definitions — `P2` (Phase 2)

**Why:** Performance budgets in quality_gates.md are not tracked at runtime. Need measurable targets: tick p99 < 10μs, WS uptime > 99.9%, order success > 99.99%.

**Tool:** Prometheus recording rules + Grafana SLO dashboard

**Implementation:**
```yaml
# Prometheus recording rule
groups:
  - name: slo
    rules:
      - record: slo:tick_latency:p99
        expr: histogram_quantile(0.99, rate(tick_processing_seconds_bucket[5m]))
      - alert: TickLatencySLOBreach
        expr: slo:tick_latency:p99 > 0.00001  # 10μs
        for: 5m
        labels:
          severity: critical
```

**CI integration:** Runtime monitoring — not CI. Alerting via Telegram.

---

### Gap #16: Tick Replay — `P2` (Phase 2)

**Why:** When a bug occurs at 14:23 IST during live trading, need to replay exact tick sequence to reproduce.

**Tool:** `memmap2` 0.9.9 + `bitcode` 0.6.9 (both already in workspace deps)

**Implementation:**
- Record raw WebSocket frames to memory-mapped file during trading session
- Replay tool reads frames and feeds into tick pipeline
- Enables deterministic bug reproduction

**CI integration:** Not CI — development/debugging tool.

---

### Gap #17: Core Dump Configuration — `P2` (Phase 2)

**Why:** On production crash, core dump shows exact state — what order was being processed, what data in flight.

**Tool:** OS configuration + Docker

**Implementation:**
```dockerfile
# In Dockerfile
RUN ulimit -c unlimited

# In docker-compose.yml
services:
  dlt-app:
    ulimits:
      core: -1
    volumes:
      - ./coredumps:/tmp/coredumps
```

**CI integration:** Not CI — production infrastructure configuration.

---

## Per-Session Quality Checklist

> Copy-paste applicable sections into your session notes. Check each item before committing.

### Mandatory — Every Commit

- [ ] `cargo fmt --all` passes (pre-commit gate 1)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings -W clippy::perf` passes (gate 2)
- [ ] `cargo test --workspace` passes, all 477+ tests green (gate 3)
- [ ] Banned pattern scanner passes (gate 4)
- [ ] O(1) latency scanner passes (gate 5)
- [ ] Secret scanner passes on all staged files (gate 6)
- [ ] Cargo.toml versions pinned exactly, no ^/~/*/>=  (gate 7)
- [ ] Test count ≥ baseline, ratchet only goes up (gate 8)
- [ ] Commit message: `<type>(<scope>): <description>`

### If Touching Hot-Path Code

> Applies to: `crates/core/src/websocket/`, `crates/core/src/pipeline/`, `crates/core/src/ticker/`, `crates/trading/`

- [ ] Zero heap allocation — no Box, Vec::new(), String::new(), format!(), .collect()
- [ ] No O(n) operations — no .iter().find(), .sort(), .contains() on Vec
- [ ] No .clone() (except unavoidable data.to_vec())
- [ ] No DashMap — use papaya
- [ ] No dyn Trait — use enum_dispatch
- [ ] No blocking I/O — no std::fs, std::net, std::thread::sleep
- [ ] Benchmark baselines maintained (if criterion benches exist)

### If Adding/Modifying Features

- [ ] Unit tests for all new functions
- [ ] Error path tests for every Result::Err / Option::None
- [ ] Property-based tests for parsers and math functions
- [ ] Config validation tests if config fields changed
- [ ] Display/Debug tests for new public types
- [ ] Smoke test for new module (basic construction + default state)

### If Touching Security-Sensitive Code

> Applies to: `crates/core/src/auth/`, secret handling, token management

- [ ] Secret never appears in Debug/Display output
- [ ] Token behind `Secret<T>` wrapper (secrecy crate)
- [ ] No hardcoded credentials — all from AWS SSM
- [ ] `grep expose_secret` shows no new exposure
- [ ] Token not stored in struct fields beyond minimal scope

### If Modifying Dependencies

- [ ] Exact version from Tech Stack Bible V6 (no ^/~/*)
- [ ] `cargo deny check` passes (licenses + bans + sources + advisories)
- [ ] `cargo audit` clean (no known CVEs)
- [ ] License in deny.toml allow-list
- [ ] No banned crates (bincode, openssl)

### Pre-PR

- [ ] Branch naming: `feature/`, `fix/`, `hotfix/`, `chore/`, `docs/`, `refactor/`, `test/`, `perf/`, `security/`, `claude/`
- [ ] All commits follow conventional format
- [ ] Working tree clean (no uncommitted changes)
- [ ] CI green on all 6 stages
- [ ] Pre-PR gate passes (5 gates)

### Pre-Release

- [ ] Coverage thresholds met: core/trading ≥ 95%, common/storage/api ≥ 90%, app ≥ 80%
- [ ] Benchmark regressions < 5% (all 10 baselines in quality_gates.md)
- [ ] Docker image < 15MB (scratch base)
- [ ] All TODOs/FIXMEs resolved in production code
- [ ] Health check passes (POST /health)
- [ ] Smoke test passes (1 tick end-to-end)
- [ ] Release checklist complete (`docs/templates/release_checklist.md`)
- [ ] Parthiban approves

---

## Cross-Reference: Enforcement Files

| Enforcement Layer | Files | Gates |
|-------------------|-------|-------|
| **Layer 1: Pre-Commit** | `.claude/hooks/pre-commit-gate.sh` | 8 gates (fmt, clippy, test, banned, O(1), secrets, versions, test count) |
| **Layer 1: Scanners** | `banned-pattern-scanner.sh`, `dedup-latency-scanner.sh`, `secret-scanner.sh` | 25+ banned patterns, 15+ O(n) patterns, 16 secret patterns |
| **Layer 1: Blockers** | `block-env-files.sh`, `test-count-guard.sh` | .env prevention, ratcheting baseline |
| **Layer 2: Pre-Push** | `.claude/hooks/pre-push-gate.sh` | 7 gates (fmt, clippy, test, banned, test count, audit, deny) |
| **Layer 3: Pre-PR** | `.claude/hooks/pre-pr-gate.sh` | 5 gates (branch, naming, clean tree, quality state, commit format) |
| **Layer 4: Pre-Merge** | `.claude/hooks/pre-merge-gate.sh` | 3 gates (no --admin, PR number, CI status) |
| **Layer 5: CI** | `.github/workflows/ci.yml` | 6 stages (compile, lint, test, security, perf, coverage) |
| **Layer 5: Security** | `.github/workflows/security-audit.yml` | Weekly CVE + yanked crate scan |
| **Layer 5: Secrets** | `.github/workflows/secret-scan.yml` | Server-side secret detection |
| **Layer 5: Auto-merge** | `.github/workflows/auto-merge.yml` | Conditional squash merge for approved actors |
| **Config** | `deny.toml`, `.rustfmt.toml`, `clippy.toml`, `typos.toml` | Cargo-deny, formatting, lint thresholds, spelling |
| **Rules** | `.claude/rules/*.md` (7 files) | Declarative policy (enforcement, hot-path, rust-code, testing, cargo-docker, data-integrity, market-hours) |
| **Skill** | `.claude/skills/quality/SKILL.md` | `/quality` command (8-step pipeline) |
| **Script** | `scripts/quality-full.sh` | Local 6-stage CI equivalent |
