# =============================================================================
# Log export to S3 — 2026-10-11 (DA-11)
# =============================================================================
# AUTHORITY: dhan-rest-only-noise-lock-2026-07-14.md §2.10-a (the owner tapped
# "Logs and alarms" on the card "Prepare which AWS cost changes from the audit
# as code?"). Cost recorded in aws-budget.md "COST NOTE 2026-10-11".
#
# WHAT IT DOES. CloudWatch keeps the app log and the Lambda logs for 14 or 30
# days. This file copies every event, as it arrives, into the cold bucket so a
# copy is kept after CloudWatch deletes its own:
#
#   log group --subscription filter--> Kinesis Firehose --> s3://tv-<env>-cold/
#     /tickvault/<env>/app          -> tv-<env>-app-logs-to-s3    -> logs/app/
#     the 12 Lambda log groups      -> tv-<env>-lambda-logs-to-s3 -> logs/lambda/
#
# It is a DATA COPY, not a page: no alarm, no metric, no Telegram line.
#
# NOT EXPORTED: /tickvault/<env>/metrics (the raw scrapes; volume unknown, a
# later decision) and the Firehose error log group below (exporting it into
# the stream that writes it would loop).
#
# FORMAT. CloudWatch Logs hands Firehose each batch already gzipped (Assumed
# from the AWS documentation; not observed on this account). Firehose is told
# UNCOMPRESSED so it does not compress twice, and each object is named
# .json.gz. An object is a run of gzip members, which gunzip reads as one
# stream of CloudWatch Logs JSON records.
#
# DATES. The prefix uses the date the record ARRIVED at Firehose, in UTC, not
# the date of the log line. A line written at 23:59 UTC can land in the next
# day's folder.
#
# RETENTION. logs/ sits under the cold bucket's existing tick-cold-tiering
# lifecycle rule (main.tf): it tiers down and never expires. No lifecycle rule
# is added or changed here, and CloudWatch retention is unchanged (CloudWatch
# is the hot copy, S3 the permanent one).
#
# PERMISSIONS (least privilege, Quote 27 of the daily-universe lock):
#   tv-<env>-logs-to-firehose: CloudWatch Logs may put records into the two
#     streams only, and only from the app log group or a tv-<env> Lambda
#     log group (two prefix patterns; the trust policy has a 2,048-character
#     limit that a per-group list broke on 2026-10-12).
#   tv-<env>-firehose-logs-to-s3: Firehose may write objects under logs/ and
#     logs-errors/ of the cold bucket only, list that one bucket, and write its
#     own error log stream. No Delete action, no lifecycle or versioning
#     action, no wildcard bucket.
# Ratchet: crates/common/tests/log_export_wiring_guard.rs.
#
# HONEST LIMITS:
#   - Only events written after the apply are exported; nothing is backfilled.
#   - A Firehose delivery failure is written to the error log group and to
#     logs-errors/, and pages no one (a page would need its own dated row).
#   - AWS allows two subscription filters per log group. Terraform holds none
#     today; the live account was NOT read, so check it before the apply.
# =============================================================================

locals {
  # The 12 terraform-managed Lambda log groups. Every one is a resource in
  # this module, so a renamed or deleted group fails the plan here.
  lambda_log_export_groups = {
    "boot-heartbeat-gate"        = aws_cloudwatch_log_group.tv_boot_heartbeat_gate.name
    "daily-budget-digest"        = aws_cloudwatch_log_group.tv_daily_budget_digest.name
    "hard-stop-guard"            = aws_cloudwatch_log_group.tv_hard_stop_guard.name
    "budget-killswitch"          = aws_cloudwatch_log_group.budget_killswitch.name
    "deploy-watchdog"            = aws_cloudwatch_log_group.deploy_watchdog.name
    "dhan-depth-token-minter"    = aws_cloudwatch_log_group.dhan_depth_token_minter.name
    "dhan-token-minter"          = aws_cloudwatch_log_group.dhan_token_minter.name
    "operator-control"           = aws_cloudwatch_log_group.operator_control.name
    "market-hours-liveness-gate" = aws_cloudwatch_log_group.tv_market_hours_liveness_gate.name
    "market-open-readiness"      = aws_cloudwatch_log_group.market_open_readiness.name
    "start-watchdog"             = aws_cloudwatch_log_group.start_watchdog.name
    "telegram-webhook"           = aws_cloudwatch_log_group.telegram_webhook.name
  }

  log_export_firehose_log_group = "/aws/kinesisfirehose/tv-${var.environment}-logs-to-s3"
}

