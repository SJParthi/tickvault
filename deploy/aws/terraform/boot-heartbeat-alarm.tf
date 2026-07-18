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
# the 08:40 IST soft boot deadline in §10) and OFF at 09:20 IST. Outside that
# 30-min weekday window the alarm publishes nothing, so the nightly/weekend
# stop can never page. This mirrors the inline-Lambda pattern already used in
# budget-guards.tf.
#
# 2026-07-09 SEAM CLOSURE (audit finding — the 09:10–09:20 IST blind hole
# spanning the 09:15 market open): the close was originally 09:10 IST while
# the market-hours liveness window (market-hours-liveness-alarm.tf) only opens
# at 09:20 IST — AND its open-mode Lambda resets its alarms to OK, so its
# 5-period missing-data evaluation cannot page before ~09:25-09:26 IST. Net: a
# process death anywhere in [09:10, 09:20) IST — exactly spanning the market
# open — paged NOBODY inside the seam and at best ~09:25 (up to ~15 min blind).
# tv_boot_completed is the RIGHT signal to extend: metrics-exporter-prometheus
# re-renders the gauge on every 60s CW-agent scrape while the process is alive
# (verified: scrape_interval 60s + the metric_selectors allowlist in
# user-data.sh.tftpl), so a healthy app publishes a datapoint every period
# through 09:10–09:20 (no false-page in the extension) and a dead one goes
# missing → 2×60s → page within ~2-3 min. Widening the MARKET-HOURS window to
# 09:10 instead was REJECTED: its ALARM_NAMES list gates 12 alarms (count
# 9 → 11 on 2026-07-10 with the ws-pool pair, → 12 on 2026-07-11 with
# groww-exchange-lag-p99-high) — 10 whose signals are deliberately invalid
# pre-09:20 (SLO tick-freshness pre-open pin, the 9-of-15 degraded
# lookback, first-score warmup, lag-window warmup), plus the 2 ws-pool pagers
# (ws-pool-all-dead + ws-failed-connections) whose gauges ARE valid from the
# 09:00 IST pool connect but are gated for the pre-09:00 Dhan connect-
# deferral false-page class (with the 09:00–09:20 handover residual accepted
# — see app-alarms.tf). Widening would re-open the exact pre-open false-page
# class the 09:20 gate was built to avoid for BOTH groups. The boot
# window now hands over to the market-hours window at exactly 09:20 IST.
# Residual (honest): a death in ~[09:16, 09:20) may not complete this alarm's
# 2-period evaluation before the close disables actions (CloudWatch's
# missing-data evaluation range can pad the naive 2×60s by 1-2 periods, so the
# safe boundary is ~09:16, not ~09:17:30) — the market-hours liveness alarm
# then pages at ~09:25-09:26 (worst ~9-10 min, inside the ≤10 min envelope).
# Ratchet:
# crates/common/tests/aws_alarm_semantics_guard.rs
# (test_boot_heartbeat_window_hands_over_to_market_hours_window).
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

  # Actions OFF by default; the boot-window Lambda flips them ON 08:50-09:20 IST
  # Mon-Fri (close widened from 09:10 on 2026-07-09 — market-open seam closure)
  # so the intentional nightly/weekend stop can never false-page.
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# Boot-window gate Lambda — enables the alarm's actions during the morning boot
# window and disables them otherwise. Pattern mirrors budget-guards.tf.
#
# 2026-07-18 (rust-only phase 2b-1): the inline Python heredoc was PORTED to
# Rust — crates/aws-lambdas/src/alarm_gate.rs (lib logic + unit tests) +
# src/bin/boot_heartbeat_gate.rs (thin bootstrap bin). Behavior parity:
# mode="open" (08:50 IST) → enable actions + reset the alarm to OK with the
# exact 'boot-heartbeat window opened (08:50 IST)' reason; any other mode
# (incl. missing) → close, disabling actions — so the nightly/weekend stop
# (metric goes missing intentionally) never pages. (2026-07-09: close was
# moved 09:10 → 09:20 to close the market-open seam; see the header note.)
# The zip is built in CI by the build-lambdas job (terraform-apply.yml) and
# downloaded into ${path.module}/.lambda-zips/ before plan/apply;
# source_code_hash is a digest of the Rust SOURCE (Rust builds are not
# bit-reproducible, so hashing the zip would churn every build).
# ---------------------------------------------------------------------------

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
  filename         = "${path.module}/.lambda-zips/boot-heartbeat-gate.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))
  role             = aws_iam_role.tv_boot_heartbeat_gate.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
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
  # RESUME 2026-07-06 per operator GO (moved up from Thu Jul 9): explicit state
  # so terraform re-enables this rule. The Jul 4 pause set state = "DISABLED";
  # the #1404 revert REMOVED the attribute, and with `state` absent the AWS
  # provider stops managing rule state - the rule stayed DISABLED on AWS.
  state = "ENABLED"
}

# Close the window at 09:20 IST (03:50 UTC) Mon-Fri — the exact minute the
# market-hours liveness window opens (market-hours-liveness-alarm.tf
# tv_market_hours_liveness_open, also cron(50 3)), so liveness coverage hands
# over with NO seam across the 09:15 market open. 2026-07-09: moved from
# 09:10 IST (cron(40 3)) — the old close left [09:10, 09:20) IST with no alarm
# able to page a dead app (see the header SEAM CLOSURE note). The two crons
# act on DISJOINT alarm sets (this Lambda gates ONLY boot_heartbeat_missing;
# the market gate manages its own 9), so same-minute firing has no race.
resource "aws_cloudwatch_event_rule" "tv_boot_heartbeat_close" {
  name                = "tv-${var.environment}-boot-heartbeat-close"
  description         = "Disable boot-heartbeat alarm actions at 09:20 IST (Mon-Fri) — hands over to the market-hours liveness window (2026-07-09 seam closure; was 09:10)"
  schedule_expression = "cron(50 3 ? * MON-FRI *)"
  # 2026-07-09 (defense-in-depth — the same #1404 lesson the open rule learned):
  # with `state` absent the AWS provider stops MANAGING rule state, so a
  # once-disabled close rule stays disabled forever on AWS — leaving this
  # breaching-on-missing alarm's actions armed past 09:20 and false-paging the
  # intentional 16:30 IST stop nightly. Pin ENABLED explicitly.
  state = "ENABLED"
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
  description = "Boot-heartbeat alarm (pages on a hung/never-booted/DIED-BEFORE-09:20 app in the 08:50-09:20 IST window — widened from 09:10 on 2026-07-09 to close the market-open seam). Signal: the dedicated tv_boot_completed gauge MISSING (treat_missing_data=breaching) — emitted by crates/app/src/main.rs on a completed boot, in the CW-agent filter (daily-universe-scope-expansion-2026-05-27.md §19). Repointed off the tv_realtime_guarantee_score proxy (PR #1278 follow-up)."
  value       = aws_cloudwatch_metric_alarm.boot_heartbeat_missing.alarm_name
}
