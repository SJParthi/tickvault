# =============================================================================
# Sealed-candle DROP counter pager (AGGREGATOR-DROP-01 companion) — 2026-07-09
# =============================================================================
# THE GAP THIS CLOSES (2026-07-09 audit, PR #2 of the candle-drop paging
# sequence): tv_seal_writer_drain_total{kind="dropped"} — incremented by
# crates/storage/src/seal_writer_loop.rs::record_cycle_observability exactly
# when one or more sealed candles were TRULY dropped (ring + spill + DLQ all
# failed; the only silent-data-loss path for sealed candles, Severity
# Critical) — was NOT exported to CloudWatch at all: it is not in the
# ratcheted EMF metric_selectors allowlist and had no /metrics log-filter
# fallback, so no metric-side alarm existed. The error!-side pager is the
# errcode-aggregator-drop-01 log-filter alarm (error-code-alarms.tf, same
# PR); THIS alarm is the counter-side redundancy so a drop pages even if the
# errors.jsonl shipping leg is degraded (the 2026-07-06 collect_list
# incident class).
#
# ROUTE (the feed-stall-restart-alarm.tf house pattern, verbatim): the
# counter is NOT in the CW agent's EMF metric_selectors allowlist — and that
# list is ratcheted (cloudwatch_app_alarms_wiring.rs). Every 60s scrape
# still ships each series as a plain-JSON event (with $.host, and the
# Prometheus `kind` label as a same-named JSON field) into
# /tickvault/<env>/metrics; a log metric filter extracts the per-scrape
# delta. NO allowlist edit, NO Rust rename.
#
# KIND-SLICE + DERIVED NAME: tv_seal_writer_drain_total carries a `kind`
# label with 6 values (submitted / flushed_rows / flush_failed /
# rescued_spill / rescued_dlq / dropped); only `dropped` is the data-loss
# signal. The filter pattern therefore ALSO matches $.kind = "dropped" so
# the busy submitted/flushed series can never inflate the Sum. The extracted
# metric gets a DISTINCT derived name (tv_seal_writer_drain_dropped_total,
# NOT the raw counter name): a kind-restricted slice must not reuse the raw
# identity — a future unfiltered extraction (or EMF allowlisting) into the
# same name would double-count under this alarm's Sum statistic (the
# feed-stall "Future EMF-allowlisting" clause, applied at naming time
# instead of as a residual).
#
# COUNTER SHAPE: identical model + identical honest residual as
# feed-stall-restart-alarm.tf / the retired order-update-reconnect-storm alarm — the
# CW agent's prometheus pipeline converts COUNTER samples to PER-SCRAPE
# DELTAS, so `Sum` over the window = seals dropped in the window. Not
# live-verified from this sandbox; if the field ever proved CUMULATIVE, Sum
# overcounts and pages too eagerly (fail-loud, never a silent miss) and this
# alarm must be reworked to DIFF(Maximum).
#
# FIRST-SAMPLE BASELINE (the round-5 feed-stall lesson, applied at design
# time): the delta pipeline DROPS each counter series' first observed sample
# as its baseline. kind="dropped" increments ONLY on a real drop, so without
# pre-registration the series would be BORN at the first drop (value N) and
# the dropped baseline sample would BE the drop — a single-episode drop (the
# dominant shape: one catastrophic cycle, then restart/recovery) would
# produce ZERO datapoints and this alarm would be dead on arrival. The
# series is therefore PRE-REGISTERED at 0 in main.rs immediately AFTER the
# metrics recorder installs (boot Step 2, next to the
# tv_feed_sidecar_stall_restart_total registration — ratcheted by
# crates/app/tests/seal_drop_paging_wiring_guard.rs, a source-order scan of
# main.rs), so the dropped first sample is the harmless 0 baseline and the
# series is DENSE while the app runs (a 0-delta event per 60s scrape —
# harmless to Sum; the metric bills during uptime hours like the feed-stall
# counter). Honest residual: a drop inside the pre-install boot window
# (seal-writer spawn -> Step 2) would increment a no-op handle and be
# uncounted here — physically implausible (a drop needs ring + spill + DLQ
# to ALL fail on live seal traffic inside the boot prefix), and the errcode
# alarm still sees its ERROR line.
#
# TUNING: Sum >= 1 per ONE aligned 300s window — a SINGLE truly-dropped seal
# is Critical-class permanent data loss; there is no benign rate to
# tolerate. eval 1 / dta 1: no flap concern (0-deltas do not breach), and a
# repeat-dropping host keeps the alarm in ALARM. ok_actions = []: the loss
# is permanent — an auto-OK when deltas return to 0 can never mean "the
# candles came back" (Rule-11 false-recovery; same rationale as the
# ok_recovery=false errcode entry).
#
# ALWAYS-ARMED (no market-hours gate): sealed-candle drops are never
# routine at ANY hour — the IST-midnight force-seal burst (~99K seals) is
# exactly the load shape the ring/spill/DLQ chain absorbs, and a drop there
# is equally permanent. Dense 0-deltas keep the metric non-breaching
# off-hours, so there is no off-hours churn to suppress.
#
# Future EMF-allowlisting: if tv_seal_writer_drain_total is ever added to
# the EMF metric_selectors allowlist, this filter's DERIVED metric name
# keeps the identities separate (no double-count) — but re-point this alarm
# at the EMF-published metric and remove the filter in the same PR to avoid
# paying for both series.
# =============================================================================