# ---------------------------------------------------------------------------
# Firehose's own error log (delivery failures). Not exported (see header).
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_log_group" "log_export_firehose" {
  name              = local.log_export_firehose_log_group
  retention_in_days = 14
}

resource "aws_cloudwatch_log_stream" "log_export_app_delivery" {
  name           = "app-logs-to-s3"
  log_group_name = aws_cloudwatch_log_group.log_export_firehose.name
}

resource "aws_cloudwatch_log_stream" "log_export_lambda_delivery" {
  name           = "lambda-logs-to-s3"
  log_group_name = aws_cloudwatch_log_group.log_export_firehose.name
}

# ---------------------------------------------------------------------------
# Role B: Firehose writes into the cold bucket.
# ---------------------------------------------------------------------------
resource "aws_iam_role" "firehose_logs_to_s3" {
  name = "tv-${var.environment}-firehose-logs-to-s3"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "firehose.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        StringEquals = { "aws:SourceAccount" = data.aws_caller_identity.current.account_id }
      }
    }]
  })
}

resource "aws_iam_role_policy" "firehose_logs_to_s3" {
  name = "tv-${var.environment}-firehose-logs-to-s3-policy"
  role = aws_iam_role.firehose_logs_to_s3.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # Write objects under the two log prefixes only. No Delete action.
        Effect = "Allow"
        Action = [
          "s3:PutObject",
          "s3:AbortMultipartUpload",
        ]
        Resource = [
          "${aws_s3_bucket.tv_cold.arn}/logs/*",
          "${aws_s3_bucket.tv_cold.arn}/logs-errors/*",
        ]
      },
      {
        # Firehose checks the bucket before it writes. This one bucket only.
        Effect = "Allow"
        Action = [
          "s3:GetBucketLocation",
          "s3:ListBucket",
          "s3:ListBucketMultipartUploads",
        ]
        Resource = aws_s3_bucket.tv_cold.arn
      },
      {
        # Its own delivery-error stream only.
        Effect = "Allow"
        Action = ["logs:PutLogEvents"]
        Resource = [
          "${aws_cloudwatch_log_group.log_export_firehose.arn}:log-stream:${aws_cloudwatch_log_stream.log_export_app_delivery.name}",
          "${aws_cloudwatch_log_group.log_export_firehose.arn}:log-stream:${aws_cloudwatch_log_stream.log_export_lambda_delivery.name}",
        ]
      },
    ]
  })
}

# ---------------------------------------------------------------------------
# The two delivery streams.
# ---------------------------------------------------------------------------
resource "aws_kinesis_firehose_delivery_stream" "app_logs_to_s3" {
  name        = "tv-${var.environment}-app-logs-to-s3"
  destination = "extended_s3"

  extended_s3_configuration {
    role_arn   = aws_iam_role.firehose_logs_to_s3.arn
    bucket_arn = aws_s3_bucket.tv_cold.arn
    # Arrival date in UTC (see header).
    prefix              = "logs/app/!{timestamp:yyyy/MM/dd}/"
    error_output_prefix = "logs-errors/!{firehose:error-output-type}/!{timestamp:yyyy/MM/dd}/"
    buffering_size      = 64
    buffering_interval  = 900
    # Already gzipped by CloudWatch Logs (see header).
    compression_format = "UNCOMPRESSED"
    file_extension     = ".json.gz"
    s3_backup_mode     = "Disabled"

    cloudwatch_logging_options {
      enabled         = true
      log_group_name  = aws_cloudwatch_log_group.log_export_firehose.name
      log_stream_name = aws_cloudwatch_log_stream.log_export_app_delivery.name
    }
  }

  depends_on = [aws_iam_role_policy.firehose_logs_to_s3]
}

