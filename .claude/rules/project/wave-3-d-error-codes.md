# Wave 3-D Error Codes

> **Authority:** This file is the runbook target for the new ErrorCode
> variants added in the Wave 3-D hardening implementation (Item 13 —
> composite real-time guarantee score gauge). Cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.

## SLO-01 — real-time guarantee score is healthy (≥ 0.95)

**Trigger:** the 10-second SLO scheduler in
`crates/core/src/instrument/slo_score.rs::evaluate_slo_score` returned
`SloOutcome::Healthy { score }` and the previous outcome was NOT
healthy (i.e. a falling-edge recovery from `Degraded` or `Critical`).
Severity::Info — informational positive ping. Edge-triggered only;
recurring healthy ticks do NOT spam.

**Why it exists:** SCOPE §13 demands a single-glance answer to the
operator's "is everything working?" question. Sustained green pings
would flood Telegram, so the scheduler emits SLO-01 only on the
recovery edge — when the system has just transitioned back to healthy
after a Degraded or Critical period (audit-findings Rule 4:
edge-triggered alerts only).

**Triage:**
1. None — this is a positive recovery confirmation.
2. If the operator ever sees SLO-01 immediately followed by SLO-02,
   the underlying degradation is flapping; investigate the weakest
   dimension named in the SLO-02 payload.

**Auto-triage safe:** YES (Info severity is always safe).

**Source:** `crates/core/src/instrument/slo_score.rs::evaluate_slo_score`,
notification variant `RealtimeGuaranteeHealthy` in
`crates/core/src/notification/events.rs`.

## SLO-02 — real-time guarantee score is degraded (< 0.95)

**Trigger:** the same 10-second scheduler returned
`SloOutcome::Degraded` (score in `[0.80, 0.95)`) or
`SloOutcome::Critical` (score `< 0.80`), AND the previous outcome
was either `Healthy` or one tier less severe (i.e. crossing into a
worse tier on a rising edge). Severity::High for `Degraded`,
Severity::Critical for `Critical`. Same edge-triggered semantics:
sustained-degraded ticks do NOT spam Telegram, only the rising edge
into a worse tier does.

**The six dimensions feeding the score:**

| Dimension | Definition | Failure meaning |
|---|---|---|
| `WS_health` | `(active_main + depth_20 + depth_200 + order_update) / expected` | WebSocket pool partially or fully down |
| `QDB_health` | `1` if QuestDB connected else `0` | persist failures cascading; rescue ring buffering |
| `Tick_freshness` | `1` if last tick observed `< 30s` ago (during market hours) | silent socket; depth/main feed stalled |
| `Token_freshness` | `1` if token expiry `> 4h` away | JWT will die mid-session unless force-renewed |
| `Spill_health` | `1` if `rate(tv_spill_dropped_total[5m]) == 0` | rescue ring overflow → DLQ |
| `Phase2_health` | `1` if today's Phase 2 outcome was `Complete` | stock F&O subscription empty for the day |

**Score = pure multiplicative product** of the six dimensions, all
clamped to `[0, 1]`. Any single dimension at `0` zeroes the score
(maps to `Critical`). Partial WS degradation drives the score
fractionally below `1.0`. See module docstring of `slo_score.rs`
for the formal definition + numerical-stability notes.

**Triage:**
1. Read the `weakest` field on the Telegram payload — it names the
   dimension that contributed the lowest individual value.
2. For each dimension, follow the corresponding runbook:
   * `WS_health` weakest → `.claude/rules/project/disaster-recovery.md`
     Scenario 5 / 6 (single connection drop / pool collapse).
   * `QDB_health` weakest → `BOOT-01` / `BOOT-02` runbook
     (`.claude/rules/project/wave-2-error-codes.md`).
   * `Tick_freshness` weakest → `WS-GAP-06` runbook
     (`.claude/rules/project/wave-2-error-codes.md`).
   * `Token_freshness` weakest → `AUTH-GAP-03` runbook
     (`.claude/rules/project/wave-2-error-codes.md`).
   * `Spill_health` weakest → `STORAGE-GAP-03` runbook + check
     `data/spill/` disk-free.
   * `Phase2_health` weakest → `PHASE2-01` runbook
     (`.claude/rules/project/wave-1-error-codes.md`).
3. Re-run `make doctor` + check
   `mcp__tickvault-logs__list_active_alerts` to correlate with the
   underlying typed alerts.

**Why we do not auto-fix:** SLO-02 is a *summary* signal. The
underlying typed errors (PHASE2-01, BOOT-01, AUTH-GAP-03, ...) each
carry their own runbook + auto-triage policy. Auto-fixing the
composite would mask the root cause; the operator (or the Claude
Code triage loop, per dimension-specific YAML rules) acts on the
typed underlying code, not on SLO-02 itself.

**Auto-triage safe:** NO (`Critical` is never auto-actioned per
`is_auto_triage_safe`; `Degraded` defers to dimension-specific
runbooks).

**Source:** `crates/core/src/instrument/slo_score.rs::evaluate_slo_score`,
notification variants `RealtimeGuaranteeDegraded` and
`RealtimeGuaranteeCritical` in `crates/core/src/notification/events.rs`.

**Ratchet tests:** see `slo_score.rs::tests` —
`test_score_is_one_when_all_dimensions_green`,
`test_score_is_zero_when_qdb_disconnected`,
`test_score_is_fractional_when_ws_partially_degraded`,
`test_classify_healthy_at_threshold_inclusive`,
`test_classify_degraded_band`,
`test_classify_critical_below_080`,
`test_weakest_dimension_is_the_minimum_input`,
plus the Criterion bench `bench_score_compute_le_1us` in
`crates/core/benches/slo_score.rs`.

## The honest 100% claim

Per `stream-resilience.md` Section 7 of the Wave 3-D scope: SLO-02
fires CRITICAL only when the composite drops below `0.80`. That is
the boundary of the tested envelope — beyond it, the typed underlying
codes are the authoritative diagnostic. The score itself does NOT
promise "WebSocket never disconnects" or "QuestDB never fails" — it
provides a coalesced, real-time, edge-triggered summary so the
operator's "is everything working?" question has a single
non-hallucinated answer at any moment within market hours.
