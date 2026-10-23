# wipe-2026-10-31.tf - ONE-OFF permission for the S3 and snapshot half of the
# 2026-10-31 market-data wipe (operator Quote 35, plan item 67).
#
# Authorization (recorded BEFORE this file was written):
#   docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md
#   "Quote 35 (2026-10-21, evening) - ONE FULL MARKET-DATA WIPE, BOX AND S3,
#   ON THE NIGHT OF SATURDAY 2026-10-31" and its "The Quote 35 tool" table.
#
# What this grants, and only this:
#   * a NEW role, trusted ONLY by the GitHub Actions environment
#     `wipe-2026-10-31` of this repository (the dispatch-only workflow
#     .github/workflows/wipe-2026-10-31.yml, job `cloud`);
#   * s3:ListBucketVersions on tv-prod-cold, only for the exact prefixes the
#     tool lists (one per market-data table, plus raw-frames/, seal-spill/,
#     top-volume-spill/);
#   * s3:DeleteObjectVersion (never s3:DeleteObject, so it cannot even add a
#     delete marker) on keys dated before 2026-11-01 under
#     questdb-partitions/<one of the market-data tables>/, raw-frames/<date>/,
#     seal-spill/<date>/ and top-volume-spill/<date>/, and only from 18:00 IST
#     on 2026-10-31;
#   * ec2:DeleteSnapshot only on a snapshot tagged BOTH
#     tv-rebuild=2026-10-22 and Name=tv-prod-root-before-rebuild-2026-10-22
#     (the old-root snapshot the 2026-10-22 rebuild made), same window;
#   * read-only ec2:Describe{Instances,Snapshots,Volumes} (the cloud deletes
#     run only with the box stopped);
#   * an explicit Deny on deploys/, sebi-preserve/, logs/, every key dated
#     2026-11 and every bucket setting;
#   * the trust policy and every Allow stop working at the end of 2026-11-01
#     IST (aws:CurrentTime), even if the removal PR is late.
# No existing role, user or bucket policy gains anything. The box half runs
# over SSM with the existing deploy credentials and needs no new permission.
#
# REMOVED after the run: the first PR after the wipe deletes this file, the
# workflow and the tool, and marks Quote 35 ENDED (Quote 27 binds again).

locals {
  wipe_github_environment = "wipe-2026-10-31"

  # Deletes are refused before 18:00 IST on Saturday 2026-10-31.
  wipe_deletes_from = "2026-10-31T12:30:00Z"
  # IAM stops honouring the role at the end of 2026-11-01 IST.
  wipe_expires_at = "2026-11-01T18:30:00Z"

  # The market-data tables of Quote 35. SEBI, audit and lifecycle tables are
  # deliberately absent. Keep in lockstep with
  # wipe_2026_10_31::market_data_tables() (the test
  # test_market_data_tables_match_the_iam_grant compares them).
  wipe_tables = [
    "candles_10m",
    "candles_10s",
    "candles_11s",
    "candles_12s",
    "candles_13s",
    "candles_14s",
    "candles_15m",
    "candles_15s",
    "candles_1d",
    "candles_1m",
    "candles_1s",
    "candles_2m",
    "candles_2s",
    "candles_30m",
    "candles_30s",
    "candles_3m",
    "candles_3s",
    "candles_4s",
    "candles_5m",
    "candles_5s",
    "candles_60m",
    "candles_6s",
    "candles_7s",
    "candles_8s",
    "candles_9s",
    "feed_aux_packets",
    "market_depth",
    "market_depth_book",
    "ticks",
    "top_volume",
    "top_volume_1m",
    "top_volume_1s",
    "top_volume_3s",
    "top_volume_5s",
  ]

  # Partition object names start with the partition date (IST wall time):
  # 2026-10-01.csv.gz, 2026-10-01T09.csv.gz, 2026-10-01T09.sha-<hex>.csv.gz.
  # "2026-0?-??" is January to September; "2026-1?-??" would be October to
  # December; only "2026-10-??" is granted, and the Deny below also refuses
  # November and December outright.
  wipe_partition_arns = flatten([
    for t in local.wipe_tables : [
      "${aws_s3_bucket.tv_cold.arn}/questdb-partitions/${t}/2026-0?-??*",
      "${aws_s3_bucket.tv_cold.arn}/questdb-partitions/${t}/2026-10-??*",
    ]
  ])

  wipe_dated_roots = ["raw-frames", "seal-spill", "top-volume-spill"]

  wipe_list_prefixes = concat(
    [for t in local.wipe_tables : "questdb-partitions/${t}/"],
    [for r in local.wipe_dated_roots : "${r}/"],
  )
}

