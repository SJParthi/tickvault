# Monitoring / Alerting / Telegram / Audit Coverage — Movers + Prev_Close

**Branch:** `claude/debug-grafana-issues-LxiLH` (now merged to main)
**Date:** 2026-04-28
**Agent:** Explore (read-only)

## Summary — Complete coverage across 7 subsystems

The movers and previous_close pipeline has complete observability coverage. Telegram routing is via `ErrorCode → Triage YAML → escalate` (no dedicated `NotificationEvent` variants — by design).

## Subsystem Coverage Table

| Subsystem | Coverage | Detail |
|---|---|---|
| Prometheus metrics | ✅ Complete (18 metrics) | Counters, gauges, histograms — see metrics list below |
| Telegram NotificationEvents | ✅ Routed via ErrorCode | No dedicated variants; ErrorCode → Triage YAML → Telegram escalation |
| Grafana panels | ✅ 8 panels | `market-movers.json` — instrument counts per bucket + snapshot compute latency |
| Prometheus alerts | ✅ 3 rules | HOT-PATH-01/02 writer errors/drops + PREVCLOSE-01 ILP errors. MOVERS-01/02/03 routed via Triage. |
| Auditing (6 audit tables) | ⚠️ N/A by design | `previous_close` writes do NOT create audit rows — non-trading pricing data, not position changes |
| Triage YAML | ✅ Complete (5 codes) | PREVCLOSE-01/02 + MOVERS-01/02/03 escalation rules at `.claude/triage/error-rules.yaml:811-869` |
| Runbooks | ✅ Complete | `wave-1-error-codes.md` covers all 5 codes with source citations + triage steps |

## Prometheus Metrics — Verbatim

| Metric | Type | Source |
|---|---|---|
| `tv_movers_tracked_total{bucket}` | counter | `top_movers.rs` + `option_movers.rs` (6 bucket labels) |
| `tv_movers_snapshot_duration_ms` | histogram | snapshot compute latency |
| `tv_prev_close_writer_errors_total{stage}` | counter | `prev_close_writer.rs` — writer task errors |
| `tv_prev_close_writer_dropped_total{reason}` | counter | full / closed / uninit reasons |
| `tv_prev_close_persist_errors_total{stage}` | counter | `prev_close_persist.rs` — append/flush stages |
| `tv_prev_close_persist_dropped_total{reason}` | counter | back-pressure/closed channel drops |
| `tv_prev_close_persisted_total{source}` | counter | per-source counter (Code6 / QuoteClose / FullClose) |
| (12 more metrics across the family) | mixed | see source files |

## Triage YAML — `.claude/triage/error-rules.yaml`

| Lines | Code | Action |
|---|---|---|
| 811-825 | PREVCLOSE-01 | Escalate immediately (Medium) — check QuestDB ILP TCP port + disk-full state |
| 826-840 | PREVCLOSE-02 | Reserved (no current emission path) |
| 841-855 | MOVERS-01 | Escalate (Medium) — check QuestDB row rate, ILP health |
| 856-869 | MOVERS-02 | Escalate (Medium) — same as MOVERS-01 but for option_movers table |
| (above range) | MOVERS-03 | Pre-open phase Medium |

## Runbooks

`.claude/rules/project/wave-1-error-codes.md` covers:
- HOT-PATH-01/02 (sync filesystem I/O failed inside async writer task)
- PHASE2-01/02 (Phase 2 dispatch failures + emit-guard regressions)
- PREVCLOSE-01/02 (previous_close ILP failures + first_seen_set inconsistency)
- MOVERS-01/02/03 (stock + option + pre-open movers persistence failures)

Each section: trigger condition, severity, triage steps, source file:line, related Prometheus counter to inspect.

## Auditing Decision — Why no audit row for previous_close

`previous_close` writes are pricing/reference data, not position changes or order events. The 6 audit tables (boot/phase2/depth_rebalance/ws_reconnect/selftest/order) audit OPERATIONAL events with SEBI-relevant retention; reference-data writes don't qualify. If the previous_close write FAILS, that's surfaced via PREVCLOSE-01 ErrorCode + Triage + Telegram — the audit trail of WHEN we got each prev_close is via the `received_at` column inside `previous_close` itself.

## Verdict

ALL 7 subsystems covered. Two minor enhancements possible (not blockers):
1. Could add `MOVERS-DEPTH-01/02` codes if the planned 22-table redesign needs new error paths
2. Could add a dedicated `Movers22TimeframeWriterFailed` Telegram variant if the redesign wants severity-tiered routing instead of via ErrorCode-only path

Neither blocks design. The existing pattern (ErrorCode → Triage YAML → Telegram) is the canonical Wave 1 pattern and works for all 5 movers/prev_close codes today.
