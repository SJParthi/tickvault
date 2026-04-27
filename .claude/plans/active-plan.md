# Implementation Plan: Comprehensive Hardening — 14 Items / 3 Waves

**Status:** DRAFT
**Date:** 2026-04-27
**Approved by:** pending (await operator green-light to flip to APPROVED)
**Branch:** `claude/debug-session-error-6yqZV`
**Supersedes:** PR #391 (`claude/fix-websocket-startup-p0x1O`) — close after this PR opens
**Triggering incident:** Overnight 2026-04-26 → 2026-04-27 — app started 22:21 IST Sun, all 5 main-feed connections were dead at 09:00 IST Mon market open. Operator manually restarted at 09:03:51. Depth, stock_movers, option_movers, previous_close all broken simultaneously.

## Index of plan documents

| File | Content |
|------|---------|
| `active-plan.md` (this file) | Index, three principles, authoritative facts, 14-item summary, 9-box checklist, scenarios, verification gates, file totals |
| `active-plan-wave-1.md` | Wave 1 detail (Items 0–4: hot-path remediation + data integrity) |
| `active-plan-wave-2.md` | Wave 2 detail (Items 5–9: WS resilience, FAST BOOT, tick-gap, audit tables) |
| `active-plan-wave-3.md` | Wave 3 detail (Items 10–13: pre-open movers, Telegram dispatcher, self-test, SLO) |
| `active-plan-cross-cutting.md` | C1–C13 cross-cutting additions applied to ALL items |
| `active-plan-gaps.md` | G1–G21 gap closure (verified by 3 background agents 2026-04-27) |
| `active-plan-proof-matrix.md` | O(1) / Uniqueness / Dedup proof per artifact |
| `active-plan-realtime-slo.md` | Real-time check / guarantee / proof matrix |
| `active-plan-100pct-slo.md` | `tv_realtime_guarantee_score` SLO + `make 100pct-audit` + dashboard spec |
| `.claude/rules/project/stream-resilience.md` | B1–B12 protocol — permanent project rule |

## Three Principles (every item must pass all three)

| # | Principle | Mechanism in this plan |
|---|-----------|------------------------|
| 1 | **O(1) latency on hot path** | papaya HashMap for spot lookups; ArrayVec for batch buffers; rtrb SPSC for movers; arc-swap for snapshots; zero allocation in tick path |
| 2 | **Uniqueness guarantee** | `(security_id, exchange_segment)` composite key everywhere (I-P1-11) — movers, prev_close, dedup sets, tick-gap detector |
| 3 | **Deduplication** | QuestDB `DEDUP UPSERT KEYS(...)` includes segment in every new table; idempotent `ALTER ADD COLUMN IF NOT EXISTS` on every schema change |

## Authoritative Facts (do NOT relitigate)

| Fact | Source |
|------|--------|
| PrevClose packet (code 6) — IDX_I ONLY | `.claude/rules/dhan/live-market-feed.md` rule 7; Dhan Ticket #5525125 (2026-04-10) |
| For NSE_EQ → read `close` field from Quote packet bytes 38-41 (f32 LE) | Same rule 8 |
| For NSE_FNO → read `close` field from Full packet bytes 50-53 (f32 LE) | Same rule 10 |
| Movers = ALL equities + ALL F&O + ALL indices (no top-N truncation) | Operator directive 2026-04-27 |
| Pre-open buffer window 09:00..=09:12 IST | `.claude/rules/project/depth-subscription.md` 2026-04-24 §1 |
| Phase 2 trigger 09:13:00 IST | Same §4 |
| Streaming heartbeat 09:15:30 IST | Same §8 |
| Depth-200 root path `/?token=...` | `.claude/rules/dhan/full-market-depth.md` rule 2 |
| Trading-date timezone is IST (Asia/Kolkata) — NEVER UTC date | This plan G3 |
| First-Quote-per-day detection resets at IST 00:00 | This plan G2 |
| Token validity 24h JWT — sleep > 24h MUST refresh-on-wake | This plan G1 (verified by agent 3, 2026-04-27) |
| Telegram bot API limits — 30/sec global, 1/sec per chat, 20/min per chat | This plan G15 (verified by agent 3) |
| Hot path = `crates/core/src/pipeline/`, `crates/storage/src/tick_persistence.rs`, `crates/core/src/websocket/` ILP | `.claude/rules/project/hot-path.md` |

