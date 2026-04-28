# Movers 22-TF v2 — Phased commits + verification gates + go/no-go

## Implementation phases (6 commits, was 5)

| Phase | Commit | Files | LoC |
|---|---|---|---|
| 1 | `feat(storage): 22 movers_{T} tables + DDL + DEDUP keys + partition manager + S3 lifecycle` | `materialized_views.rs`, `movers_22tf_persistence.rs`, `partition_manager.rs`, `s3-lifecycle-movers-tables.json`, 4 schema/lifecycle tests | ~500 |
| 2 | `feat(common): MoverRow Copy struct (ArrayString<16>) + 22-tf constants` | `mover_types.rs`, `constants.rs`, 3 tests | ~200 |
| 3 | `feat(core): papaya MoversTracker + arena snapshot + scheduler + supervisor + market-hours gate + 3-tier prev_close fallback + F&O expiry filter` | `top_movers.rs`, `movers_22tf_scheduler.rs`, `movers_22tf_supervisor.rs`, `movers_22tf_writer_state.rs`, 12 unit + supervisor tests | ~900 |
| 4 | `feat(observability): 10 metrics + 3 ErrorCodes + Triage YAML + 6-panel Grafana + 4 alert rules + drift/drop SLA + runbook` | `error_code.rs`, `events.rs`, `error-rules.yaml`, `movers-22tf.json`, `alerts.yml`, `wave-4-error-codes.md` (new MOVERS-22TF-01..03 entries), runbook | ~500 |
| 5 | `test(movers): 37 ratchets + 2 DHAT + 3 Criterion + 1 chaos test (1M rows/sec)` | `tests/`, `benches/`, `benchmark-budgets.toml`, `chaos_movers_22tf_throughput.rs` | ~700 |
| 6 | `chore(hooks): banned-pattern cats 8/9/10 + make doctor sections 8/9 + dedup_segment_meta_guard extension` | `banned-pattern-scanner.sh`, `Makefile`, `dedup_segment_meta_guard.rs`, `scripts/doctor.sh` | ~200 |
| **Total** | | | **~3,000 LoC** |

Within stream-resilience.md B12 PR cap (≤3,000 LoC). Ships as 6-commit follow-up PR on top of #408.

## Pre-merge verification gates

- [ ] All 37 new ratchets pass
- [ ] 2 DHAT tests: `total_blocks == 0` on hot path
- [ ] 3 Criterion benches within budget (50ns / 10ms / 200ns)
- [ ] **1 chaos test ≥1M rows/sec sustained, ≤0.1% drop rate**
- [ ] Coverage 100% on 5 new modules
- [ ] `bash .claude/hooks/banned-pattern-scanner.sh` (cats 8/9/10 active)
- [ ] `bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all`
- [ ] `bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"`
- [ ] `bash .claude/hooks/plan-verify.sh` (status: VERIFIED)
- [ ] `make scoped-check` green; `FULL_QA=1 make scoped-check` green
- [ ] `make doctor` green post-boot, sections 8 + 9 PASS
- [ ] Live boot — 22 tables populated within 1s of market open (manual smoke)
- [ ] `mcp__tickvault-logs__list_active_alerts` empty post-boot
- [ ] `mcp__tickvault-logs__list_novel_signatures` empty 5min post-boot
- [ ] 3 adversarial review agents (hot-path / security / general-purpose) pass on diff

## 9-box checklist per phase (per stream-resilience.md B8)

For each new code path:

| # | Item | Status |
|---|---|---|
| 1 | typed `NotificationEvent` variant | |
| 2 | `ErrorCode` enum variant + `code_str()` + `severity()` + `runbook_path()` | |
| 3 | `tracing::error!` with `code = ErrorCode::X.code_str()` field | |
| 4 | Prometheus counter (or gauge) with static labels | |
| 5 | Grafana panel wrapping counter in `increase()` / `rate()` | |
| 6 | Prometheus alert rule in `alerts.yml` | |
| 7 | Call site exists (pub-fn-wiring-guard passes) | |
| 8 | Triage YAML rule referencing the ErrorCode | |
| 9 | Ratchet test pinning the wiring | |

