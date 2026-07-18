# =============================================================================
# Silent-feed degradation alarms — 2026-07-06 incident hardening.
# =============================================================================
# THE INCIDENT (2026-07-06): the Dhan feed degraded ALL day with FOUR
# independent signals and ZERO pages:
#   1. exchange->receive lag p99 46s / max 199s (no metric existed for it)
#   2. 29-67 of 776 instruments silent EVERY minute (old tick-gap alarm
#      threshold 100 never crossed — retuned to 40 PROVISIONAL in
#      app-alarms.tf #5; sits above the documented ~33 always-silent
#      healthy floor, round-3 correction 2026-07-08)
#   3. 125 SLO score crossings in the 0.94-0.97 band (in-app SLO-02 is
#      Telegram-suppressed/log-only since 2026-05-11; the CW
#      realtime-guarantee alarm only fires < 0.80 — a dead band this file's
#      degraded alarm closes)
#   4. BOUNDARY-01 catch-up sealing 9k-11.5k seals/10min (never exported to
#      CW at all — per-feed export added 2026-07-06 to cloudwatch-agent.json
#      + user-data.sh.tftpl)
#
# All 3 alarms here follow the house market-hours-gate pattern
# (audit-findings Rule 3): actions_enabled=false + appended to the window-gate
# Lambda ALARM_NAMES (market-hours-liveness-alarm.tf, 09:20-15:35 IST Mon-Fri).
# Edge-triggering is CloudWatch-native (SNS fires on OK->ALARM transition only
# — audit-findings Rule 4); treat_missing_data=notBreaching everywhere (the
# box is intentionally stopped nightly at 16:30 IST + weekends).
#
# Namespace / dimensions / SNS reuse the module-global locals declared in
# app-alarms.tf (local.app_namespace / local.app_dimensions /
# local.app_alarm_actions / local.app_alarm_ok).
#
# COST (dated 2026-07-06): +3 metric NAMES = 4 new CloudWatch custom-metric
# series (~$1.20/mo: 2x tv_boundary_catchup_total under [host,feed]
# (feed=dhan|groww), 1x tv_dhan_exchange_lag_p99_seconds, 1x
# tv_dhan_lag_samples_excluded_total) + 3 alarms (~$0.30/mo) ≈ $1.50/mo —
# inside the $35/mo pre-GST budget alarm ceiling. aws-budget.md carries the
# matching dated note.
# COST UPDATE (2026-07-17, stage-3 dead-WS sweep): the boundary-catchup
# alarm + its 2 [host,feed] series retired (S2 note below) — net
# ≈ -$0.70/mo vs the 2026-07-06 figure.
# =============================================================================

# ---------------------------------------------------------------------------
# RETIRED (PR-C2, 2026-07-13): `realtime_guarantee_degraded`
# (tv_realtime_guarantee_score 0.80-0.95 dead-band) — the SLO
# evaluator/publisher was deleted per the operator PARK ruling
# (wave-3-d-error-codes.md banner), so the score is never published again.
# Removed with its window-gate entry. The 2026-07-06 silent-feed dead-band
# coverage lives on in the per-feed lag + catch-up-storm alarms below.
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# S2. BOUNDARY-01 catch-up seal storm — RETIRED 2026-07-17 (stage-3 dead-WS
# sweep). The alarm's metric, tv_boundary_catchup_total, lost its LAST
# possible writer with this PR: the 21-TF TICK aggregator (the watermark
# catch-up sealer that incremented it) is DELETED — no tick publisher exists
# on the REST-only runtime, so the per-feed series can never emit a
# datapoint again. The S2 dormancy note (dated 2026-07-14, PR-C2) had
# assigned this alarm's removal-vs-retain fate to the aggregator-chain
# deletion PR — this is that PR. Removed in lockstep: the window-gate
# ALARM_NAMES entry (market-hours-liveness-alarm.tf), the dashboard
# catch-up widget + alarm-strip ARN (dashboard.tf), and the second
# [host,feed] EMF metric_declaration in cloudwatch-agent.json /
# user-data.sh.tftpl. Cost: -1 alarm (~-$0.10/mo) - 2 [host,feed] series
# (~-$0.60/mo) — dated note in aws-budget.md (COST NOTE 2026-07-17,
# stage-3). The BOUNDARY-01 runbook (wave-6-error-codes.md) carries the
# matching EMIT-SITES-DELETED banner; the ErrorCode variant is RETAINED.
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# S3 RETIRED (2026-07-17 — dashboard tidy): the dhan_exchange_lag_p99_high
# alarm watched tv_dhan_exchange_lag_p99_seconds, whose ONLY publisher
# (`run_dhan_lag_publisher` in feed_lag_monitor.rs) lost its spawn site +
# tick source with the Dhan live-WS lane deletion (PR-C2, 2026-07-13) and is
# DELETED in this PR together with the whole ring/publisher half of the
# feed-lag monitor. The gauge (and its companion counter
# tv_dhan_lag_samples_excluded_total) can never be published again, so the
# alarm was a permanently-missing-data dead monitor. Removed with its
# window-gate ALARM_NAMES row and the 2 EMF allowlist entries; dated cost
# note in aws-budget.md (COST NOTE 2026-07-17, dashboard tidy).
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# S4 RETIRED (2026-07-15 — Groww live-feed retirement): the
# groww_exchange_lag_p99_high alarm watched tv_groww_exchange_lag_p99_seconds,
# whose ONLY sample producer (record_groww_tick in the deleted Groww bridge)
# died with the Groww live feed — the gauge is never published again, so the
# alarm was a permanently-missing-data dead monitor. Removed with its
# window-gate ALARM_NAMES row, EMF allowlist entry and dashboard widgets; the
# market-hours liveness alarm was re-pointed to tv_rest_1m_fire_heartbeat in
# the same PR (market-hours-liveness-alarm.tf). Dated cost note in
# aws-budget.md (COST NOTE 2026-07-15).
# ---------------------------------------------------------------------------


output "silent_feed_cloudwatch_alarms" {
  description = "0 silent-feed degradation alarms remain (2026-07-06 incident hardening history; the SLO 0.80-0.95 dead-band alarm retired PR-C2 2026-07-13 with the PARKed SLO publisher; the Groww lag mirror retired 2026-07-15 with the Groww live feed; the BOUNDARY-01 catch-up-storm alarm retired 2026-07-17 with the tick aggregator — stage-3 dead-WS sweep, S2 note above; the Dhan lag mirror retired 2026-07-17 with the dead Dhan-lag publisher chain). File retained for the dated retirement notes; the retuned tick-gap alarm history stays in app-alarms.tf."
  value = [
    # boundary_catchup_storm_dhan retired 2026-07-17 (stage-3 dead-WS sweep).
    # dhan_exchange_lag_p99_high retired 2026-07-17 (dead Dhan-lag chain).
    # groww_exchange_lag_p99_high retired 2026-07-15 (Groww live feed).
  ]
}
