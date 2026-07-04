# =============================================================================
# Boot-heartbeat alarm — page if the app HUNG or never booted at 08:30 IST.
# =============================================================================
# Authority: daily-universe-scope-expansion-2026-05-27.md §19 ("EC2 cron
# heartbeat" — "IF tv_boot_completed is missing in the last 10 minutes THEN
# trigger Lambda -> SNS Critical: 'EC2 failed to start OR app failed to boot'").
#
# THE RESIDUAL RISK THIS CLOSES (audit + PR #1275):
#   PR #1275 set systemd TimeoutStartSec=infinity so the BY-DESIGN infinite
#   instrument-CSV-build retry (daily-universe lock §4) is never SIGTERM-killed.
#   Correct — but it removed systemd's restart safety net, AND every app alarm
#   in app-alarms.tf is treat_missing_data="notBreaching" (so they stay OK while
#   the box is intentionally stopped). Net effect: an app that HANGS or NEVER
#   BOOTS at the 08:30 IST start (wedged Docker daemon, CSV outage, OOM during
#   build) sits SILENTLY — the operator only notices a MISSING morning Telegram.
#   The existing start-watchdog (start-watchdog-lambda.tf) proves the *instance*
#   is RUNNING at 08:45 IST, but NOT that the *app* booted. This file closes that
#   exact gap.
#
# HONEST SIGNAL CHOICE — THE DEDICATED BOOT METRIC (the #1278 follow-up, now done):
#   §19 names `tv_boot_completed`. As of this PR that metric IS REAL: it is set to
#   1.0 by `crates/app/src/main.rs::emit_boot_completed()` at BOTH boot-completion
#   points (fast-boot crash-recovery + slow-boot normal), each reached ONLY after
#   every boot gate has passed — a halt uses `process::exit(...)` / `bail!`/`?`,
#   none of which reach the emit, so a wedged/halted boot NEVER publishes it. It is
#   in the CloudWatch-agent metric_declaration filter (user-data.sh.tftpl
#   emf_processor). So this alarm now pages on the IDEAL signal:
#
#     tv_boot_completed
#
#   `metrics-exporter-prometheus` re-renders a gauge's last value on every scrape,
#   so the one-shot `set(1.0)` keeps appearing while the process is alive (same
#   pattern as the boot `tv_instrument_load_*` gauges) and disappears only when the
#   process exits. If the app never finishes booting (hung CSV build, OOM, wedged
#   Docker) or dies, this gauge is NEVER published -> the metric is MISSING.
#
#   Unlike the previous PROXY (`tv_realtime_guarantee_score`, the post-boot SLO
#   loop gated on the `realtime_guarantee_score` feature flag), this signal does
#   NOT depend on any feature gate and means EXACTLY "a full boot finished" — a
#   strictly more honest boot proof.
#
#   So: treat_missing_data = "breaching" -> MISSING data PAGES. That is the
#   inverse of every other app alarm (which use notBreaching to avoid stale-
#   firing while the box is stopped) and it is INTENTIONAL: a missing boot
#   heartbeat is EXACTLY the condition we must page on. (LessThanThreshold /
#   threshold=0 / statistic=Maximum: the 1.0 gauge never satisfies <0, so
#   present=OK, missing=breaching — unchanged math, dedicated signal.)
#
# AVOIDING THE EVENING/WEEKEND FALSE-PAGE (the pager-fatigue trap the other
# alarms explicitly dodge): a "breaching on missing" alarm left always-on would
# fire every evening at 16:30 IST when the box is intentionally stopped. So this
# alarm's ACTIONS are gated to the boot window only: actions_enabled=false by
# default; a tiny boot-window Lambda turns them ON at 08:50 IST (10 min after
# the 08:40 IST soft boot deadline in §10) and OFF at 09:10 IST (before market
# open). Outside that 20-min weekday window the alarm publishes nothing, so the
# nightly/weekend stop can never page. This mirrors the inline-Lambda pattern
# already used in budget-guards.tf.
#
# DONE (the PR #1278 follow-up): the dedicated `tv_boot_completed` gauge is now
# emitted by crates/app/src/main.rs (both boot paths) and added to the CloudWatch
# metric_declaration filter (user-data.sh.tftpl). This alarm has been repointed
# from the old `tv_realtime_guarantee_score` proxy to that dedicated signal. See
# §19.
#
# Cost: 1 alarm (inside the free-tier headroom noted in app-alarms.tf) + 1 tiny
# Lambda at 2 invokes/weekday (~44/mo) — well within the Lambda free tier (₹0).
# =============================================================================

