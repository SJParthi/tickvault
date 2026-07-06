# =============================================================================
# Market-hours liveness alarm — page if the app is WEDGED or CRASH-LOOPING
# during trading hours (interaction-audit HIGH — the post-09:10 no-page gap).
# =============================================================================
# Authority: operator-charter-forever.md §C "100% monitoring" + §F
# "Severity::Critical → Telegram"; daily-universe-scope-expansion-2026-05-27.md
# §10 (08:30 IST start / 09:15 market open).
#
# THE GAP THIS CLOSES (from the interaction audit):
#   After 09:10 IST the app has NO liveness safety net:
#     (a) systemd: PR #1275 set TimeoutStartSec=infinity + Restart=always. Until
#         the StartLimit added alongside this PR, a wedged/crash-looping app got
#         NO hard failure. StartLimit now makes a *crash-loop* observable (unit →
#         `failed`, metrics go MISSING) — but a wedged/HUNG app (alive PID, no
#         work) still publishes nothing new and needs an alarm to page.
#     (b) The ONLY "breaching on missing" app alarm is boot-heartbeat, and its
#         actions are gated to 08:50–09:10 IST (boot-heartbeat-alarm.tf). EVERY
#         other alarm in app-alarms.tf is treat_missing_data="notBreaching" (so
#         they never stale-fire while the box is intentionally stopped) — which
#         means a post-09:10 wedge/crash-loop NEVER pages: the metric just goes
#         missing and every notBreaching alarm stays silently OK.
#   Net: between 09:10 IST and the 16:30 IST stop, an app that hangs / OOMs /
#   crash-loops sits SILENTLY. This alarm is the market-hours liveness page.
#
# HONEST SIGNAL CHOICE — tv_realtime_guarantee_score (VERIFIED published):
#   The post-boot SLO loop (crates/app/src/main.rs, the Wave 3-D composite
#   real-time guarantee score) sets `tv_realtime_guarantee_score` UNCONDITIONALLY
#   on EVERY 10s tick while it runs (main.rs:6854 `metrics::gauge!(
#   "tv_realtime_guarantee_score").set(outcome.score())`). Proof it reaches
#   CloudWatch: it is in the CW-agent metric_declaration label_matcher +
#   metric_selectors allowlist (user-data.sh.tftpl) and is already charted on the
#   operator dashboard + alarmed by realtime_guarantee_critical (app-alarms.tf
#   #11). metrics-exporter-prometheus re-renders a gauge's last value on every
#   scrape, so a live app keeps re-publishing it; a WEDGED/DEAD/crash-looped app
#   stops the 10s loop → the gauge goes MISSING.
#
#   One honest caveat: the score is gated on config.features.realtime_guarantee_
#   score, which is `true` in config/base.toml (line 321) and NOT overridden in
#   config/production.toml → enabled in prod today. That flag is exactly why the
#   boot-heartbeat alarm moved OFF this metric to the dedicated tv_boot_completed
#   gauge for BOOT proof. For a MARKET-HOURS LIVENESS page the score is the right
#   signal: it is the SLO loop's own heartbeat and is published every 10s during
#   the whole session. If the flag is ever disabled, this alarm would false-page
#   in the market-hours window — a follow-up could point it at an unconditional
#   tick-freshness / ticks-processed metric once one is added to the CW filter.
#   That is the honest limit; the alarm still uses a REAL, currently-published
#   metric (no phantom), matching the boot-heartbeat worker's discipline.
#
#   So: treat_missing_data = "breaching" → MISSING data PAGES during the gated
#   window. Same inverse-of-other-alarms rationale as boot-heartbeat.
#
# AVOIDING THE OVERNIGHT/WEEKEND FALSE-PAGE:
#   The box is STOPPED outside 08:30–16:30 IST, so the metric is ALSO absent then
#   — a "breaching on missing" alarm left always-on would fire every night. So,
#   mirroring boot-heartbeat-alarm.tf, this alarm's ACTIONS are gated to MARKET
#   HOURS only: actions_enabled=false by default; a tiny window Lambda turns them
#   ON at ~09:20 IST (5 min after the 09:15 open, giving the SLO loop time to
#   publish its first score) and OFF at ~15:35 IST (5 min after the 15:30 close).
#   Outside that weekday window the alarm publishes nothing, so the nightly /
#   weekend stop can never page. Reuses the boot-heartbeat inline-Lambda pattern,
#   shifted to the market-hours window.
#
# Cost: 1 alarm (score metric already scraped, so 0 NEW custom metrics) + 1 tiny
# Lambda at 2 invokes/weekday (~44/mo) — well within the Lambda free tier (₹0).
# Alarm count 23 → 24 (app-alarms.tf output note); overage above the 10 free-tier
# alarms is unchanged in kind (~$0.10/mo for this one) — inside the budget cap.
# =============================================================================