## The 14 Plan Items (3 Waves)

### Wave 1 — Hot-path & Data Integrity (PRECONDITION + P0)

| # | Item | Files | Detail |
|---|------|-------|--------|
| **0** | **Hot-path I/O + lock remediation** (precondition for Items 2/3/4) | `tick_processor.rs`, `tick_persistence.rs`, `top_movers.rs` | wave-1 §0 |
| 1 | Phase 2 ALWAYS-NOTIFY + EmitGuard (G7) | `main.rs`, `notification/events.rs` | wave-1 §1 |
| 2 | Stock + Index movers ALL ranks (no top-N) | `top_movers.rs`, `movers_persistence.rs` | wave-1 §2 |
| 3 | Option movers — 22K contracts every 5s | NEW `option_movers.rs`, `movers_persistence.rs` | wave-1 §3 |
| 4 | `previous_close` UN-DEPRECATE + segment-routed persist + IST-midnight reset (G2, G3) | NEW `previous_close_persistence.rs`, `tick_processor.rs` | wave-1 §4 |

### Wave 2 — Resilience (P1)

| # | Item | Files | Detail |
|---|------|-------|--------|
| 5 | Main-feed WS never-give-up + token-aware wake (G1) | `websocket/connection.rs`, `auth/token_manager.rs` | wave-2 §5 |
| 6 | Depth-20 + Depth-200 + Order-Update never-give-up | `websocket/depth_connection.rs`, `websocket/order_update.rs` | wave-2 §6 |
| 7 | FAST BOOT race fix (60s deadline, escalating logs) (G14) | `main.rs`, `tick_persistence.rs` | wave-2 §7 |
| 8 | Tick-gap RCA — composite-key papaya + 60s coalesce (G4) | `tick_processor.rs`, `subscription_planner.rs` | wave-2 §8 |
| 9 | 6 audit tables with retention policy | NEW `*_audit_persistence.rs` x 6 | wave-2 §9 |

### Wave 3 — Visibility & SLO (P2)

| # | Item | Files | Detail |
|---|------|-------|--------|
| 10 | Pre-open movers (09:00..=09:12) | NEW `preopen_movers.rs` | wave-3 §10 |
| 11 | **Telegram dispatcher hardening: bucket + coalescer + drop counter (G15)** | `notification/telegram.rs` | wave-3 §11 |
| 12 | `MarketOpenSelfTestPassed` at 09:16 IST | `main.rs`, `notification/events.rs` | wave-3 §12 |
| 13 | `tv_realtime_guarantee_score` SLO + `make 100pct-audit` + Grafana 100pct dashboard | `observability.rs`, `Makefile`, `deploy/docker/grafana/dashboards/100pct.json` | wave-3 §13 |

## 9-Box Observability Checklist (14 × 9 = 126 cells)

Every item must complete these 9 boxes before marked done:

1. Typed event in `crates/core/src/notification/events.rs`
2. ErrorCode variant in `crates/common/src/error_code.rs`
3. `tracing::error!/warn!/info!` with `code = ErrorCode::X.code_str()` field
4. Prometheus counter/gauge/histogram emission
5. Grafana panel under `deploy/docker/grafana/dashboards/`
6. Prometheus alert rule under `deploy/docker/prometheus/alerts.yml`
7. Production call site (rule 13 — defined-but-never-called = bug)
8. Triage YAML rule in `.claude/triage/error-rules.yaml`
9. Ratchet test under `crates/*/tests/` proving the wiring

Detail per item in `active-plan-wave-{1,2,3}.md`.

## Ordering & Dependencies

| Constraint | Why |
|---|---|
| Item 0 BEFORE Items 2/3/4 | Items 2/3/4 add I/O onto a stalling tick processor |
| Items 4 BEFORE Item 13 | SLO score depends on `tv_prev_close_persisted_total{source}` |
| Item 11 BEFORE Items 1/12 | Phase2 + SelfTest events ride the hardened Telegram dispatcher |
| Item 5 BEFORE Item 6 | Same supervisor pattern — main-feed first as reference |
| Items 9 BEFORE Item 13 | Audit tables feed the 100pct dashboard panels |

## Files Touched (estimate)

