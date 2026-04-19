# 100% Audit Tracker — Mechanically-Provable Coverage Matrix

**Status:** LIVING DOC (refreshed every `make 100pct-audit` run)
**Owner:** Parthiban (architect), Claude Code (builder)
**Scope:** Every dimension the operator can demand "prove 100%" on, with a test / metric / gate as the receipt.
**Milestone:** M5 of `.claude/plans/autonomous-operations-100pct.md`.

## The four categories of "100%"

Be precise about what 100% means. There are exactly four categories; anyone
promising "100%" without specifying which is selling you theatre.

| Category | What it means | Example |
|---|---|---|
| **P — Mechanically provable** | Type system, CI gate, or ratcheted test proves it at build time | Line coverage threshold = 100%, DEDUP key invariants |
| **R — Runtime-verifiable** | A live metric / alert / audit log proves it in production | Tick gap < 1s, WS reconnect < 2s, QuestDB `up{}` = 1 |
| **L — Layered (asymptotic)** | Absolute 100% is impossible (external system can fail); layered defenses drive loss towards zero | Zero tick loss = WS + spill + replay + DEDUP + alert + operator |
| **I — Impossible (absolute)** | Rice/Gödel/network-physics forbid absolute zero | Zero bugs ever, zero network packet loss |

No dimension in this tracker is marked "100%" without citing which category it
falls under and what artifact proves the claim.

## Dimension matrix (16 dimensions)

### Coverage + Testing

| Dimension | Category | Target | Proof artifact |
|---|---|---|---|
| Line coverage (all crates) | P | 100% | `quality/crate-coverage-thresholds.toml` + `scripts/coverage-gate.sh` in CI (blocks merge if <100%) |
| Branch coverage | P | 100% | Same file, same gate |
| Mutation coverage (critical crates) | P | 0 survivors | `.github/workflows/mutation.yml` fails PR on any `SURVIVED:` line |
| Proptest randomised input coverage | P | 10,000+ cases per prop | `crates/core/tests/proptest_parser.rs` + `crates/core/tests/proptest_greeks_core.rs` + CI runs |
| Loom concurrency coverage | P | All atomic types | `crates/trading/tests/loom_circuit_breaker.rs` |
| DHAT zero-allocation on hot path | P | 0 allocs | `crates/core/tests/dhat_allocation.rs` + hot-path-reviewer agent |
| Fuzz coverage | L | 5min/target/week (scales to 24h with paid CI) | `fuzz/` + `.github/workflows/fuzz.yml` |
| Sanitizers (ASan + TSan) | P | Weekly clean run | `.github/workflows/safety.yml` |
| 22-test standard | P | Every crate covers all 22 categories | `.claude/rules/project/testing.md` + `scripts/validate-automation.sh` |
| Schema validation | P | Every public struct | `crates/common/tests/schema_validation.rs` |

### Source quality

| Dimension | Category | Target | Proof artifact |
|---|---|---|---|
| Code checks (fmt + clippy -D warnings -W perf) | P | 0 violations | `.claude/hooks/pre-commit-gate.sh` + CI `Build & Verify` |
| Banned patterns (secrets, `.unwrap()`, `.expect()`, `.clone()` on hot path, `println!`, `DashMap`, `dyn` on hot, `HashSet<u32>` for instruments, etc.) | P | 0 matches | `.claude/hooks/banned-pattern-scanner.sh` (6 categories) |
| No hardcoded values | P | All values from `config/base.toml` or constants.rs | `pre-commit-gate.sh` gate 4 |
| Version pinning (no `^`/`~`/`*`/`>=`) | P | All deps pinned exact | `pre-commit-gate.sh` gate 6 + `deny.toml` |
| Must-use compliance | P | `#![deny(unused_must_use)]` in every lib.rs | `crates/common/tests/panic_safety.rs` |
| No `unwrap`/`expect` in prod code | P | `#![deny(clippy::unwrap_used, clippy::expect_used)]` in every lib.rs | Same |

### Performance