resource "aws_kinesis_firehose_delivery_stream" "lambda_logs_to_s3" {
  name        = "tv-${var.environment}-lambda-logs-to-s3"
  destination = "extended_s3"

  extended_s3_configuration {
    role_arn   = aws_iam_role.firehose_logs_to_s3.arn
    bucket_arn = aws_s3_bucket.tv_cold.arn
    # Arrival date in UTC (see header).
    prefix              = "logs/lambda/!{timestamp:yyyy/MM/dd}/"
    error_output_prefix = "logs-errors/!{firehose:error-output-type}/!{timestamp:yyyy/MM/dd}/"
    buffering_size      = 64
    buffering_interval  = 900
    # Already gzipped by CloudWatch Logs (see header).
    compression_format = "UNCOMPRESSED"
    file_extension     = ".json.gz"
    s3_backup_mode     = "Disabled"

    cloudwatch_logging_options {
      enabled         = true
      log_group_name  = aws_cloudwatch_log_group.log_export_firehose.name
      log_stream_name = aws_cloudwatch_log_stream.log_export_lambda_delivery.name
    }
  }

  depends_on = [aws_iam_role_policy.firehose_logs_to_s3]
}

# ---------------------------------------------------------------------------
# Role A: CloudWatch Logs puts records into the two streams.
# ---------------------------------------------------------------------------
resource "aws_iam_role" "logs_to_firehose" {
  name = "tv-${var.environment}-logs-to-firehose"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "logs.${var.aws_region}.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        # Only the app log group and this environment's Lambda log groups may
        # use this role. Two prefix patterns, not one ARN per group: the
        # per-group list (two ARN forms for each of the 13 groups) broke AWS's
        # 2,048-character limit on a role's trust policy (ACLSizePerRole), and
        # every terraform apply from 2026-10-12 failed creating this role, so
        # no subscription filter existed and nothing reached S3. The trailing
        # * covers both forms CloudWatch Logs may send (with and without
        # ":*"). Wider than the list: any tv-<env> Lambda log group matches,
        # but the role can only put records into the two streams below, and a
        # subscription filter still has to name the role. Every exported
        # Lambda group matches the second pattern (ratcheted by
        # log_export_wiring_guard).
        StringLike = {
          "aws:SourceArn" = [
            "arn:aws:logs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:log-group:${aws_cloudwatch_log_group.tv_app.name}*",
            "arn:aws:logs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:log-group:/aws/lambda/tv-${var.environment}-*",
          ]
        }
      }
    }]
  })
}

resource "aws_iam_role_policy" "logs_to_firehose" {
  name = "tv-${var.environment}-logs-to-firehose-policy"
  role = aws_iam_role.logs_to_firehose.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "firehose:PutRecord",
        "firehose:PutRecordBatch",
      ]
      Resource = [
        aws_kinesis_firehose_delivery_stream.app_logs_to_s3.arn,
        aws_kinesis_firehose_delivery_stream.lambda_logs_to_s3.arn,
      ]
    }]
  })
}

# ---------------------------------------------------------------------------
# The subscription filters: every event (empty pattern) goes to its stream.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_log_subscription_filter" "app_logs_to_s3" {
  name            = "tv-${var.environment}-app-logs-to-s3"
  log_group_name  = aws_cloudwatch_log_group.tv_app.name
  filter_pattern  = ""
  destination_arn = aws_kinesis_firehose_delivery_stream.app_logs_to_s3.arn
  role_arn        = aws_iam_role.logs_to_firehose.arn

  depends_on = [aws_iam_role_policy.logs_to_firehose]
}

resource "aws_cloudwatch_log_subscription_filter" "lambda_logs_to_s3" {
  for_each = local.lambda_log_export_groups

  name            = "tv-${var.environment}-${each.key}-logs-to-s3"
  log_group_name  = each.value
  filter_pattern  = ""
  destination_arn = aws_kinesis_firehose_delivery_stream.lambda_logs_to_s3.arn
  role_arn        = aws_iam_role.logs_to_firehose.arn

  depends_on = [aws_iam_role_policy.logs_to_firehose]
}