| Layer | Files | LoC delta |
|-------|-------|-----------|
| `crates/core/` parser + pipeline + websocket + auth | 9 | +1,200 |
| `crates/storage/` (movers + prev_close + 6 audit modules + tick_persistence) | 10 | +900 |
| `crates/app/main.rs` boot + dispatcher wiring | 1 | +500 |
| `crates/common/` types + error_code + config | 4 | +250 |
| `crates/core/src/notification/` events + telegram dispatcher | 2 | +400 |
| Tests (DHAT + Criterion + loom + chaos + ratchets) | ~50 new files | +2,800 |
| Grafana dashboards | 100pct.json (new), operator-health (extend), market-data (extend) | +500 lines JSON |
| Prometheus alert rules | `alerts.yml` | +120 |
| Triage YAML | `.claude/triage/error-rules.yaml` | +60 |
| Runbooks | `docs/runbooks/` (one per new ErrorCode) | +900 |
| Plan documents (THIS PR ONLY) | 10 files | +1,990 |
| **TOTAL across 3 PRs** | ~85 files | +9,620 LoC |

## Verification Gates

| Gate | Command | Blocks merge? |
|------|---------|---------------|
| Plan items all checked | `bash .claude/hooks/plan-verify.sh` | yes |
| Scoped tests pass (FULL_QA forced — touches `crates/common/`) | `FULL_QA=1 make scoped-check` | yes |
| Hot-path zero-alloc | `cargo test --test 'dhat_*' --workspace` | yes |
| Banned-pattern scan (incl. new category 7 — sync fs in hot path) | `bash .claude/hooks/banned-pattern-scanner.sh` | yes |
| Workspace test | `cargo test --workspace` | yes |
| Bench gate (Criterion 5% regression cap) | `bash scripts/bench-gate.sh` | yes |
| 100% audit dashboard | `make 100pct-audit` exit 0 (all 14 sub-checks pass) | yes |
| Real-time proof | `make doctor` shows all 4 WS connected + movers tables non-empty + previous_close populated for 3 segments | manual |

## Scenarios Covered (15)

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App starts Sunday 22:00 IST, runs through to Monday 09:00 | All 4 WS connected automatically at 09:00 (no manual restart) |
| 2 | Network blip 02:00 IST (off-market) | Connections sleep until 09:00, then reconnect — no Telegram spam |
| 3 | Phase 2 plan empty | `Phase2Failed{...}` Telegram with diagnostic, NOT silent |
| 4 | 09:15:00 sharp | Option movers task starts; first snapshot persisted by 09:15:05 |
| 5 | NSE_EQ RELIANCE first Quote of day | `previous_close` row written for (RELIANCE, NSE_EQ) from byte 38-41 |
| 6 | NSE_FNO option contract first Full of day | `previous_close` row from byte 50-53 |
| 7 | IDX_I NIFTY=13 PrevClose packet (code 6) | `previous_close` row from byte 8-11 |
| 8 | QuestDB Docker not yet up at boot | Tick processor sleeps up to 60s; rescue ring silent during boot window; CRITICAL alert at 30s |
| 9 | Connection 0 task crashes mid-day | Pool supervisor respawns within 5s; SubscribeRxGuard restores subscriptions |
| 10 | Operator queries `select count(*) from stock_movers where ts > now()-1h` | Returns ~239 stocks × 720 snapshots = 172,000 rows |
| 11 (G1) | App sleeps Friday 15:30 → Monday 09:00 IST (65.5h) | Token force-renewed on wake before reconnect; no DH-901 noise |
| 12 (G4) | 406 instruments tick-gap simultaneously | 1 coalesced Telegram summary every 60s, NOT 406 alerts |
| 13 (G7) | Phase2 dispatcher early-return without emitting | Debug build panics; release build emits `PHASE2-02` ERROR |
| 14 (G8) | System clock drifts 90s pre-market | Boot probe emits CRITICAL `tv_clock_skew_seconds > 2`; HALT |
| 15 (G14) | Cold Docker pull 30–60s on first boot | Tick processor `wait_for_questdb_ready(60s)` with 5/10/20/30s escalating logs |

## Plan-enforcement protocol

1. This file (DRAFT) presented to operator.
2. On approval → Status: APPROVED → start Wave 1 implementation in a new PR.
3. Each Wave = one PR. Wave PR is mergeable only when `bash .claude/hooks/plan-verify.sh` passes for items in that wave.
4. After all 3 Wave PRs merge → mark this plan VERIFIED → archive to `.claude/plans/archive/2026-04-27-comprehensive-hardening.md`.
5. PR #391 closed when Wave 1 PR opens (referencing this plan).
