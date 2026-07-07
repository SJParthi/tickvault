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
# (44 with the reconnect-storm + feed-stall-restarts + readiness-lambda-errors
# alarms landing in the same PR). Overage above the 10 free-tier alarms moves
# $2.30 -> $3.40/mo. The rule-file "10 alarms free tier" claims were already
# stale pre-PR.
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
  #
  # ok_recovery (round-1 review fix, 2026-07-06; widened round-4): for
  # repeat-emitters (DH-901 every 15 min, WS-GAP-07 storms, the FEED-STALL-01
  # storm detector) the eval-3/dta-1 OK transition genuinely tracks recovery
  # (the code stopped firing) -> ok_actions ON. For ONE-SHOT / DISCRETE
  # emitters the same sparse-metric + notBreaching mechanics AUTO-transition
  # to OK ~15 min after the single datapoint ages out of the lookback -- and
  # the telegram-webhook Lambda forwards OK states as a green "recovered"
  # page -- while the underlying condition still persists (a Rule-11
  # false-recovery). ok_recovery = false suppresses that misleading OK for:
  #   - rest-canary-01: 3 probes/day; OK = "no new probe ran yet". Recovery
  #     signal = the NEXT scheduled probe staying silent (or the DH-901
  #     profile-poll alarm).
  #   - ws-reinject-01: emitted exactly ONCE per boot (wal_reinject.rs abort
  #     arm); the condition -- frames staged in WAL replaying/ with a
  #     dead/wedged consumer -- persists until the NEXT boot. OK ~15 min
  #     later cannot mean recovered.
  #   - proc-01: a discrete kernel OOM-kill event; the memory pressure that
  #     caused it is not fixed by the episode aging out.
  #   - dh-906: a discrete per-order reject; OK = aged out, never "orders
  #     working again".
  # auth-gap-04 stays ok_recovery = true with a stated ambiguity (round-4):
  # its emit site returns Err from the boot mint path, systemd Restart=always
  # re-boots and re-emits roughly every failing boot cycle (each cycle spans
  # TOTP_MAX_RETRIES x 30s windows) -- a repeat-emitter whose OK ~= "stopped
  # firing" (secret reconciled, or the unit stopped). Caveat: if systemd's
  # StartLimitBurst (8/600s) ever halts the restart loop while the secret is
  # still wrong, emissions stop and the OK would be an aged-out false
  # recovery -- borderline, kept ON with this stated residual.
  error_code_alerts = {
    "rest-canary-01" = {
      pattern     = "{ $.code = \"REST-CANARY-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false
      desc        = "REST-CANARY-01: Dhan REST health probe FAILED (09:05/12:00/15:25 IST canary). REST surface down or rejecting while the WebSocket may still look healthy (2026-07-06 12:00 IST incident class). Read status/url/body in the errors-jsonl stream. NO recovered/OK page for this alarm: the probe runs 3x/day, so the auto-OK ~15 min later only means the episode aged out - the next probe staying silent is the recovery signal. Runbook: .claude/rules/project/dhan-rest-canary-error-codes.md"
    }
    "dh-901" = {
      pattern     = "{ $.code = \"DH-901\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true
      desc        = "DH-901: Dhan auth failing - token invalid/expired or profile checks failing. Check tv_token_remaining_seconds + SSM TOTP secret. Runbook: .claude/rules/dhan/annexure-enums.md rule 11 + wave-4-error-codes.md"
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
      pattern     = "\"DH-906\""
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # round-4: discrete per-order reject - auto-OK ~15 min later means the episode aged out, never "orders working again" (Rule-11 false-recovery)
      desc        = "DH-906: Dhan order error - NEVER auto-retry; fix the order. NO recovered/OK page: a reject is a discrete event, so the auto-OK ~15 min later only means the episode aged out of the lookback. NOTE: pre-armed tripwire - no coded emit site exists and dry_run=true means no live orders today; the literal arrives inside Dhan's response text via OmsError. Runbook: .claude/rules/dhan/annexure-enums.md rule 11"
    }
    "auth-gap-04" = {
      pattern     = "{ $.code = \"AUTH-GAP-04\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true # round-4 documented ambiguity: repeat-emits per failing boot cycle under systemd Restart=always, so OK ~= stopped firing; if StartLimitBurst (8/600s) halts the loop, the OK would be aged-out - stated residual (see locals header)
      desc        = "AUTH-GAP-04: TOTP secret likely rotated externally - auth is DEAD until the SSM totp-secret is reconciled with dhan.co. CAVEAT on the recovered/OK page: it is trustworthy while the systemd restart loop keeps re-emitting; if systemd's StartLimitBurst halted the unit, the OK only means emissions stopped - verify the app is actually up before treating it as recovery. Runbook: .claude/rules/project/wave-4-error-codes.md"
    }
    "ws-gap-07" = {
      pattern     = "{ $.code = \"WS-GAP-07\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true
      desc        = "WS-GAP-07: live-feed frame channel CLOSED - the tick consumer died; no ticks reach the pipeline from that connection until restart. Runbook: .claude/rules/project/wave-2-error-codes.md"
    }
    # FEED-STALL-01 (round-3 review fix, 2026-07-06): the ONLY ERROR-level
    # FEED-STALL-01 emission is the sidecar's own STORM escalation — the 6th+
    # rapid restart inside a 300s sliding window (>STALL_RESTART_STORM_MAX=5,
    # groww_sidecar_supervisor.rs). Per-restart emissions are warn!-level and
    # NEVER reach the ERROR-only errors.jsonl sink, so this filter counts
    # storm-escalation LINES, not restarts. The earlier "Sum >= 3 restarts per
    # 15 min" tuning could therefore never see 3-5 restarts/15 min (zero ERROR
    # lines) — a Rule-11 false-OK envelope. Retuned: ONE storm line pages
    # (threshold 1 per 300s; the Rust detector already debounces at >5
    # restarts/5 min, so a single self-heal restart still never pages). The
    # ">=3 restarts per 15 min" pager — counting EVERY restart, warn! + error!
    # alike — is the separate tv-<env>-feed-stall-restarts counter alarm
    # (feed-stall-restart-alarm.tf).
    "feed-stall-01" = {
      pattern     = "{ $.code = \"FEED-STALL-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true
      desc        = "FEED-STALL-01 STORM escalation: the Groww sidecar's own storm detector fired (>5 stall-restarts within a 5-min sliding window - the 6th+ rapid restart emits the only ERROR-level FEED-STALL-01 line; per-restart emissions are warn!-level and invisible to this filter). The provider keeps closing the socket faster than ~50s/cycle. A single self-heal restart never pages. The >=3-restarts-per-15-min pager (all restart cadences) is tv-<env>-feed-stall-restarts (feed-stall-restart-alarm.tf). Check credential/entitlement. Runbook: .claude/rules/project/feed-stall-watchdog-error-codes.md"
    }
    "ws-reinject-01" = {
      pattern     = "{ $.code = \"WS-REINJECT-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # round-4: emitted exactly ONCE per boot; the staged-WAL condition persists until the NEXT boot - auto-OK would be a Rule-11 false recovery
      desc        = "WS-REINJECT-01: boot WAL re-injection ABORTED - consumer dead/wedged; frames stay staged in WAL replaying/ and re-replay next boot. NO recovered/OK page: the code fires once per boot and the condition persists until the next boot, so the auto-OK ~15 min later only means the single datapoint aged out - recovery is the NEXT boot's clean replay. Runbook: .claude/rules/project/ws-reinject-error-codes.md"
    }
    "proc-01" = {
      pattern     = "{ $.code = \"PROC-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # round-4: discrete OOM-kill event - auto-OK means the episode aged out, not that the memory pressure is resolved (Rule-11 false-recovery)
      desc        = "PROC-01: kernel OOM kill detected in this cgroup (Severity Critical). NO recovered/OK page: an OOM kill is a discrete event, so the auto-OK ~15 min later only means the episode aged out - the leak/pressure behind it is not thereby fixed; watch tv_process_rss_bytes + host memory alarms for the real recovery. Runbook: .claude/rules/project/wave-4-error-codes.md"
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
  # ok_recovery = false (rest-canary-01, ws-reinject-01, proc-01, dh-906 -
  # the one-shot/discrete emitters) suppresses the OK page: their auto-OK
  # ~15 min after the datapoint ages out would be a Rule-11 false
  # "recovered" message while the condition persists (see the locals
  # comment above for the per-code rationale).
  ok_actions = each.value.ok_recovery ? local.app_alarm_ok : []
}
