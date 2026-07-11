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
#         actions are gated to 08:50–09:20 IST (boot-heartbeat-alarm.tf;
#         2026-07-09 — the close moved 09:10→09:20 so the boot window hands
#         over to THIS window with no [09:10, 09:20) seam over the open). EVERY
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
#   WEEKDAY NSE HOLIDAYS (2026-07-07 review fix): the open cron is holiday-blind
#   (plain MON-FRI), but holiday-gate.sh SELF-STOPS the box on a definitive
#   NSE-holiday verdict at boot (~08:32 IST) — so a blind 09:20 enable + OK
#   reset would drive every breaching-on-missing member (this alarm +
#   app-log-ingestion-silent) OK→ALARM against an intentionally-stopped box:
#   a false page every weekday holiday. The gate Lambda's open mode therefore
#   verifies the tv-app instance is up (ec2:DescribeInstances) before enabling,
#   and FAILS OPEN on any EC2 API error so a real trading day never loses the
#   page. Ratchet: crates/app/tests/cloudwatch_agent_glob_guard.rs
#   (test_gate_lambda_open_is_holiday_safe).
#
#   ROUND-3 HARDENING (2026-07-07): the single 09:20 instance-state sample
#   alone was RACY — the box did NOT stay stopped on holidays. Two
#   holiday-blind self-healers kept restarting it all day (start-watchdog
#   mode=check @ 08:45 IST self-start; aws-autopilot start-instances every
#   15 min inside its 08:30-16:30 IST up-window, incl. the 03:45 UTC ≈ 09:15
#   IST slot + GH cron jitter), and holiday-gate.sh re-stopped it ~2-3 min
#   after each boot — 1-3 min up-bursts that can bracket the 09:20 sample and
#   restore the false page. Fix: holiday-gate.sh now stamps today's IST date
#   into the /tickvault/<env>/holiday-stop-date SSM param BEFORE the stop;
#   (a) BOTH restarters consult it and skip the self-start (the war ends at
#   the source — the box now genuinely stays stopped), and (b) this gate
#   Lambda's open mode checks the marker FIRST — marker == today is
#   authoritative for "intentionally stopped today" and cannot be raced by a
#   transiently-up box, making the instance-state sample a second line for
#   the marker-less manual-stop case. FAIL-OPEN on any SSM error (missing/
#   stale marker = trading day). Honest residual: if the marker put itself
#   fails AND a restarter races the 09:20 sample, the false page can still
#   occur — bounded to that double-failure, vs. every raced holiday before.
#   Ratchet: test_gate_lambda_open_checks_holiday_marker_first +
#   test_holiday_stop_marker_chain_is_wired (cloudwatch_agent_glob_guard.rs).
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
from datetime import datetime, timedelta, timezone

cw = boto3.client('cloudwatch')
ec2 = boto3.client('ec2')
ssm = boto3.client('ssm')

ALARM_NAMES = [n.strip() for n in os.environ['ALARM_NAMES'].split(',') if n.strip()]
INSTANCE_ID = os.environ['EC2_INSTANCE_ID']
HOLIDAY_STOP_PARAM = os.environ['HOLIDAY_STOP_PARAM']
IST = timedelta(hours=5, minutes=30)

# mode="open"  (09:20 IST) -> enable alarm actions for the market-hours window,
#                             but ONLY if the tv-app box is actually up.
# mode="close" (15:35 IST) -> disable them again so the intentional off-hours
#                             state (metric missing on the nightly/weekend
#                             stop, or legitimately-zero score/seals while the
#                             box idles outside market hours) never pages.
#
# WEEKDAY-NSE-HOLIDAY SAFETY (2026-07-07 review fix): the open cron is a plain
# MON-FRI schedule with no NSE-holiday awareness, and on weekday NSE holidays
# the box SELF-STOPS at boot (deploy/aws/holiday-gate.sh, ~08:32 IST). Enabling
# the breaching-on-missing members of ALARM_NAMES (market-hours-liveness,
# app-log-ingestion-silent) against an intentionally-stopped box would
# false-page every weekday holiday (~09:25 / ~09:35 IST). So "open" first asks
# EC2 whether the instance is up; a not-up box (holiday self-stop or operator
# manual stop) keeps its actions DISABLED and skips the OK reset.
# FAIL-OPEN: any DescribeInstances error enables as before — a real trading
# day must never lose the liveness page (mirror of holiday-gate.sh's
# fail-open philosophy, pointed the other way).
def holiday_stop_is_today():
    # ROUND-3 hardening (2026-07-07): deploy/aws/holiday-gate.sh stamps
    # today's IST date into HOLIDAY_STOP_PARAM right BEFORE it self-stops the
    # box on a weekday NSE holiday. Marker == today is AUTHORITATIVE for
    # "intentionally stopped today" — unlike the single instance-state sample
    # below, it cannot be raced by a holiday-blind restarter briefly bringing
    # the box up around 09:20 IST (the restart-war window). Stale markers
    # (a previous holiday's date) never match today. FAIL-OPEN: any SSM error
    # / missing param = not a holiday — a real trading day must never lose
    # the liveness page.
    try:
        raw = (ssm.get_parameter(Name=HOLIDAY_STOP_PARAM)['Parameter']['Value'] or '').strip()
        today_ist = (datetime.now(timezone.utc) + IST).strftime('%Y-%m-%d')
        return raw == today_ist
    except Exception as e:
        print(f"holiday-stop marker unavailable ({e}) -- fail-open, not a holiday")
        return False