# ---------------------------------------------------------------------------
# The market-hours liveness alarm — pages when tv_realtime_guarantee_score is
# MISSING (app wedged / crash-looped / dead). Actions gated to market hours.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "market_hours_liveness_missing" {
  alarm_name        = "tv-${var.environment}-market-hours-liveness-missing"
  alarm_description = "App liveness signal ABSENT during MARKET HOURS — the SLO loop's tv_realtime_guarantee_score (set every 10s by crates/app/src/main.rs while the app runs) has not been published for ~5 min. The app is WEDGED, CRASH-LOOPING, OOM-killed, or DEAD between 09:15–15:30 IST. systemd StartLimit makes a crash-loop go to `failed` (no restart) — either way the app is not working and needs operator action. Check: SSM → the box → 'systemctl status tickvault' + 'systemctl is-failed tickvault' + 'docker ps' + tail /opt/tickvault/logs/errors.jsonl. See operator-charter-forever.md §C and SLO-02 runbook."

  # LessThanThreshold / threshold=0 / statistic=Maximum: the score is in [0,1],
  # so a present value never satisfies <0 (present = OK); a MISSING metric is
  # forced BREACHING below. Same math as boot-heartbeat-alarm.tf.
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 5 # five missing 60s periods = ~5 min absent before paging
  metric_name         = "tv_realtime_guarantee_score"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  # INTENTIONAL inverse of every other app alarm: during market hours a MISSING
  # liveness signal is the condition we MUST page on, so missing data BREACHES.
  treat_missing_data = "breaching"
  dimensions         = local.app_dimensions

  # Actions OFF by default; the window Lambda flips them ON 09:20–15:35 IST Mon-Fri
  # so the intentional nightly/weekend stop (metric absent) can never false-page.
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# Market-hours window gate Lambda — enables the gated alarms' actions during
# trading hours and disables them otherwise. Pattern mirrors
# boot-heartbeat-alarm.tf.
#
# 2026-07-03 (5 AM false-SOS fix): generalized from a single ALARM_NAME to a
# comma-separated ALARM_NAMES list so the SAME window Lambda also gates the
# two value-based off-hours false-pagers in app-alarms.tf:
#   - tv-<env>-realtime-guarantee-critical (score legitimately 0 off-hours —
#     VERIFIED SOS page at 05:40 IST 2026-07-03 on a healthy pre-market box)
#   - tv-<env>-aggregator-no-seals (zero seals off-hours is by design)
# ---------------------------------------------------------------------------
data "archive_file" "tv_market_hours_liveness_gate_zip" {
  type        = "zip"
  output_path = "${path.module}/.tv-market-hours-liveness-gate.zip"
  source {
    content  = <<-PYEOF
import os, boto3

cw = boto3.client('cloudwatch')

ALARM_NAMES = [n.strip() for n in os.environ['ALARM_NAMES'].split(',') if n.strip()]

# mode="open"  (09:20 IST) -> enable alarm actions for the market-hours window.
# mode="close" (15:35 IST) -> disable them again so the intentional off-hours
#                             state (metric missing on the nightly/weekend
#                             stop, or legitimately-zero score/seals while the
#                             box idles outside market hours) never pages.
def handler(event, context):
    mode = (event or {}).get('mode', 'close')
    if mode == 'open':
        cw.enable_alarm_actions(AlarmNames=ALARM_NAMES)
        # Reset to OK on open so a stale ALARM from a prior window does not
        # immediately re-fire on the first enabled evaluation.
        for name in ALARM_NAMES:
            cw.set_alarm_state(
                AlarmName=name,
                StateValue='OK',
                StateReason='market-hours window opened (09:20 IST)',
            )
        print(f"enabled actions for {ALARM_NAMES}")
        return {'mode': mode, 'enabled': True}
    cw.disable_alarm_actions(AlarmNames=ALARM_NAMES)
    print(f"disabled actions for {ALARM_NAMES}")
    return {'mode': mode, 'enabled': False}
PYEOF
    filename = "index.py"
  }
}

resource "aws_iam_role" "tv_market_hours_liveness_gate" {
  name = "tv-${var.environment}-market-hours-liveness-gate-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "tv_market_hours_liveness_gate" {
  name = "tv-${var.environment}-market-hours-liveness-gate-policy"
  role = aws_iam_role.tv_market_hours_liveness_gate.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # EnableAlarmActions / DisableAlarmActions / SetAlarmState have no
        # resource-level ARN scoping in IAM, so "*" is required; the Lambda
        # only ever names the single ALARM_NAME it is given.
        Effect   = "Allow"
        Action   = ["cloudwatch:EnableAlarmActions", "cloudwatch:DisableAlarmActions", "cloudwatch:SetAlarmState"]
        Resource = "*"
      },
      {
        Effect   = "Allow"
        Action   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "arn:aws:logs:${var.aws_region}:*:*"
      },
    ]
  })
}

