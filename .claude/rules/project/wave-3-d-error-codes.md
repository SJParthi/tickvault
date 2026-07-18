# Wave 3-D Error Codes

> **⚠ SLO PUBLISHER PARKED 2026-07-13 (PR-C2 — Dhan live-WS lane deletion; operator PARK ruling recorded in `.claude/plans/active-plan-dhan-live-off-c2-lane-deletion.md`):** the 10s SLO evaluator/publisher (`spawn_slo_publisher_task` / `spawn_supervised_slo_publisher` in main.rs) was DELETED with the lane — its six dimensions were dominated by the retired Dhan inputs (WS pool health, Dhan tick freshness, Phase-2 outcome), so the composite could never again read honestly on the Groww-only runtime. Effects: `tv_realtime_guarantee_score` + the `tv_realtime_guarantee_dimension` gauges are no longer published; the `tv-<env>-realtime-guarantee-critical` + `-degraded` CloudWatch alarms were REMOVED in the SAME PR (2026-07-14 truth-sync: an earlier revision of this banner said "full terraform removal is a flagged Phase C follow-up" — stale; PR-C2 itself retired both alarms, their window-gate armed-list entries, the score's EMF allowlist rows, and its fallback log-metric-filter — see the dated notes in app-alarms.tf / silent-feed-alarms.tf / metrics-log-metric-filters.tf); the SLO-01/SLO-02 emit sites died with the publisher; SLO-03 (the publisher-respawn supervisor code) has no emitter. The `slo_score.rs` pure evaluator + the `SloXX` ErrorCode variants were RETAINED (contract stubs) pending either a Groww-scoped SLO re-design with a fresh dated operator quote, or Phase C variant cleanup — **the C4 sweep (2026-07-15) executed that cleanup: `slo_score.rs`, its bench, its budget keys, and the SLO-01/02/03 variants are DELETED** (a future Groww-scoped SLO re-design starts fresh with its own dated quote). The market-hours liveness alarm was re-pointed off the score to `tv_groww_exchange_lag_p99_seconds` in Phase A (2026-07-13). Content below retained for historical audit.

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
| `Tick_freshness` | fraction of subscribed SIDs with a tick in the last `30s` (`1 − silent/universe`, clamped; INDIA VIX excluded from the silent count; pinned `1` outside market hours AND during the NSE pre-open window [09:00, 09:15) IST — the 2026-07-08 pre-open pin: auction-window silence is not degradation, and the 9-of-15 degraded alarm's lookback at the 09:20 gate-open must not hold pre-open breaching datapoints; fractional coverage per the 2026-07-03 #1342 fix, computed by `compute_tick_freshness`) | silent socket; depth/main feed stalled |
| `Token_freshness` | `1` if token expiry `> 4h` away | JWT will die mid-session unless force-renewed |
| `Spill_health` | `1` if `rate(tv_spill_dropped_total[5m]) == 0` *(metric + alarm retired 2026-07-18, stage-4 — emit sites died with the stage-2 tick-chain deletion)* | rescue ring overflow → DLQ |
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
   `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) to correlate with the
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

## SLO-03 — SLO evaluator/publisher task died and was respawned

**Trigger:** the 10-second SLO evaluator/publisher task (the scheduler in
`crates/app/src/main.rs` that samples the six dimensions, calls
`evaluate_slo_score`, and emits `tv_realtime_guarantee_score` + the six
`tv_realtime_guarantee_dimension` gauges) exited — panic, external cancel,
or unexpected clean return. The task is an infinite loop, so ANY resolution
of its `JoinHandle` is abnormal. The supervisor
(`spawn_supervised_slo_publisher` in `crates/app/src/main.rs` — mirrors
`spawn_supervised_oom_monitor` / DISK-WATCHER-01 / WS-GAP-05) caught the
death, logged `error!(code = "SLO-03", reason, ...)`, incremented
`tv_slo_publisher_respawn_total{reason}` (`reason` ∈ `panic` / `cancelled` /
`clean_exit` / `unknown` per `classify_join_exit`), waited the bounded 5s
backoff (`SLO_PUBLISHER_RESPAWN_BACKOFF_SECS`), and respawned the publisher
so the metric stream resumes.

**Why this code exists (live incident 2026-07-03 10:35 IST):** the publisher
was a bare `tokio::spawn` with a dropped `JoinHandle`. It died silently
mid-market at 10:35 IST (context: WS-GAP-06 tick-gap storm + a STAGE-C.2b
LiveFeed re-injection burst, dropped=1,127,801, at 10:36) — last
`tv_realtime_guarantee_score` datapoint 10:35, no `error!` (a tokio task
panic prints to stderr, not the tracing pipeline), no respawn. The
guarantee-critical alarm false-OK'd on missing data (missing→NonBreaching);
only the `market-hours-liveness-missing` alarm caught it. Severity::High —
the respawn self-heals, but a dying publisher blinds the guarantee alarm,
so the operator must see it.

**Triage:**
1. A one-off SLO-03 with the metric stream resumed = healthy self-heal;
   read the `reason` field and (for `reason="panic"`) the backtrace in
   `data/logs/errors.jsonl.*` / stderr at leisure.
2. A sustained `tv_slo_publisher_respawn_total` rate (respawn storm) means
   the publisher keeps dying — a real bug in the loop (QDB probe, tick-gap
   scan, token headroom read). Capture the backtrace preceding each SLO-03
   line and file it; restart the app to reset from clean state if it flaps.
3. `reason="cancelled"` at shutdown is benign (runtime teardown).
4. Cross-check `tv_realtime_guarantee_score` in CloudWatch — datapoints
   must resume within ~15s of the SLO-03 line.

**Auto-triage safe:** YES (Severity::High; the respawn already restored the
metric stream — the operator inspects the reason/backtrace, never
auto-fixes anything).

**Source:** `crates/app/src/main.rs` (`spawn_slo_publisher_task` /
`spawn_supervised_slo_publisher` + the once-per-process spawn guard at the
`start_dhan_lane` feature-gated site),
`crates/common/src/error_code.rs::Slo03PublisherRespawned`,
`crates/storage/src/disk_health_watcher.rs::classify_join_exit` (shared
exit classifier). Wiring ratchet:
`crates/core/src/auth/secret_manager.rs::test_slo_publisher_supervisor_is_wired_into_main`.

## The honest 100% claim

Per `stream-resilience.md` Section 7 of the Wave 3-D scope: SLO-02
fires CRITICAL only when the composite drops below `0.80`. That is
the boundary of the tested envelope — beyond it, the typed underlying
codes are the authoritative diagnostic. The score itself does NOT
promise "WebSocket never disconnects" or "QuestDB never fails" — it
provides a coalesced, real-time, edge-triggered summary so the
operator's "is everything working?" question has a single
non-hallucinated answer at any moment within market hours.
