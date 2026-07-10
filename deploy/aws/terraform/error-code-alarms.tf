# =============================================================================
# Error-code log-filter alarms — the error! -> page route, RESTORED 2026-07-06
# =============================================================================
# THE GAP THIS CLOSES (zero-page incident, 2026-07-06):
#   The CloudWatch-only migration (#O1/#O2/#O3) retired the
#   Loki -> Alertmanager -> Telegram route with NO replacement, so an `error!`
#   reached only the log sinks. On 2026-07-06 the 12:00 IST REST-CANARY-01
#   probe failure produced ZERO pages. These 10 log metric filters + alarms
#   (8 on 2026-07-06; +AGGREGATOR-DROP-01 on 2026-07-09; +WAL-SUSPEND-01 on
#   2026-07-10) on
#   the /tickvault/<env>/app log group (the errors.jsonl stream) restore the
#   route: error! -> errors.jsonl -> CloudWatch Logs -> filter -> tv_errcode_*
#   metric -> alarm (<=5 min) -> SNS tv-alerts -> Telegram webhook Lambda.
#
# HONEST ALARM COUNT: this file takes the REAL total from 33 -> 41 alarms
# (45 with the reconnect-storm + feed-stall-restarts + readiness-lambda-errors
# + market-hours-gate-errors alarms landing in the same PR). Overage above the
# 10 free-tier alarms moves $2.30 -> $3.50/mo. The rule-file "10 alarms free
# tier" claims were already stale pre-PR. (2026-07-09: +2 more alarms —
# errcode-aggregator-drop-01 here + seal-writer-dropped in seal-drop-alarm.tf,
# ~+$0.20/mo.)
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
#
# 2026-07-09 UPDATE: +1 entry (AGGREGATOR-DROP-01) -> 9 filters + 9 alarms.
# The 2026-07-09 audit confirmed the Severity::Critical sealed-candle-drop
# code (the ONLY silent-data-loss path for sealed candles) paged nobody.
# Companion counter-side pager: seal-drop-alarm.tf.
#
# 2026-07-10 UPDATE (W2 PR#6): +1 entry (WAL-SUSPEND-01) -> 10 filters +
# 10 alarms (~+$0.10/mo). Audit follow-up row 10: a WAL-suspended QuestDB
# table silently stopped applying ILP-ACKed writes with zero signal — the
# new 60s wal_tables() probe pages it here.
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
    # rapid restart inside a 300s ANCHORED-RESET window
    # (>STALL_RESTART_STORM_MAX=5, groww_sidecar_supervisor.rs — the window
    # start resets when it elapses; round-13 correction: NOT a sliding
    # window, so a burst straddling the anchor can defer the escalation by
    # up to ~one extra 300s window). Per-restart emissions are warn!-level
    # and NEVER reach the ERROR-only errors.jsonl sink, so this filter counts
    # storm-escalation LINES, not restarts. The earlier "Sum >= 3 restarts per
    # 15 min" tuning could therefore never see 3-5 restarts/15 min (zero ERROR
    # lines) — a Rule-11 false-OK envelope. Retuned: ONE storm line pages
    # (threshold 1 per 300s; the Rust detector already debounces at >5
    # restarts/5 min, so a single self-heal restart still never pages).
    # Tripwire floor (span math, round-13 — the earlier "~50s" used 300/6
    # average-rate math): 6 restarts span 5 gaps <= the 300s window, so
    # cycles <= ~60s can escalate. The
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
      desc        = "FEED-STALL-01 STORM escalation: the Groww sidecar's own storm detector fired (>5 stall-restarts within a 5-min ANCHORED-reset window - the 6th+ rapid restart emits the only ERROR-level FEED-STALL-01 line; per-restart emissions are warn!-level and invisible to this filter). The provider keeps closing the socket at <=~60s/cycle (span math: 6 restarts span 5 gaps <= 300s; anchored-reset, not sliding - a burst straddling the anchor can defer the escalation by up to ~one extra 300s window). A single self-heal restart never pages. The >=3-restarts-per-15-min pager (all restart cadences) is tv-<env>-feed-stall-restarts (feed-stall-restart-alarm.tf). Check credential/entitlement. Runbook: .claude/rules/project/feed-stall-watchdog-error-codes.md"
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
    # AGGREGATOR-DROP-01 (added 2026-07-09 — audit finding): the ONLY
    # silent-data-loss path for a sealed candle (ring + spill + DLQ all
    # failed), Severity::Critical, previously paged NOBODY. Emit site:
    # crates/storage/src/seal_writer_loop.rs::record_cycle_observability —
    # error!(code = ErrorCode::AggregatorDrop01.code_str(), dropped = N)
    # fires once per drain cycle with a non-zero truly-dropped count, so a
    # persistent catastrophic host state repeat-emits per cycle and
    # eval-3/dta-1 holds ALARM across <=15-min gaps. ok_recovery = false:
    # a drop is a discrete PERMANENT data-loss event (the dropped seals are
    # gone from the durable chain) — the auto-OK ~15 min after the episode
    # ages out can never mean "the candles came back" (Rule-11
    # false-recovery; the PROC-01 precedent). The counter-side pager on
    # tv_seal_writer_drain_total{kind="dropped"} lives in seal-drop-alarm.tf.
    "aggregator-drop-01" = {
      pattern     = "{ $.code = \"AGGREGATOR-DROP-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-09: discrete permanent data loss - the dropped sealed candles do not come back when the episode ages out (Rule-11 false-recovery; PROC-01 precedent)
      desc        = "AGGREGATOR-DROP-01: sealed candle(s) DROPPED after ring + spill + DLQ ALL failed (Severity Critical - the only silent-data-loss path for sealed candles; by definition the host is out of memory AND out of disk AND data/dlq/ is unwritable). NO recovered/OK page: the loss is permanent - the auto-OK ~15 min later only means the episode aged out. Triage: docker/host state, df -h /data, ls -la data/spill/ data/dlq/; if the host is healthy and dirs writable, restart the app. Counter-side pager: tv-<env>-seal-writer-dropped (seal-drop-alarm.tf). Runbook: .claude/rules/project/wave-6-error-codes.md"
    }
    # WAL-SUSPEND-01 (added 2026-07-10, W2 PR#6 — audit follow-up row 10):
    # a QuestDB table's WAL apply is SUSPENDED (post disk-full / apply
    # error) — ILP keeps ACKing rows into the table's WAL while they
    # silently stop becoming visible/applied. Emit site:
    # crates/storage/src/wal_suspension_watcher.rs::emit_wal_delta —
    # error!(code = ErrorCode::WalSuspend01TableSuspended.code_str(),
    # table = ...) fires ONCE per (table, suspension episode) on the
    # rising edge of the 60s wal_tables() probe (Rule-4 edge latch; a
    # merely-DOWN QuestDB never fires it — BOOT-01/02 own that page).
    # ok_recovery = false: once-per-episode emitter (the ws-reinject-01
    # precedent) — the auto-OK ~15 min after the single datapoint ages
    # out would be a Rule-11 false recovery while the table is still
    # suspended; the real recovery signals are the falling-edge info!
    # line + tv_questdb_wal_suspended_tables returning to 0.
    "wal-suspend-01" = {
      pattern     = "{ $.code = \"WAL-SUSPEND-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-10: once-per-episode emitter - the auto-OK ~15 min later only means the datapoint aged out while the table may still be suspended (Rule-11 false-recovery; ws-reinject-01 precedent)
      desc        = "WAL-SUSPEND-01: a QuestDB table's WAL apply is SUSPENDED - ingestion keeps ACKing rows while they silently stop becoming visible/applied (silent data-visibility loss; typical cause = a disk-full episode or a WAL apply error). Operator action: read the table/error_tag/error_message fields in the errors-jsonl stream, fix the underlying cause (df -h /data, QuestDB logs), then run ALTER TABLE <table> RESUME WAL in the QuestDB console - NEVER auto-executed (resuming into a still-broken disk replays the failure). NO recovered/OK page: the code fires once per suspension episode; recovery signal = the falling-edge recovery log + tv_questdb_wal_suspended_tables returning to 0. Runbook: .claude/rules/project/wal-suspension-error-codes.md"
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
  # ok_recovery = false (rest-canary-01, ws-reinject-01, proc-01, dh-906,
  # aggregator-drop-01 [2026-07-09], wal-suspend-01 [2026-07-10] -
  # the one-shot/discrete emitters) suppresses the OK page: their auto-OK
  # ~15 min after the datapoint ages out would be a Rule-11 false
  # "recovered" message while the condition persists (see the locals
  # comment above for the per-code rationale).
  #
  # ONE-TIME apply-evening noise (round-8, accepted + pre-briefed in the PR
  # body): every NEW alarm is created in INSUFFICIENT_DATA and - with
  # treat_missing_data=notBreaching on sparse/absent metrics - transitions
  # INSUFFICIENT_DATA -> OK on its first evaluation. CloudWatch invokes
  # ok_actions on ANY transition into OK, and the telegram-webhook Lambda
  # formats every OK as a green message (it reads only NewStateValue - no
  # OldStateValue filter). Expect up to ~5 one-time green "recovered" pages
  # the apply evening (canonical count, round-14): the 4 ok_recovery=true
  # codes here (dh-901, auth-gap-04, ws-gap-07, feed-stall-01) +
  # feed-stall-restarts. Exempt: the reconnect-storm alarm via
  # actions_enabled=false, and BOTH AWS/Lambda Errors watchman alarms
  # (readiness-errors + market-hours-gate-errors) via ok_actions=[]
  # (round-14 — their auto-OK is aged-out, never a fix).
  # Creation settling, NOT recoveries. Flagged
  # follow-up (not this PR): an OldStateValue == INSUFFICIENT_DATA
  # suppression branch in the telegram-webhook Lambda - benefits every
  # future alarm PR.
  ok_actions = each.value.ok_recovery ? local.app_alarm_ok : []
}
