# Movers 22-TF v3 — Phased commits + verification gates + go/no-go

## 13 phased commits (Phase 5 done; depth ships before movers Phase 1-7)

| Phase | Commit | Files | LoC | Status |
|---|---|---|---|---|
| 5 | `fix(subscription): stock F&O expiry rollover T-only (was T-1)` — constant 1→0; 5 test rewrites; rule file + runbook update | `constants.rs`, `subscription_planner.rs`, `depth-subscription.md`, `expiry-day.md` | ~300 | **DONE (commit d428835)** |
| 6 | **`chore(depth-200): wipe off /twohundreddepth dead code`** — delete `examples/depth_200_variants.rs` (~400 LoC of diagnostic), strip historical narrative comments referencing `/twohundreddepth` from prod files; KEEP the locked-fact ratchet + banned-pattern hook + one-line LOCKED FACT comment | `examples/depth_200_variants.rs` (DELETE), `depth_connection.rs` (comments only), `dhan_locked_facts.rs` (unchanged), `config.rs` (unchanged) | ~50 net LoC removed | **NEXT** |
| 7 | **`feat(depth-20): dynamic top-150 selector + 1-minute Swap20 rebalance`** — new module `depth_20_dynamic_subscriber.rs`; queries `option_movers` table per minute, picks top 150 by `volume DESC` filtered by `change_pct > 0`; emits `Swap20 { unsubscribe: leavers, subscribe: entrants }` over existing channels; 4 new ratchets; uses `DEPTH-DYN-01/02` ErrorCodes (already RESERVED in `wave-4-error-codes.md`) | `depth_20_dynamic_subscriber.rs` (new), `main.rs` (boot wiring slot 3/4/5), `error_code.rs` (DEPTH-DYN-01/02 promote from RESERVED to defined), `error-rules.yaml`, `wave-4-error-codes.md` (RESERVED → defined), tests | ~600 | After 6 |
| 8 | `feat(storage): 22 movers_{T} tables + DDL + DEDUP + partition manager + S3 lifecycle` (was Phase 1) | `materialized_views.rs`, `movers_22tf_persistence.rs`, `partition_manager.rs`, `s3-lifecycle-movers-tables.json`, 4 schema/lifecycle tests | ~500 | After 7 |
| 9 | `feat(common): MoverRow 26-column Copy struct + 22-tf constants` (was Phase 2) | `mover_types.rs`, `constants.rs`, 3 tests | ~250 | After 8 |
| 10 | `feat(core): papaya MoversTracker + arena snapshot + scheduler + supervisor + market-hours gate + 3-tier prev_close + F&O expiry filter + depth-cache lookup + spot-price lookup` (was Phase 3) | `top_movers.rs`, `movers_22tf_scheduler.rs`, `movers_22tf_supervisor.rs`, `movers_22tf_writer_state.rs`, 12 unit + supervisor + depth-cache integration tests | ~1,000 | After 9 |
| 11 | `feat(observability): 10 metrics + 3 ErrorCodes + Triage YAML + 24-panel Grafana + 4 alert rules + drift/drop SLA + runbook` (was Phase 4) | `error_code.rs`, `events.rs`, `error-rules.yaml`, `movers-22tf.json` (24 panels: Stocks 2 + Index 2 + Options 7 + Futures 7 + Depth 6), `alerts.yml`, `wave-4-error-codes.md` (MOVERS-22TF-01..03 entries), runbook | ~600 | After 10 |
| 12 | `test(movers): 47 ratchets + 2 DHAT + 3 Criterion + 1 chaos test (1M rows/sec)` (was Phase 6) | tests/, benches/, `benchmark-budgets.toml`, `chaos_movers_22tf_throughput.rs` | ~800 | After 11 |
| 13 | `chore(hooks): banned-pattern cats 8/9/10 + make doctor sections 8/9 + dedup_segment_meta_guard extension` (was Phase 7) | `banned-pattern-scanner.sh`, `Makefile`, `dedup_segment_meta_guard.rs`, `scripts/doctor.sh` | ~200 | After 12 |
| **Total NEW LoC after Phase 5** | | | **~4,000** | (split across 3 PRs below) |

## PR splits (per stream-resilience.md B12 ≤ 3K LoC cap)

| PR | Phases | LoC | Independence |
|---|---|---|---|
| PR-A (already merged via commit `d428835`) | Phase 5 | ~300 | Independent of movers + depth |
| **PR-B (NEXT)** | Phase 6 + Phase 7 | ~650 | Independent of movers; uses existing `option_movers` table |
| PR-C | Phase 8 + 9 + 10 + 11 + 12 + 13 | ~3,350 | Movers core; depends on PR-B's `option_movers` queries to be in production |

## Pre-merge verification gates (16 gates, was 14)

