# =============================================================================
# Error-code log-filter alarms — the error! -> page route, RESTORED 2026-07-06
# =============================================================================
# THE GAP THIS CLOSES (zero-page incident, 2026-07-06):
#   The CloudWatch-only migration (#O1/#O2/#O3) retired the
#   Loki -> Alertmanager -> Telegram route with NO replacement, so an `error!`
#   reached only the log sinks. On 2026-07-06 the 12:00 IST REST-CANARY-01
#   probe failure produced ZERO pages. These 8 log metric filters + alarms on
#   the /tickvault/<env>/app log group (the errors.jsonl stream) restore the
#   route: error! -> errors.jsonl -> CloudWatch Logs -> filter -> tv_errcode_*
#   metric -> alarm (<=5 min) -> SNS tv-alerts -> Telegram webhook Lambda.
#
# HONEST ALARM COUNT: this file takes the REAL total from 33 -> 41 alarms
# (43 with the reconnect-storm + readiness-lambda-errors alarms landing in the
# same PR). Overage above the 10 free-tier alarms moves $2.30 -> $3.30/mo.
# The rule-file "10 alarms free tier" claims were already stale pre-PR.
#
# DIMENSIONLESS BY DESIGN: errors.jsonl events carry NO `host` field (the host
# label is added by the Prometheus scrape, not the tracing layer), and metric
# filter transformations can only EXTRACT dimensions from event JSON — they
# cannot emit constant dimensions. So the tv_errcode_* metrics and their
# alarms are dimensionless; the filter and alarm match each other exactly.
#
# NO default_value ANYWHERE: a default_value emits a datapoint for every
# NON-matching event in the group, making every metric always-billed. Sparse
# metrics (billed only in hours a code fires) + treat_missing_data=notBreaching
# is correct and near-free.
#
# ADDING A FUTURE PAGED CODE = ONE map entry below (filter + alarm generated).
# =============================================================================