| Dimension | Category | Target | Proof artifact |
|---|---|---|---|
| O(1) on hot path | P | Enforced at banned-pattern + DHAT + bench level | `quality/benchmark-budgets.toml` + `scripts/bench-gate.sh` |
| Tick parse latency | P | ≤10ns | `crates/core/benches/tick_parser.rs` + 5% regression gate |
| Registry lookup | P | ≤50ns | `crates/common/benches/registry.rs` |
| Tick pipeline routing | P | ≤100ns | `crates/core/benches/pipeline.rs` |
| OMS state transition | P | ≤100ns | `crates/trading/benches/state_machine.rs` |
| Wire-to-persist full tick | P | ≤10μs | `crates/core/benches/full_tick.rs` |

### Observability (Logging + Monitoring + Alerting)

| Dimension | Category | Target | Proof artifact |
|---|---|---|---|
| ErrorCode tag on every `error!` | P | 100% | `crates/common/tests/error_code_tag_guard.rs` |
| Every ErrorCode has rule + runbook | P | 54/54 variants | `crates/common/tests/error_code_rule_file_crossref.rs` + `triage_rules_full_coverage_guard.rs` |
| Prometheus metric catalog | P | No drift | `crates/common/tests/metrics_catalog.rs` |
| Recording rules | P | No drift | `crates/common/tests/recording_rules_guard.rs` |
| Alertmanager rules wiring | P | Every alert has a rule file | `crates/common/tests/grafana_alerts_wiring.rs` |
| Zero-tick-loss alert | R | Fires on any gap ≥1s | `deploy/docker/prometheus/rules/tickvault-alerts.yml` + `crates/storage/tests/zero_tick_loss_alert_guard.rs` |
| Resilience SLA alerts (WS/QuestDB/Valkey) | R | Fires on degradation | `crates/storage/tests/resilience_sla_alert_guard.rs` |
| Telegram delivery | R | Every ERROR level | `crates/core/src/notification/` |
| Structured logging JSONL | P | Every ERROR → `errors.jsonl.<HOUR>` | `crates/app/src/observability.rs` + hourly rotation |
| Summary writer | R | 60s refresh | `crates/core/src/notification/summary_writer.rs` |
| Triage coverage | P | 54/54 codes | `triage_rules_full_coverage_guard.rs` |

### Security

| Dimension | Category | Target | Proof artifact |
|---|---|---|---|
| cargo audit (no advisories) | P | 0 high/critical | `.github/workflows/ci.yml` Security & Audit job |
| cargo deny (license + version) | P | 0 violations | `deny.toml` + same CI job |
| Secret scanning | P | No secrets in commits | `.claude/hooks/secret-scanner.sh` + CI Secret Scan |
| API bearer auth middleware | P | Mutating endpoints protected | `crates/api/src/middleware.rs` + `crates/api/tests/api_auth_middleware_guard.rs` |
| Secret<T> zeroize | P | All credentials | `crates/common/src/config.rs` secrecy + zeroize |
| TLS with aws-lc-rs | P | All HTTPS clients | `reqwest` + workspace deps |
| Static IP enforcement | R | Pre-market check | `crates/core/src/network/ip_verifier.rs` |
| systemd service hardening (AWS) | P | NoNewPrivileges, ProtectSystem | `scripts/tv-tunnel/tickvault-tunnel.service` |
| Security review | R | Every PR | `/security-review` skill + `security-reviewer` agent |

### Data integrity + O(1) + Uniqueness + Dedup

| Dimension | Category | Target | Proof artifact |
|---|---|---|---|
| security_id uniqueness (`(security_id, segment)`) | P | Composite key everywhere | `.claude/rules/project/security-id-uniqueness.md` + planner/registry/dedup tests |
| Tick DEDUP includes segment | P | `DEDUP_KEY_TICKS` contains segment | `crates/storage/tests/dedup_segment_meta_guard.rs` |
| Derivative DEDUP includes underlying | P | `DEDUP_KEY_DERIVATIVE_CONTRACTS` contains underlying_symbol | Same |
| Price precision f32→f64 | P | `f32_to_f64_clean()` | `crates/storage/src/tick_persistence.rs` |
| IEEE 754 widening artifacts | P | 0 | `crates/storage/tests/f32_precision_regression.rs` |
| Live-feed purity (no backfill→ticks) | P | 3-layer enforcement | `crates/storage/tests/live_feed_purity_guard.rs` |
| Instrument registry composite index | P | `(SecurityId, ExchangeSegment)` | `crates/common/src/instrument_registry.rs` tests |