def instance_is_up():
    try:
        r = ec2.describe_instances(InstanceIds=[INSTANCE_ID])
        state = r['Reservations'][0]['Instances'][0]['State']['Name']
        # 'pending' counts as up: a late trading-day start must still arm the
        # window (the OK reset + 5-15 min evaluation absorb the boot).
        return state in ('running', 'pending'), state
    except Exception as e:
        print(f"describe_instances failed ({e}) -- fail-open, treating as up")
        return True, 'unknown'

def handler(event, context):
    mode = (event or {}).get('mode', 'close')
    if mode == 'open':
        # Marker check FIRST — race-proof (round 3); instance state second —
        # covers the marker-less manual-stop case (round 1).
        if holiday_stop_is_today():
            print(f"holiday-stop marker == today (NSE holiday self-stop); "
                  f"leaving actions disabled for {ALARM_NAMES}")
            return {'mode': mode, 'enabled': False, 'holiday_stop': True}
        up, state = instance_is_up()
        if not up:
            print(f"instance {INSTANCE_ID} state={state} -- intentional stop "
                  f"(NSE holiday self-stop / manual); leaving actions disabled "
                  f"for {ALARM_NAMES}")
            return {'mode': mode, 'enabled': False, 'instance_state': state}
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
        # Weekday-NSE-holiday safety (2026-07-07): the open path checks the
        # tv-app instance state before enabling the breaching-on-missing
        # alarms (holiday-gate.sh self-stops the box on holidays, so a blind
        # MON-FRI enable would false-page). DescribeInstances has no
        # resource-level scoping in IAM (AWS limitation), so "*" is required;
        # the Lambda only ever reads EC2_INSTANCE_ID. Same pattern as
        # start-watchdog-lambda.tf.
        Effect   = "Allow"
        Action   = ["ec2:DescribeInstances"]
        Resource = "*"
      },
      {
        # Round-3 holiday-race hardening (2026-07-07): the open path reads
        # the /tickvault/<env>/holiday-stop-date marker holiday-gate.sh
        # stamps before its self-stop — marker == today is race-proof
        # (the single instance-state sample can be bracketed by a
        # restart-war up-burst). Read-only, scoped to that one parameter.
        Effect   = "Allow"
        Action   = ["ssm:GetParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/holiday-stop-date"
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
      # alarm under the same 09:20-15:35 IST Mon-Fri window. 2026-07-07: the
      # app-log-ingestion-silent alarm (log-retention.tf) joined — zero log
      # ingestion is by design while the box is intentionally stopped, so it
      # needs the same window gate to never false-page off-hours.
      # 2026-07-06 (silent-feed incident hardening): the retuned tick-gap
      # alarm (its gauge is only written in-session, so the last value goes
      # stale post-close) + the 3 new silent-feed alarms
      # (silent-feed-alarms.tf) joined the same window.
      # 2026-07-10 (pre-09:00 deferral false-page fix): the two ws-pool
      # alarms (ws-pool-all-dead + ws-failed-connections, app-alarms.tf)
      # joined — the pool watchdog writes their gauges 24/7 while the pool
      # deliberately opens zero Dhan sockets until 09:00 IST, so always-armed
      # actions paged "all connections down" every trading morning
      # ~08:34-09:00 and on overnight catch-up-deploy boots (observed
      # 2026-07-10 at 02:45, 03:42, 08:34 IST). The open path's
      # set_alarm_state(OK) below resets any stale pre-open ALARM at window
      # open (edge-triggered — no false-page carry-over into the armed
      # window).
      ALARM_NAMES = join(",", [
        aws_cloudwatch_metric_alarm.market_hours_liveness_missing.alarm_name,
        aws_cloudwatch_metric_alarm.realtime_guarantee_critical.alarm_name,
        aws_cloudwatch_metric_alarm.aggregator_no_seals.alarm_name,
        aws_cloudwatch_metric_alarm.order_update_reconnect_storm.alarm_name, # 2026-07-06 flapper alarm
        aws_cloudwatch_metric_alarm.app_log_ingestion_silent.alarm_name,
        aws_cloudwatch_metric_alarm.tick_gap_instruments_silent.alarm_name,
        aws_cloudwatch_metric_alarm.realtime_guarantee_degraded.alarm_name,
        aws_cloudwatch_metric_alarm.boundary_catchup_storm_dhan.alarm_name,
        aws_cloudwatch_metric_alarm.dhan_exchange_lag_p99_high.alarm_name,
        aws_cloudwatch_metric_alarm.groww_exchange_lag_p99_high.alarm_name, # 2026-07-11 scoreboard PR-C
        aws_cloudwatch_metric_alarm.ws_pool_all_dead.alarm_name,      # 2026-07-10 deferral false-page fix
        aws_cloudwatch_metric_alarm.ws_failed_connections.alarm_name, # 2026-07-10 deferral false-page fix
      ])
      # Weekday-NSE-holiday safety: the open path skips enabling when this
      # instance is not up (holiday-gate.sh self-stop). Referencing
      # aws_instance.tv_app.id from a Lambda env is cycle-free — the proven
      # pattern from start-watchdog-lambda.tf (the cycle concern in main.tf
      # applies only to the instance's OWN role policy).
      EC2_INSTANCE_ID = aws_instance.tv_app.id
      # Round-3 holiday-race hardening: the intentional-stop marker
      # holiday-gate.sh writes before its self-stop. Checked FIRST on open —
      # cannot be raced by a restart-war up-burst at the 09:20 sample.
      HOLIDAY_STOP_PARAM = "/tickvault/${var.environment}/holiday-stop-date"
    }
  }
}

