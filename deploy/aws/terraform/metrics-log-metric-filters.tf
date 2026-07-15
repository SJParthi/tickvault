# =============================================================================
# Log-metric-filter fallback publishers for the two liveness alarm metrics.
# =============================================================================
# WHY (B1 root-cause, 2026-07-02): the CloudWatch agent's prometheus
# emf_processor.metric_declaration used `source_labels: ["__name__"]` —
# `__name__` is NOT a label on the scraped series (labels: host / instance /
# job / prom_metric_type), so the declaration matched ZERO metrics, no `_aws`
# EMF envelope was ever stamped, and the `Tickvault/Prod` namespace sat empty
# for ~40 days while the boot-heartbeat + market-hours-liveness alarms rang
# blind on missing data. The EMF fix (source_labels: ["host"], label_matcher
# "^tickvault-prod$" in user-data.sh.tftpl + deploy/aws/cloudwatch-agent.json)
# is validate-on-boot — it cannot be proven correct until the box runs it.
#
# THESE FILTERS ARE THE BELT-AND-SUSPENDERS: the agent ALREADY ships every
# scraped sample as a plain-JSON event to log group /tickvault/<env>/metrics
# (verified live 2026-07-02: `{"host":"tickvault-prod","tv_boot_completed":1,…}`),
# independent of the EMF envelope. A log METRIC FILTER extracts the value from
# those existing events and publishes it into Tickvault/Prod with the EXACT
# name + `host` dimension the two alarms watch (app-alarms.tf locals:
# namespace "Tickvault/Prod", dimensions { host = "tickvault-prod" }).
# So even if the EMF theory is imperfect, the two liveness alarms go green.
#
# DIMENSION MATCH (load-bearing): the alarms key on dimension host=tickvault-prod.
# Metric filters CAN emit dimensions via JSON field extraction — `host = "$.host"`
# picks up the literal "tickvault-prod" the prometheus scrape config stamps on
# every event. Without this block the filter would publish a dimensionless
# metric the alarms never see.
#
# NO default value on purpose: when the app is dead/stopped the metric must go
# MISSING (treat_missing_data=breaching is the alarms' entire detection model).
#
# The log group is created by the CloudWatch agent on the box (NOT terraform-
# managed) — it has existed with events since 2026-05; referencing it by name
# is safe.
#
# Cost: 2 custom metrics (~$0.60/mo) — inside the budget envelope; can be
# deleted once the EMF path is verified live (post-boot verification per the
# B1 Q4 command list).
# =============================================================================

resource "aws_cloudwatch_log_metric_filter" "tv_boot_completed_fallback" {
  name           = "tv-${var.environment}-boot-completed-fallback"
  log_group_name = "/tickvault/${var.environment}/metrics"

  # Match any event carrying the tv_boot_completed field (JSON filter syntax).
  pattern = "{ $.tv_boot_completed = * }"

  metric_transformation {
    name      = "tv_boot_completed"
    namespace = "Tickvault/Prod"
    value     = "$.tv_boot_completed"
    dimensions = {
      host = "$.host"
    }
    # Deliberately no default value — missing data must stay missing (alarm breaches on it).
  }
}

# RETIRED (PR-C2, 2026-07-13): the tv_realtime_guarantee_score fallback
# filter — the SLO publisher was deleted per the operator PARK ruling
# (wave-3-d-error-codes.md banner); no event carries the field anymore and
# both score alarms were retired with it.