## Cross-references baked in

| Rule file | Wave 4 charter element honoured |
|---|---|
| `wave-4-shared-preamble.md` §1 | Use `mcp__tickvault-logs__*` tools at session start, not hand-rolled grep |
| `wave-4-shared-preamble.md` §2 | "100% inside the tested envelope" wording in PR body |
| `wave-4-shared-preamble.md` §3 | 3-agent parallel review BEFORE writing code AND on the diff |
| `wave-4-shared-preamble.md` §4 | 7-layer observability per code path (see ratchets v2-ratchets.md) |
| `wave-4-shared-preamble.md` §5 | Real-time verification gates between every commit |
| `stream-resilience.md` B1 | Plan body lives in 4 files; chat carries pointers |
| `stream-resilience.md` B2 | Each Write payload chunked < 2K tokens |
| `stream-resilience.md` B3 | 4-agent audit done (this v2 incorporates findings) |
| `stream-resilience.md` B6 | Every error has typed `ErrorCode` + runbook + triage |
| `stream-resilience.md` B7 | 5-sink fan-out via existing tracing-appender |
| `stream-resilience.md` B9 | DHAT + Criterion + budget entry |
| `stream-resilience.md` B10 | DEDUP UPSERT KEYS + ALTER ADD COLUMN IF NOT EXISTS |
| `audit-findings-2026-04-17.md` Rule 3 | Market-hours gate on tokio task |
| `audit-findings-2026-04-17.md` Rule 4 | Edge-triggered alerts (drift, drop SLA) |
| `audit-findings-2026-04-17.md` Rule 5 | flush/persist failures use `error!`, never `warn!` |
| `audit-findings-2026-04-17.md` Rule 11 | NO false-OK signals — coverage gate is positive progress signal |
| `audit-findings-2026-04-17.md` Rule 12 | Every counter panel wrapped in `increase()` / `rate()` |
| `security-id-uniqueness.md` (I-P1-11) | Composite `(security_id, segment)` everywhere |
| `hot-path.md` | papaya for concurrent shared state |
| `aws-budget.md` | 90d hot → 365d S3 IT → ≥1y Glacier; 100GB EBS hot tier |
| `testing-scope.md` | Scoped test runner picks up all 5 new modules |

## Honest 100% claim (forced PR-body wording per §8)

> "100% inside the tested envelope, with ratcheted regression coverage:
> 22-timeframe full-universe (~24,324 instruments) movers snapshots
> persisted at exact cadences from 1s to 1h. Sustained throughput
> ≥1M rows/sec validated by chaos test on c7i.xlarge. Drop rate
> ≤0.1% bounded by mpsc(8192) drop-newest + 0.1% SLA alert.
> Hot path zero-alloc (DHAT-pinned), O(1) papaya lookup
> (Criterion-pinned ≤50ns p99). Prev_close routing per segment
> verified per Dhan ticket #5525125. Composite-key uniqueness
> per I-P1-11. Market-hours gate prevents off-hours waste.
> 22 isolated writers prevent fan-in bottleneck. Beyond the
> envelope, rescue ring + DEDUP UPSERT collapse any replay."

## Go/no-go protocol

After file write + commit + push:

1. STOP — wait for explicit Parthiban approval on:
   - 5 open questions in `v2-risks.md`
   - Whether to proceed phase-by-phase or all-in
   - Whether to bump QuestDB ILP connection limit from 20 → 32
   - Whether to schedule the chaos test as PR-blocker or follow-up
2. Only on explicit GO does Phase 1 begin
3. After each phase, commit + push + verify gates green BEFORE the next phase
4. After Phase 6, run 3 adversarial agents on diff; fix CRITICAL/HIGH inline; re-run
5. Open PR as DRAFT initially; mark ready-for-review only after all 14 gates green