### Resilience (the asymptotic ones — honest about what they are)

| Dimension | Category | Target | Proof artifact + defense layers |
|---|---|---|---|
| Zero tick loss | L | Asymptotic | (1) WS reconnect <2s, (2) spill to disk on QuestDB down, (3) replay on recovery, (4) DEDUP eliminates duplicates, (5) `tv_ticks_dropped_total` alert, (6) daily tick-count cross-check vs Dhan historical. No absolute guarantee — network packets CAN be lost upstream of tickvault. |
| Zero WS disconnect | L | Asymptotic | (1) 5 connections per pool, (2) connection state machine, (3) ping/pong health, (4) auto-reconnect with backoff, (5) DATA-805 handler (pause 60s + audit), (6) kill-switch on cascade. No absolute guarantee — Dhan CAN send code 50 for its own reasons (maintenance, rate-limit). |
| QuestDB never fails | L | Asymptotic | (1) Docker healthcheck, (2) spill to disk on ILP failure, (3) auto-replay on recovery, (4) self-heal ALTER TABLE, (5) `up{job="questdb"}` alert, (6) partition manager detaches old data before disk full. No absolute guarantee — disk CAN fill, hardware CAN fail. |
| O(1) on ALL paths | I→L | Cold paths have O(n) | Hot path: P (enforced). Cold path (CSV parse, ILP flush): L — batched + bounded. No absolute O(1) on cold paths because the data size varies. |
| Zero bugs ever | I | Impossible | Halting problem + Rice's theorem say this cannot be proven for a Turing-complete language. Closest proxies: coverage + mutation + fuzz + property + chaos + sanitizers + review. |

## Automation layer

### Real-time audit

```bash
make 100pct-audit         # runs every verifiable check, prints dashboard
bash scripts/100pct-audit.sh  # same, direct invocation
bash scripts/100pct-audit.sh --json  # machine-readable output for dashboards
```

Each dimension prints:
- Category (P/R/L/I)
- Current status (PASS / GAP / SKIPPED / IMPOSSIBLE-ABSOLUTE)
- Proof artifact (test name, metric name, file path)
- Last checked timestamp

### Gate integration

The audit runs in three contexts:
1. **Local** — developer runs before `git commit` (advisory)
2. **CI** — `.github/workflows/ci.yml` runs it on every PR (blocking)
3. **Runtime** — `cron` job on AWS EC2 runs every hour, alerts on regression

### Tests that enforce this tracker is accurate

`crates/common/tests/audit_100pct_tracker_guard.rs`:
- Every dimension in this doc has a matching check in `scripts/100pct-audit.sh`
- Every check in the script has a dimension entry here
- Every cited proof artifact resolves (test exists, metric is emitted, file is present)
- No dimension is marked category P/R without a real artifact path
- Every L/I category has a layered-defense block in the justification

## What this does NOT do

- Does NOT promise absolute zero tick loss / zero disconnect / zero downtime.
  Anyone who does is wrong.
- Does NOT cover AWS-specific items that require a live instance (those are
  M6).
- Does NOT cover Dhan-side outages (their infra, their SLA).
- Does NOT cover operator error (someone `rm -rf`ing the data dir).

What it DOES do: for every dimension you care about, give you the exact
test/metric/file that proves or defends it — refreshed in real time.

## Categorization summary

| Category | Dimensions | Example |
|---|---|---|
| P (Mechanically provable) | ~40 | Line coverage, banned patterns, DEDUP keys |
| R (Runtime-verifiable) | ~10 | Tick gap alert, WS reconnect rate, QuestDB up |
| L (Layered asymptotic) | ~5 | Zero tick loss, zero disconnect, QuestDB never fails |
| I (Impossible absolute) | ~3 | Zero bugs, full O(1), perfect security |

Total: ~58 tracked dimensions. The script runs each and reports per category.