resource "aws_iam_role" "wipe_2026_10_31" {
  name                 = "tv-${var.environment}-wipe-2026-10-31"
  description          = "One-off Quote 35: delete pre-2026-11-01 market-data copies and the old-root snapshot. Remove after the run."
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
          "token.actions.githubusercontent.com:sub" = "repo:${var.github_repo_full_name}:environment:${local.wipe_github_environment}"
        }
        DateLessThan = {
          "aws:CurrentTime" = local.wipe_expires_at
        }
      }
    }]
  })
}

resource "aws_iam_role_policy" "wipe_2026_10_31" {
  name = "tv-${var.environment}-wipe-2026-10-31"
  role = aws_iam_role.wipe_2026_10_31.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "ListOnlyTheExactPrefixes"
        Effect   = "Allow"
        Action   = ["s3:ListBucketVersions"]
        Resource = aws_s3_bucket.tv_cold.arn
        Condition = {
          StringEquals = {
            "s3:prefix" = local.wipe_list_prefixes
          }
          DateLessThan = {
            "aws:CurrentTime" = local.wipe_expires_at
          }
        }
      },
      {
        Sid    = "DeleteVersionsDatedBeforeNovember"
        Effect = "Allow"
        Action = ["s3:DeleteObjectVersion"]
        Condition = {
          DateGreaterThan = {
            "aws:CurrentTime" = local.wipe_deletes_from
          }
          DateLessThan = {
            "aws:CurrentTime" = local.wipe_expires_at
          }
        }
        Resource = concat(
          local.wipe_partition_arns,
          flatten([
            for r in local.wipe_dated_roots : [
              "${aws_s3_bucket.tv_cold.arn}/${r}/2026-0?-??/*",
              "${aws_s3_bucket.tv_cold.arn}/${r}/2026-10-??/*",
            ]
          ]),
        )
      },
      {
        Sid    = "ReadTheInstanceAndSnapshots"
        Effect = "Allow"
        Action = [
          "ec2:DescribeInstances",
          "ec2:DescribeSnapshots",
          "ec2:DescribeVolumes",
        ]
        Resource = "*"
        Condition = {
          DateLessThan = {
            "aws:CurrentTime" = local.wipe_expires_at
          }
        }
      },
      {
        Sid      = "DeleteOnlyTheOldRootSnapshot"
        Effect   = "Allow"
        Action   = ["ec2:DeleteSnapshot"]
        Resource = "arn:aws:ec2:${var.aws_region}::snapshot/*"
        Condition = {
          StringEquals = {
            "aws:ResourceTag/tv-rebuild" = "2026-10-22"
            "aws:ResourceTag/Name"       = "tv-prod-root-before-rebuild-2026-10-22"
          }
          DateGreaterThan = {
            "aws:CurrentTime" = local.wipe_deletes_from
          }
          DateLessThan = {
            "aws:CurrentTime" = local.wipe_expires_at
          }
        }
      },
      {
        Sid    = "NeverNovemberDeploysLogsOrSebi"
        Effect = "Deny"
        Action = ["s3:*"]
        Resource = [
          "${aws_s3_bucket.tv_cold.arn}/questdb-partitions/*/2026-11-*",
          "${aws_s3_bucket.tv_cold.arn}/questdb-partitions/*/2026-12-*",
          "${aws_s3_bucket.tv_cold.arn}/raw-frames/2026-11-*",
          "${aws_s3_bucket.tv_cold.arn}/raw-frames/2026-12-*",
          "${aws_s3_bucket.tv_cold.arn}/seal-spill/2026-11-*",
          "${aws_s3_bucket.tv_cold.arn}/seal-spill/2026-12-*",
          "${aws_s3_bucket.tv_cold.arn}/top-volume-spill/2026-11-*",
          "${aws_s3_bucket.tv_cold.arn}/top-volume-spill/2026-12-*",
          "${aws_s3_bucket.tv_cold.arn}/deploys/*",
          "${aws_s3_bucket.tv_cold.arn}/sebi-preserve/*",
          "${aws_s3_bucket.tv_cold.arn}/logs/*",
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

output "wipe_2026_10_31_role_arn" {
  description = "One-off Quote 35 wipe role; removed after the run"
  value       = aws_iam_role.wipe_2026_10_31.arn
}