locals {
  # eval/dta 3/1: identical first-page latency to 1/1, but holds ALARM across
  # <=15-min repeat gaps (2026-07-06 DH-901 shape: 2 messages per episode --
  # one ALARM + one OK -- instead of ~32 flapping pairs).
  error_code_alerts = {
    "rest-canary-01" = {
      pattern   = "{ $.code = \"REST-CANARY-01\" && $.level = \"ERROR\" }"
      period    = 300
      threshold = 1
      eval      = 3
      dta       = 1
      desc      = "REST-CANARY-01: Dhan REST health probe FAILED (09:05/12:00/15:25 IST canary). REST surface down or rejecting while the WebSocket may still look healthy (2026-07-06 12:00 IST incident class). Read status/url/body in the errors-jsonl stream. Runbook: .claude/rules/project/dhan-rest-canary-error-codes.md"
    }
    "dh-901" = {
      pattern   = "{ $.code = \"DH-901\" && $.level = \"ERROR\" }"
      period    = 300
      threshold = 1
      eval      = 3
      dta       = 1
      desc      = "DH-901: Dhan auth failing - token invalid/expired or profile checks failing. Check tv_token_remaining_seconds + SSM TOTP secret. Runbook: .claude/rules/dhan/annexure-enums.md rule 11 + wave-4-error-codes.md"
    }
    # DH-906 is a plain TERM filter, not a coded JSON filter: zero coded emit
    # sites exist in the codebase (verified 2026-07-06 - tests, one doc
    # comment, one cfg(test) counter only). At runtime the literal arrives
    # only inside Dhan's response text via OmsError free text, in an unknown
    # field at an unknown level - the delimiter-based term filter matches both
    # streams at all levels. Honest boundary: an UNLOGGED reject is invisible;
    # dormant while dry_run=true. Flagged follow-up (NOT this PR): a 3-line
    # error!(code = ErrorCode::Dh906OrderError.code_str(), ...) at the
    # OmsError classification site converts this to a coded filter.
    "dh-906" = {
      pattern   = "\"DH-906\""
      period    = 300
      threshold = 1
      eval      = 3
      dta       = 1
      desc      = "DH-906: Dhan order error - NEVER auto-retry; fix the order. NOTE: pre-armed tripwire - no coded emit site exists and dry_run=true means no live orders today; the literal arrives inside Dhan's response text via OmsError. Runbook: .claude/rules/dhan/annexure-enums.md rule 11"
    }
    "auth-gap-04" = {
      pattern   = "{ $.code = \"AUTH-GAP-04\" && $.level = \"ERROR\" }"
      period    = 300
      threshold = 1
      eval      = 3
      dta       = 1
      desc      = "AUTH-GAP-04: TOTP secret likely rotated externally - auth is DEAD until the SSM totp-secret is reconciled with dhan.co. Runbook: .claude/rules/project/wave-4-error-codes.md"
    }
    "ws-gap-07" = {
      pattern   = "{ $.code = \"WS-GAP-07\" && $.level = \"ERROR\" }"
      period    = 300
      threshold = 1
      eval      = 3
      dta       = 1
      desc      = "WS-GAP-07: live-feed frame channel CLOSED - the tick consumer died; no ticks reach the pipeline from that connection until restart. Runbook: .claude/rules/project/wave-2-error-codes.md"
    }
    # FEED-STALL-01 pages only on a STORM (Sum >= 3 per 15 min): the runbook
    # itself defines a single stall-restart as healthy self-heal; the storm is
    # the operator-action signal (persistent provider-side reject).
    "feed-stall-01" = {
      pattern   = "{ $.code = \"FEED-STALL-01\" && $.level = \"ERROR\" }"
      period    = 900
      threshold = 3
      eval      = 1
      dta       = 1
      desc      = "FEED-STALL-01 STORM: >=3 Groww sidecar stall-restarts in 15 min - the provider keeps closing the socket; a single self-heal restart never pages (per the runbook's own operator-action bound). Check credential/entitlement. Runbook: .claude/rules/project/feed-stall-watchdog-error-codes.md"
    }
    "ws-reinject-01" = {
      pattern   = "{ $.code = \"WS-REINJECT-01\" && $.level = \"ERROR\" }"
      period    = 300
      threshold = 1
      eval      = 3
      dta       = 1
      desc      = "WS-REINJECT-01: boot WAL re-injection ABORTED - consumer dead/wedged; frames stay staged in WAL replaying/ and re-replay next boot. Runbook: .claude/rules/project/ws-reinject-error-codes.md"
    }
    "proc-01" = {
      pattern   = "{ $.code = \"PROC-01\" && $.level = \"ERROR\" }"
      period    = 300
      threshold = 1
      eval      = 3
      dta       = 1
      desc      = "PROC-01: kernel OOM kill detected in this cgroup (Severity Critical). Cross-check tv_process_rss_bytes + host memory alarms. Runbook: .claude/rules/project/wave-4-error-codes.md"
    }
  }
}

resource "aws_cloudwatch_log_metric_filter" "error_code" {
  for_each       = local.error_code_alerts
  name           = "tv-${var.environment}-errcode-${each.key}"
  log_group_name = aws_cloudwatch_log_group.tv_app.name # terraform-managed group (main.tf)
  pattern        = each.value.pattern
  metric_transformation {
    name      = "tv_errcode_${replace(each.key, "-", "_")}"
    namespace = "Tickvault/Prod"
    value     = "1"
    unit      = "Count"
    # NO dimensions: errors.jsonl events carry no host field and metric filters
    # cannot emit constant dimensions. Dimensionless by design; alarm matches.
    # NO default_value: sparse metric = billed only in hours with datapoints;
    # treat_missing_data=notBreaching makes sparseness correct.
  }
}

resource "aws_cloudwatch_metric_alarm" "error_code" {
  for_each            = local.error_code_alerts
  alarm_name          = "tv-${var.environment}-errcode-${each.key}"
  alarm_description   = "${each.value.desc} (log-derived from /tickvault/${var.environment}/app; added 2026-07-06 after the zero-page incident - the error! -> Telegram route was severed by the CloudWatch-only migration)"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = each.value.eval
  datapoints_to_alarm = each.value.dta
  metric_name         = "tv_errcode_${replace(each.key, "-", "_")}"
  namespace           = local.app_namespace
  period              = each.value.period
  statistic           = "Sum"
  threshold           = each.value.threshold
  treat_missing_data  = "notBreaching"
  # deliberately NO dimensions (see filter comment)
  alarm_actions = local.app_alarm_actions
  ok_actions    = local.app_alarm_ok
}