resource "aws_cloudwatch_log_metric_filter" "seal_writer_dropped_fallback" {
  name = "tv-${var.environment}-seal-writer-dropped-fallback"
  # Agent-created group, referenced by name (house precedent:
  # metrics-log-metric-filters.tf; adopted into terraform by log-retention.tf).
  log_group_name = "/tickvault/${var.environment}/metrics"
  pattern        = "{ $.tv_seal_writer_drain_total = * && $.kind = \"dropped\" }"
  metric_transformation {
    name      = "tv_seal_writer_drain_dropped_total" # DERIVED kind-slice name — see header
    namespace = "Tickvault/Prod"
    value     = "$.tv_seal_writer_drain_total"
    dimensions = {
      host = "$.host" # /metrics events DO carry host (prometheus scrape label)
    }
    # Deliberately no default_value (that knob emits datapoints for
    # NON-matching events — never wanted). The series is DENSE while the
    # app runs (0-delta per 60s scrape) thanks to the main.rs boot-time
    # pre-registration — see FIRST-SAMPLE BASELINE header.
  }
}

resource "aws_cloudwatch_metric_alarm" "seal_writer_dropped" {
  alarm_name          = "tv-${var.environment}-seal-writer-dropped"
  alarm_description   = "AGGREGATOR-DROP-01 counter pager: one or more SEALED CANDLES were TRULY DROPPED (ring + spill + DLQ all failed - the only silent-data-loss path for sealed candles, Severity Critical; by definition the host is out of memory AND out of disk AND data/dlq/ is unwritable). Sum of the agent's per-scrape deltas of tv_seal_writer_drain_total kind=dropped, pre-registered at 0 at boot right after the metrics recorder installs so the dropped first delta sample is the 0 baseline, not drop #1 - this pager sees the session's FIRST drop. NO recovered/OK page: the loss is permanent. Redundant with the errcode-aggregator-drop-01 log-filter alarm (error-code-alarms.tf) so a drop pages even if one shipping leg is degraded. Triage: docker/host state, df -h /data, ls -la data/spill/ data/dlq/. Runbook: .claude/rules/project/wave-6-error-codes.md"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_seal_writer_drain_dropped_total"
  namespace           = local.app_namespace
  # 300s aligned window: Sum of per-scrape deltas = seals dropped in the
  # window; a single drop is Critical-class, so threshold 1 / eval 1.
  period             = 300
  statistic          = "Sum"
  dimensions         = local.app_dimensions # { host = "tickvault-prod" }
  treat_missing_data = "notBreaching"
  # Always-armed (no market-hours gate): a sealed-candle drop is permanent
  # data loss at any hour, incl. the IST-midnight force-seal burst — see
  # ALWAYS-ARMED header block.
  alarm_actions = local.app_alarm_actions
  # NO ok_actions: the loss is permanent — deltas returning to 0 can never
  # mean the dropped candles came back (Rule-11 false-recovery; matches the
  # errcode entry's ok_recovery = false).
  ok_actions = []
}
