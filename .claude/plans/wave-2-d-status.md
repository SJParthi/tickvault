# Wave-2-D Status Report (claude/wave-2-d-resilience-weCug)

**Generated:** 2026-04-28
**Branch:** `claude/wave-2-d-resilience-weCug`
**Base:** `dce0a3f` (main, post Wave-2-A/B/C)

## Executive summary

The Wave-2-D mega prompt asks for Items 8 + 9 + C11. **An audit of the
current `main` shows ~90% of this work was already shipped in PRs
#397/#398/#399/#400.** The scope-as-written would mostly be re-creating
files that already exist in identical form. This report documents what
is already shipped and the surgical fills that remain.

## Already shipped on main (verified, not assumed)

### Item 8 — Tick-gap detector

| Asset | File | LoC | Status |
|---|---|---|---|
| Detector core | `crates/core/src/pipeline/tick_gap_detector.rs` | 274 | composite-key papaya, scan, daily reset, global handle |
| Unit tests (9) | same file `mod tests` | — | composite-key, sort, reset, idempotent install, etc. |
| Hot-path wiring | `crates/core/src/pipeline/tick_processor.rs:1082` | — | `record_tick_global(...)` on every tick |
| Boot wiring | `crates/app/src/main.rs:244-282` | — | install + 60s coalescing task |
| Coalesce task emits | `tracing::error!(code = WS-GAP-06)` w/ top-10 samples | — | feeds Loki → Telegram via existing routing |
| DHAT zero-alloc | `crates/core/tests/dhat_tick_gap_detector.rs` | — | exists |
| Criterion bench | `crates/core/benches/tick_gap_detector.rs` | — | exists |
| `WsGap06TickGapSummary` ErrorCode | `crates/common/src/error_code.rs:198` | — | live |

### Item 9 — Audit-table persistence (6 modules)

| Module | LoC | Status |
|---|---|---|
| `phase2_audit_persistence.rs` | 179 | exists |
| `depth_rebalance_audit_persistence.rs` | 164 | exists |
| `ws_reconnect_audit_persistence.rs` | 171 | exists |
| `boot_audit_persistence.rs` | 141 | exists |
| `selftest_audit_persistence.rs` | 143 | exists |
| `order_audit_persistence.rs` | 180 | exists |
| ErrorCodes `Audit01..06`, `StorageGap03/04` | `error_code.rs:208-222` | live |
| S3 lifecycle JSON | `deploy/aws/s3-lifecycle-audit-tables.json` | exists |
| Grafana dashboard | `deploy/docker/grafana/dashboards/audit-trail.json` | exists (8 titles) |
| Existing `tv-questdb-disconnected` alert | `alerts.yml` | exists |

### C11 — DR doc

`disaster-recovery.md` already has scenarios 12 + 13 — but they cover
**Boot-time QuestDB readiness** (Item 7) and **Regulatory-audit
reconstruction** (Item 9). These slots are populated; the prompt's
proposed "Overnight wake / Holiday wake" content does not have a slot.

## Surgical gaps (the only real work left)

### G1 — `reset_daily()` is defined but never called

`tick_gap_detector::TickGapDetector::reset_daily()` exists with a unit
test, but **no call site** in production code. This is a literal
audit-findings.md Rule 13 violation ("if a method exists + is tested
but is never called, it IS a bug"). Need a tokio task that fires at
15:35 IST daily and invokes `reset_daily()`.

### G2 — Three missing Prometheus alerts

`alerts.yml` has 29 alerts. Missing per the prompt:
- `tv-audit-write-failure` — `rate(tv_audit_write_errors_total[5m]) > 0.0167` (1/min)
- `tv-s3-archive-failure` — `increase(tv_s3_archive_errors_total[1h]) > 0`
- `tv-tick-gap-coalesce-storm` — `rate(tv_tick_gap_summary_total[1m]) > 1.67` (>100/min)

### G3 — Typed `NotificationEvent::TickGapsSummary` (optional polish)

The coalesce task currently uses `tracing::error!(code = "WS-GAP-06", …)`
which routes via Loki → Telegram. A typed `NotificationEvent` variant
would be cleaner, but the existing path is functionally correct and
already covered by ratchets. **Skip unless operator explicitly wants it.**

### G4 — DR doc rewrite of scenarios 5-8 + new "Overnight/Holiday wake"

`disaster-recovery.md` scenarios 5-8 already mention Wave-2-A/B/C
behaviour ("REWRITTEN — Wave 2 Item 5/6"). Reading them shows the
content is up-to-date. The "Overnight wake (Fri 15:30 → Mon 09:00, 65h)"
and "Holiday wake (~92h)" content is implicit in scenario 5's wording
("up to ~65h overnight (Fri 16:00 → Mon 09:00 sleep)"), but a dedicated
sub-section would help operators searching for those specific windows.

## What I propose

Given Wave-2-D is mostly already on main, I suggest a surgical commit
that closes the four gaps above (G1 + G2 + G4) — call it
`feat(wave-2-d): close residual gaps from Wave-2-D mega-prompt audit`.

That ships:
- A 15:35 IST reset task in `main.rs` (G1)
- 3 new entries in `alerts.yml` (G2)
- Two new sub-sections in `disaster-recovery.md` (G4)
- Tests for each:
  - `test_tick_gap_reset_daily_called_at_1535_ist_via_scheduler` (source-scan guard)
  - `test_audit_write_failure_alert_present` / S3 / coalesce-storm
  - DR doc has no test (markdown), but the cross-ref test already
    enforces every ErrorCode → rule-file linkage.

Skips G3 (typed `TickGapsSummary` event) — current `error!` path is
correct and already routes to Telegram.

## Honest scope qualifier

This is a small, focused commit (~150 LoC across 3 files) — not the
multi-thousand-LoC PR the prompt envisions. **Operator confirmation
requested before pushing**, given the gap between the prompt's stated
scope and the actual gap surface.

If the operator wants the full prompt scope executed regardless of
duplication (e.g., to force a fresh review pass on the foundation), say
so explicitly and I will proceed — it will be a multi-session piece of
work given context budget.