resource "aws_lambda_function" "tv_market_hours_liveness_gate" {
  function_name    = "tv-${var.environment}-market-hours-liveness-gate"
  filename         = data.archive_file.tv_market_hours_liveness_gate_zip.output_path
  source_code_hash = data.archive_file.tv_market_hours_liveness_gate_zip.output_base64sha256
  role             = aws_iam_role.tv_market_hours_liveness_gate.arn
  handler          = "index.handler"
  runtime          = "python3.12"
  timeout          = 30
  memory_size      = 128
  environment {
    variables = {
      # All market-hours-gated alarms (comma-separated). 2026-07-03: the two
      # app-alarms.tf value-based off-hours false-pagers joined the liveness
      # alarm under the same 09:20-15:35 IST Mon-Fri window.
      ALARM_NAMES = join(",", [
        aws_cloudwatch_metric_alarm.market_hours_liveness_missing.alarm_name,
        aws_cloudwatch_metric_alarm.realtime_guarantee_critical.alarm_name,
        aws_cloudwatch_metric_alarm.aggregator_no_seals.alarm_name,
        aws_cloudwatch_metric_alarm.order_update_reconnect_storm.alarm_name, # 2026-07-06 flapper alarm
      ])
    }
  }
}

resource "aws_cloudwatch_log_group" "tv_market_hours_liveness_gate" {
  name              = "/aws/lambda/tv-${var.environment}-market-hours-liveness-gate"
  retention_in_days = 14
}

# Open the liveness window at 09:20 IST (03:50 UTC) Mon-Fri — 5 min after the
# 09:15 IST market open, giving the post-boot SLO loop time to publish its first
# tv_realtime_guarantee_score sample on a healthy session.
resource "aws_cloudwatch_event_rule" "tv_market_hours_liveness_open" {
  name                = "tv-${var.environment}-market-hours-liveness-open"
  description         = "Enable market-hours liveness alarm actions at 09:20 IST (Mon-Fri)"
  schedule_expression = "cron(50 3 ? * MON-FRI *)"
  # RESUME 2026-07-06 per operator GO (moved up from Thu Jul 9): explicit state
  # so terraform re-enables this rule. The Jul 4 pause set state = "DISABLED";
  # the #1404 revert REMOVED the attribute, and with `state` absent the AWS
  # provider stops managing rule state - the rule stayed DISABLED on AWS.
  state = "ENABLED"
}

# Close the window at 15:35 IST (10:05 UTC) Mon-Fri — 5 min after the 15:30 IST
# market close, so post-close idle (and the 16:30 IST nightly stop) never pages.
resource "aws_cloudwatch_event_rule" "tv_market_hours_liveness_close" {
  name                = "tv-${var.environment}-market-hours-liveness-close"
  description         = "Disable market-hours liveness alarm actions at 15:35 IST (Mon-Fri)"
  schedule_expression = "cron(5 10 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_target" "tv_market_hours_liveness_open" {
  rule      = aws_cloudwatch_event_rule.tv_market_hours_liveness_open.name
  target_id = "tv-market-hours-liveness-open"
  arn       = aws_lambda_function.tv_market_hours_liveness_gate.arn
  input     = jsonencode({ mode = "open" })
}

resource "aws_cloudwatch_event_target" "tv_market_hours_liveness_close" {
  rule      = aws_cloudwatch_event_rule.tv_market_hours_liveness_close.name
  target_id = "tv-market-hours-liveness-close"
  arn       = aws_lambda_function.tv_market_hours_liveness_gate.arn
  input     = jsonencode({ mode = "close" })
}

resource "aws_lambda_permission" "tv_market_hours_liveness_open" {
  statement_id  = "AllowExecutionFromEventBridgeOpen"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.tv_market_hours_liveness_gate.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.tv_market_hours_liveness_open.arn
}

resource "aws_lambda_permission" "tv_market_hours_liveness_close" {
  statement_id  = "AllowExecutionFromEventBridgeClose"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.tv_market_hours_liveness_gate.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.tv_market_hours_liveness_close.arn
}

output "market_hours_liveness_alarm_name" {
  description = "Market-hours liveness alarm (pages on a wedged/crash-looped/dead app in the 09:20-15:35 IST window). Signal: the tv_realtime_guarantee_score gauge MISSING (treat_missing_data=breaching) — emitted every 10s by the SLO loop in crates/app/src/main.rs, in the CW-agent filter (user-data.sh.tftpl). Closes the post-09:10 IST no-page gap the boot-heartbeat window leaves open. The same gate Lambda also window-gates realtime-guarantee-critical + aggregator-no-seals (2026-07-03 5 AM false-SOS fix)."
  value       = aws_cloudwatch_metric_alarm.market_hours_liveness_missing.alarm_name
}
