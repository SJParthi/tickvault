# s3-old-data-delete-2026-10-09.tf - ONE-OFF permission to delete the
# pre-2026-10-09 market-data copies in s3://tv-prod-cold (operator Quote 29e).
#
# Authorization (recorded BEFORE this file was written):
#   docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md
#   "Quote 29e (2026-10-09, 21:52 IST) - ONE-TIME S3 DELETE PERMISSION".
#   The operator tapped "One-time permission" on the card "Allow a one-time S3
#   delete permission to remove the pre-9-Oct copies?".
#
# What this grants, and only this:
#   * a NEW role, trusted ONLY by the GitHub Actions environment
#     `s3-old-data-delete-2026-10-09` of this repository (the dispatch-only
#     workflow .github/workflows/s3-old-data-delete-2026-10-09.yml);
#   * s3:ListBucketVersions on tv-prod-cold, only for the 19 exact prefixes
#     the tool lists;
#   * s3:DeleteObjectVersion (never s3:DeleteObject, so it cannot even add a
#     delete marker) on keys dated before 2026-10-09 under
#     questdb-partitions/<one of the 17 market-data tables>/, raw-frames/<date>/
#     and seal-spill/<date>/;
#   * an explicit Deny on every key dated 2026-10-09, on deploys/ and on
#     sebi-preserve/, so the "2026-10-0?" patterns can never reach today.
#   * the trust policy and both Allow statements stop working at the end of
#     2026-10-16 IST (aws:CurrentTime), even if the removal PR is late.
# No existing role, user or bucket policy gains anything.
#
# REMOVED after the run: the first PR after the delete deletes this file, the
# workflow and the tool, and marks Quote 29e ENDED (Quote 27 binds again).

locals {
  s3_old_delete_github_environment = "s3-old-data-delete-2026-10-09"

  # IAM itself stops honouring the role at the end of 2026-10-16 IST, the
  # tool's last run day, even if the removal PR is late.
  s3_old_delete_expires_at = "2026-10-16T18:30:00Z"

  # The 17 market-data tables of Quote 29. SEBI, audit and lifecycle tables
  # are deliberately absent. Keep in lockstep with MARKET_DATA_TABLES in
  # crates/app/src/s3_old_data_delete.rs.
  s3_old_delete_tables = [
    "candles_10m",
    "candles_15m",
    "candles_1m",
    "candles_1s",
    "candles_30m",
    "candles_3m",
    "candles_3s",
    "candles_5m",
    "candles_5s",
    "candles_60m",
    "feed_aux_packets",
    "market_depth",
    "ticks",
    "top_volume_1m",
    "top_volume_1s",
    "top_volume_3s",
    "top_volume_5s",
  ]

  # Partition object names start with the partition date: 2026-10-01.csv.gz,
  # 2026-10-01T09.csv.gz, 2026-10-01T09.sha-<hex>.csv.gz. "2026-0?-??" is
  # January to September; "2026-10-0?" is 1 to 9 October, and the Deny below
  # removes the 9th.
  s3_old_delete_partition_arns = flatten([
    for t in local.s3_old_delete_tables : [
      "${aws_s3_bucket.tv_cold.arn}/questdb-partitions/${t}/2026-0?-??*",
      "${aws_s3_bucket.tv_cold.arn}/questdb-partitions/${t}/2026-10-0?*",
    ]
  ])

  s3_old_delete_list_prefixes = concat(
    [for t in local.s3_old_delete_tables : "questdb-partitions/${t}/"],
    ["raw-frames/", "seal-spill/"],
  )
}

resource "aws_iam_role" "s3_old_data_delete" {
  name                 = "tv-${var.environment}-s3-old-data-delete-2026-10-09"
  description          = "One-off Quote 29e: delete pre-2026-10-09 market-data copies in tv-cold. Remove after the run."
  max_session_duration = 3600

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Principal = {
        Federated = aws_iam_openid_connect_provider.github.arn
      }
      Action = "sts:AssumeRoleWithWebIdentity"
      Condition = {
        StringEquals = {
          "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com"
          "token.actions.githubusercontent.com:sub" = "repo:${var.github_repo_full_name}:environment:${local.s3_old_delete_github_environment}"
        }
        DateLessThan = {
          "aws:CurrentTime" = local.s3_old_delete_expires_at
        }
      }
    }]
  })
}

resource "aws_iam_role_policy" "s3_old_data_delete" {
  name = "tv-${var.environment}-s3-old-data-delete-2026-10-09"
  role = aws_iam_role.s3_old_data_delete.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "ListOnlyTheNineteenPrefixes"
        Effect   = "Allow"
        Action   = ["s3:ListBucketVersions"]
        Resource = aws_s3_bucket.tv_cold.arn
        Condition = {
          StringEquals = {
            "s3:prefix" = local.s3_old_delete_list_prefixes
          }
          DateLessThan = {
            "aws:CurrentTime" = local.s3_old_delete_expires_at
          }
        }
      },
      {
        Sid    = "DeleteVersionsDatedBeforeTheNinth"
        Effect = "Allow"
        Action = ["s3:DeleteObjectVersion"]
        Condition = {
          DateLessThan = {
            "aws:CurrentTime" = local.s3_old_delete_expires_at
          }
        }
        Resource = concat(
          local.s3_old_delete_partition_arns,
          [
            "${aws_s3_bucket.tv_cold.arn}/raw-frames/2026-0?-??/*",
            "${aws_s3_bucket.tv_cold.arn}/raw-frames/2026-10-0?/*",
            "${aws_s3_bucket.tv_cold.arn}/seal-spill/2026-0?-??/*",
            "${aws_s3_bucket.tv_cold.arn}/seal-spill/2026-10-0?/*",
          ],
        )
      },
      {
        Sid    = "NeverTodayDeploysOrSebi"
        Effect = "Deny"
        Action = ["s3:*"]
        Resource = [
          "${aws_s3_bucket.tv_cold.arn}/questdb-partitions/*/2026-10-09*",
          "${aws_s3_bucket.tv_cold.arn}/raw-frames/2026-10-09/*",
          "${aws_s3_bucket.tv_cold.arn}/seal-spill/2026-10-09/*",
          "${aws_s3_bucket.tv_cold.arn}/deploys/*",
          "${aws_s3_bucket.tv_cold.arn}/sebi-preserve/*",
        ]
      },
      {
        Sid    = "NeverChangeTheBucket"
        Effect = "Deny"
        Action = [
          "s3:DeleteBucket",
          "s3:DeleteBucketPolicy",
          "s3:PutBucketPolicy",
          "s3:PutBucketVersioning",
          "s3:PutLifecycleConfiguration",
          "s3:PutBucketPublicAccessBlock",
        ]
        Resource = aws_s3_bucket.tv_cold.arn
      },
    ]
  })
}

output "s3_old_data_delete_role_arn" {
  description = "One-off Quote 29e delete role; removed after the run"
  value       = aws_iam_role.s3_old_data_delete.arn
}