resource "aws_cloudwatch_log_group" "tv_market_hours_liveness_gate" {
  name              = "/aws/lambda/tv-${var.environment}-market-hours-liveness-gate"
  retention_in_days = 14
}

# ---------------------------------------------------------------------------
# Watch the watchman (round-13, 2026-07-06): the gate Lambda's 09:20 IST open
# invocation is the ONLY path that arms the 12 gated alarms (the ALARM_NAMES
# env list above — incl. the leg-3 order-update reconnect-storm pager + the
# 2026-07-06 silent-feed set + the 2026-07-10 ws-pool pair). A gate failure
# previously re-opened
# the 2026-07-06 zero-page gap SILENTLY — the gated alarms simply stayed
# disarmed all session with nothing watching the gate itself. Same shape as
# the readiness watchman (market-open-readiness-lambda.tf): AWS/Lambda
# Errors, Sum >= 1 per 300s, notBreaching. Residual: a rule that never
# INVOKES the Lambda (scheduler drop / disabled rule) produces no Errors
# datapoint at all (notBreaching -> silent) — the explicit state = "ENABLED"
# pins + the liveness alarms are the backstop.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "market_hours_gate_lambda_errors" {
  alarm_name          = "tv-${var.environment}-market-hours-gate-errors"
  alarm_description   = "The market-hours gate Lambda FAILED - its 09:20 IST open invocation is the ONLY path that arms the 12 gated alarms (market-hours-liveness-missing, realtime-guarantee-critical, aggregator-no-seals, order-update-reconnect-storm, app-log-ingestion-silent, tick-gap-instruments-silent, realtime-guarantee-degraded, boundary-catchup-storm-dhan, dhan-exchange-lag-p99-high, groww-exchange-lag-p99-high, ws-pool-all-dead, ws-failed-connections - the Lambda's ALARM_NAMES env is the authoritative list). A failed open leaves all 12 disarmed for the session (the 2026-07-06 leg-3 zero-page class); a failed close leaves them armed overnight (false-page risk). NO green OK page ever follows this alarm (ok_actions suppressed - the Lambda runs 2x/day, so an auto-OK is aged-out, never a fix): manually re-arm/verify the 12 gated alarms (enable_alarm_actions / disable_alarm_actions) REGARDLESS, after reading the gate Lambda's log group."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions = {
    FunctionName = aws_lambda_function.tv_market_hours_liveness_gate.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  # NO ok_actions (round-14): this Lambda runs 2x/day (09:20 open / 15:35
  # close), so the post-ALARM auto-OK is always AGED-OUT, never a fix — a
  # recurring Rule-11 false-recovery green per failure episode. Worse, for
  # THIS watchman the green also invited skipping the manual re-arm of the
  # 12 gated alarms (incl. the leg-3 reconnect-storm pager) — the
  # description above says: re-arm manually REGARDLESS.
  ok_actions = []
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
  description = "Market-hours liveness alarm (pages on a wedged/crash-looped/dead app in the 09:20-15:35 IST window). Signal: the tv_realtime_guarantee_score gauge MISSING (treat_missing_data=breaching) — emitted every 10s by the SLO loop in crates/app/src/main.rs, in the CW-agent filter (user-data.sh.tftpl). Takes over from the boot-heartbeat window at exactly 09:20 IST (2026-07-09 — the boot window close moved 09:10→09:20, so there is no seam over the 09:15 market open). The same gate Lambda also window-gates realtime-guarantee-critical + aggregator-no-seals (2026-07-03 5 AM false-SOS fix)."
  value       = aws_cloudwatch_metric_alarm.market_hours_liveness_missing.alarm_name
}