# ---------------------------------------------------------------------------
# The boot-heartbeat alarm itself — pages when tv_boot_completed is MISSING
# (app hung / never booted). Actions gated to the boot window below.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "boot_heartbeat_missing" {
  alarm_name        = "tv-${var.environment}-boot-heartbeat-missing"
  alarm_description = "App boot heartbeat ABSENT — the dedicated boot-completed signal (tv_boot_completed, set to 1 by crates/app/src/main.rs only when a full boot finishes) has not been published. The 08:30 IST start brought up the instance but the app HUNG or never finished booting (wedged Docker, CSV-build outage, OOM during build). systemd TimeoutStartSec=infinity (PR #1275) means systemd will NOT restart it — operator action needed. See daily-universe-scope-expansion-2026-05-27.md §19. Check: SSM -> the box -> 'systemctl status tickvault' + 'docker ps' + the instrument-CSV-fetch logs (INSTR-FETCH-*)."

  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 2 # two missing 60s periods = ~2 min absent before paging
  metric_name         = "tv_boot_completed"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  # INTENTIONAL inverse of every other app alarm: a MISSING heartbeat is the
  # condition we MUST page on, so missing data is BREACHING here.
  treat_missing_data = "breaching"
  dimensions         = local.app_dimensions

  # Actions OFF by default; the boot-window Lambda flips them ON 08:50-09:10 IST
  # Mon-Fri so the intentional nightly/weekend stop can never false-page.
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# Boot-window gate Lambda — enables the alarm's actions during the morning boot
# window and disables them otherwise. Pattern mirrors budget-guards.tf.
# ---------------------------------------------------------------------------
data "archive_file" "tv_boot_heartbeat_gate_zip" {
  type        = "zip"
  output_path = "${path.module}/.tv-boot-heartbeat-gate.zip"
  source {
    content  = <<-PYEOF
import os, boto3

cw = boto3.client('cloudwatch')

ALARM_NAME = os.environ['ALARM_NAME']

# mode="open"  (08:50 IST) -> enable alarm actions for the boot window.
# mode="close" (09:10 IST) -> disable them again so the nightly/weekend stop
#                             (metric goes missing intentionally) never pages.
def handler(event, context):
    mode = (event or {}).get('mode', 'close')
    if mode == 'open':
        cw.enable_alarm_actions(AlarmNames=[ALARM_NAME])
        # Reset to OK on open so a stale ALARM from a prior window does not
        # immediately re-fire on the first enabled evaluation.
        cw.set_alarm_state(
            AlarmName=ALARM_NAME,
            StateValue='OK',
            StateReason='boot-heartbeat window opened (08:50 IST)',
        )
        print(f"enabled actions for {ALARM_NAME}")
        return {'mode': mode, 'enabled': True}
    cw.disable_alarm_actions(AlarmNames=[ALARM_NAME])
    print(f"disabled actions for {ALARM_NAME}")
    return {'mode': mode, 'enabled': False}
PYEOF
    filename = "index.py"
  }
}