- [ ] All 47 new ratchets pass
- [ ] 5 rewritten ratchets pass with new assertions
- [ ] 2 DHAT tests: `total_blocks == 0` on hot path
- [ ] 3 Criterion benches within budget (50ns / 11ms / 200ns)
- [ ] **1 chaos test ≥1M rows/sec sustained, ≤0.1% drop rate**
- [ ] Coverage 100% on 5 new modules
- [ ] `bash .claude/hooks/banned-pattern-scanner.sh` (cats 8/9/10 active)
- [ ] `bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all`
- [ ] `bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"`
- [ ] `bash .claude/hooks/plan-verify.sh` (status: VERIFIED)
- [ ] `make scoped-check` green; `FULL_QA=1 make scoped-check` green
- [ ] `make doctor` green post-boot, sections 8 + 9 PASS
- [ ] **Live boot test: stock F&O on simulated expiry day rolls to next expiry; non-expiry day keeps nearest**
- [ ] **Live boot test: 22 tables populated within 1s of market open + 24 Grafana panels render**
- [ ] `mcp__tickvault-logs__list_active_alerts` empty post-boot
- [ ] `mcp__tickvault-logs__list_novel_signatures` empty 5min post-boot
- [ ] 3 adversarial review agents (hot-path / security / general-purpose) on diff
- [ ] **Confirm Dhan support citation reference exists in commit 5**

## 9-box checklist per phase (per stream-resilience.md B8)

For each new code path:

| # | Item |
|---|---|
| 1 | typed `NotificationEvent` variant |
| 2 | `ErrorCode` enum variant + `code_str()` + `severity()` + `runbook_path()` |
| 3 | `tracing::error!` with `code = ErrorCode::X.code_str()` field |
| 4 | Prometheus counter (or gauge) with static labels |
| 5 | Grafana panel wrapping counter in `increase()` / `rate()` |
| 6 | Prometheus alert rule in `alerts.yml` |
| 7 | Call site exists (pub-fn-wiring-guard passes) |
| 8 | Triage YAML rule referencing the ErrorCode |
| 9 | Ratchet test pinning the wiring |

## Honest 100% claim (forced PR-body wording)

> "100% inside the tested envelope, with ratcheted regression coverage:
> 22-timeframe full-universe (~24,324 instruments) movers snapshots
> persisted at exact cadences from 1s to 1h. Sustained throughput
> ≥1M rows/sec validated by chaos test on c7i.xlarge. Drop rate
> ≤0.1% bounded by mpsc(8192) drop-newest + 0.1% SLA alert.
> Hot path zero-alloc (DHAT-pinned), O(1) papaya lookup
> (Criterion-pinned ≤50ns p99). Prev_close routing per segment
> verified per Dhan ticket #5525125. Composite-key uniqueness
> per I-P1-11. Market-hours gate prevents off-hours waste.
> 22 isolated writers prevent fan-in bottleneck. Depth columns
> populated from existing MarketDepthCache (read-only).
> Stock F&O expiry rollover changed from T-1 to T-only per
> Dhan trading restriction. Beyond the envelope, rescue ring +
> DEDUP UPSERT collapse any replay."

## Go/no-go protocol

After file write + commit + push:

1. STOP — wait for explicit Parthiban approval on:
   - 7 open questions in `v2-risks.md` (was 5; +2 for depth + expiry rollover)
   - Whether to ship expiry rollover (Phase 5) as SEPARATE PR (recommended) or bundled
   - Whether to bump QuestDB ILP connection limit from 20 → 32
   - Whether to schedule chaos test as PR-blocker or follow-up
2. Only on explicit GO does Phase 1 begin
3. After each phase: commit + push + verify gates green BEFORE next phase
4. After Phase 7: 3 adversarial agents on diff; fix CRITICAL/HIGH inline; re-run
5. Open PR(s) as DRAFT initially; mark ready-for-review only after all 16 gates green

## Cross-references baked in

| Rule file | Wave 4 charter element |
|---|---|
| `wave-4-shared-preamble.md` §1 | MCP tools at session start |
| `wave-4-shared-preamble.md` §2 | "100% inside tested envelope" wording |
| `wave-4-shared-preamble.md` §3 | 3-agent parallel review |
| `wave-4-shared-preamble.md` §4 | 7-layer observability |
| `stream-resilience.md` B1+B2 | Plan body in 4 files; chat carries pointers |
| `stream-resilience.md` B6 | Typed ErrorCode + runbook + triage |
| `stream-resilience.md` B9 | DHAT + Criterion + budget |
| `stream-resilience.md` B10 | DEDUP UPSERT KEYS + ALTER ADD COLUMN IF NOT EXISTS |
| `audit-findings-2026-04-17.md` Rule 3 | Market-hours gate |
| `audit-findings-2026-04-17.md` Rule 4 | Edge-triggered alerts |
| `audit-findings-2026-04-17.md` Rule 5 | flush/persist via `error!` |
| `audit-findings-2026-04-17.md` Rule 11 | NO false-OK signals |
| `audit-findings-2026-04-17.md` Rule 12 | Counters in `increase()`/`rate()` |
| `security-id-uniqueness.md` (I-P1-11) | Composite (security_id, segment) |
| `hot-path.md` | papaya for concurrent shared state |
| `aws-budget.md` | 90d hot → 365d S3 IT → ≥1y Glacier |
| `depth-subscription.md` 2026-04-24 §6 | **UPDATED** — stock expiry rollover constant 1→0 |
| `expiry-day.md` runbook | **UPDATED** — Dhan trading-restriction section |