resource "aws_iam_role" "tv_boot_heartbeat_gate" {
  name = "tv-${var.environment}-boot-heartbeat-gate-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "tv_boot_heartbeat_gate" {
  name = "tv-${var.environment}-boot-heartbeat-gate-policy"
  role = aws_iam_role.tv_boot_heartbeat_gate.id
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

resource "aws_lambda_function" "tv_boot_heartbeat_gate" {
  function_name    = "tv-${var.environment}-boot-heartbeat-gate"
  filename         = data.archive_file.tv_boot_heartbeat_gate_zip.output_path
  source_code_hash = data.archive_file.tv_boot_heartbeat_gate_zip.output_base64sha256
  role             = aws_iam_role.tv_boot_heartbeat_gate.arn
  handler          = "index.handler"
  runtime          = "python3.12"
  timeout          = 30
  memory_size      = 128
  environment {
    variables = {
      ALARM_NAME = aws_cloudwatch_metric_alarm.boot_heartbeat_missing.alarm_name
    }
  }
}

resource "aws_cloudwatch_log_group" "tv_boot_heartbeat_gate" {
  name              = "/aws/lambda/tv-${var.environment}-boot-heartbeat-gate"
  retention_in_days = 14
}

# Open the boot-heartbeat window at 08:50 IST (03:20 UTC) Mon-Fri — 10 min after
# the 08:40 IST soft boot deadline (§10), giving the app time to publish its
# first health signal on a healthy boot.
resource "aws_cloudwatch_event_rule" "tv_boot_heartbeat_open" {
  name                = "tv-${var.environment}-boot-heartbeat-open"
  description         = "Enable boot-heartbeat alarm actions at 08:50 IST (Mon-Fri)"
  schedule_expression = "cron(20 3 ? * MON-FRI *)"
  # PAUSED 2026-07-04 per operator: local-only verification Mon Jul 6 - Wed Jul 8 2026.
  # MUST BE REVERTED for Thu Jul 9 resume — revert branch: claude/aws-resume-jul9
  # (the box is intentionally OFF, so tv_boot_completed is missing by design;
  # with the OPEN gate disabled the alarm's actions stay actions_enabled=false
  # and it can never page. The CLOSE gate below stays ENABLED as a harmless
  # safety net that re-asserts actions-disabled at 09:10 IST.)
  state = "DISABLED"
}

# Close the window at 09:10 IST (03:40 UTC) Mon-Fri — before market open so the
# steady-state realtime_guarantee_critical alarm takes over and the nightly stop
# never false-pages.
resource "aws_cloudwatch_event_rule" "tv_boot_heartbeat_close" {
  name                = "tv-${var.environment}-boot-heartbeat-close"
  description         = "Disable boot-heartbeat alarm actions at 09:10 IST (Mon-Fri)"
  schedule_expression = "cron(40 3 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_target" "tv_boot_heartbeat_open" {
  rule      = aws_cloudwatch_event_rule.tv_boot_heartbeat_open.name
  target_id = "tv-boot-heartbeat-open"
  arn       = aws_lambda_function.tv_boot_heartbeat_gate.arn
  input     = jsonencode({ mode = "open" })
}

resource "aws_cloudwatch_event_target" "tv_boot_heartbeat_close" {
  rule      = aws_cloudwatch_event_rule.tv_boot_heartbeat_close.name
  target_id = "tv-boot-heartbeat-close"
  arn       = aws_lambda_function.tv_boot_heartbeat_gate.arn
  input     = jsonencode({ mode = "close" })
}

resource "aws_lambda_permission" "tv_boot_heartbeat_open" {
  statement_id  = "AllowExecutionFromEventBridgeOpen"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.tv_boot_heartbeat_gate.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.tv_boot_heartbeat_open.arn
}

resource "aws_lambda_permission" "tv_boot_heartbeat_close" {
  statement_id  = "AllowExecutionFromEventBridgeClose"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.tv_boot_heartbeat_gate.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.tv_boot_heartbeat_close.arn
}

output "boot_heartbeat_alarm_name" {
  description = "Boot-heartbeat alarm (pages on a hung/never-booted app in the 08:50-09:10 IST window). Signal: the dedicated tv_boot_completed gauge MISSING (treat_missing_data=breaching) — emitted by crates/app/src/main.rs on a completed boot, in the CW-agent filter (daily-universe-scope-expansion-2026-05-27.md §19). Repointed off the tv_realtime_guarantee_score proxy (PR #1278 follow-up)."
  value       = aws_cloudwatch_metric_alarm.boot_heartbeat_missing.alarm_name
}
