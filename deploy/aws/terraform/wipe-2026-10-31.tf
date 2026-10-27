# wipe-2026-10-31.tf - ONE-OFF permission for the S3 and snapshot half of the
# market-data wipe (operator Quote 35, plan item 67), brought forward to the
# night of Tuesday 2026-10-27 by Quote 42 (plan item X1). Names unchanged.
#
# Authorization (recorded BEFORE this file was written):
#   docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md
#   "Quote 35 (2026-10-21, evening) - ONE FULL MARKET-DATA WIPE, BOX AND S3,
#   ON THE NIGHT OF SATURDAY 2026-10-31", its "The Quote 35 tool" table, and
#   "Quote 42 (2026-10-27, ~08:20 IST) - THE QUOTE 35 WIPE AND THE QUOTE
#   32/33 REBUILD RUN TONIGHT, 27 OCTOBER".
#
# What this grants, and only this:
#   * a NEW role, trusted ONLY by the GitHub Actions environment
#     `wipe-2026-10-31` of this repository (the dispatch-only workflow
#     .github/workflows/wipe-2026-10-31.yml, job `cloud`);
#   * s3:ListBucketVersions on tv-prod-cold, only for the exact prefixes the
#     tool lists (one per market-data table, plus raw-frames/, seal-spill/,
#     top-volume-spill/, the three quarantine roots and
#     rebuild-2026-10-22/tables/);
#   * s3:DeleteObjectVersion (never s3:DeleteObject, so it cannot even add a
#     delete marker) on 2026-dated keys under questdb-partitions/<one of the
#     market-data tables>/ and the six dated roots, and on the rebuild's
#     whole-table copies of the market-data tables
#     (rebuild-2026-10-22/tables/<table>/), only from 18:00 IST on
#     2026-10-27 (Quote 42);
#   * an explicit Deny on every key dated 2026-10-28 or later, anywhere in
#     the bucket (Quote 42's cutoff). One Allow pattern per table plus this
#     Deny, rather than one Allow per allowed date, keeps the policy under
#     IAM's 10,240-character inline limit. The rebuild copies carry no date
#     in the key: the tool deletes only a version last written before
#     2026-10-28 00:00 IST, and the new box never writes under that prefix;
#   * ec2:DeleteSnapshot only on a snapshot tagged BOTH
#     tv-rebuild=2026-10-22 and Name=tv-prod-root-before-rebuild-2026-10-22
#     (the old-root snapshot the 2026-10-22 rebuild made), only from 16:30
#     IST on 2026-10-28 (Quote 42: after Wednesday's first session);
#   * read-only ec2:Describe{Instances,Snapshots,Volumes} (the cloud deletes
#     run only with the box stopped);
#   * an explicit Deny on deploys/, sebi-preserve/, logs/,
#     rebuild-2026-10-22/keep/ (the SEBI and audit copies) and every bucket
#     setting;
#   * the trust policy and every Allow stop working at the end of 2026-11-01
#     IST (aws:CurrentTime), even if the removal PR is late.
# No existing role, user or bucket policy gains anything. The box half runs
# over SSM with the existing deploy credentials and needs no new permission.
#
# REMOVED after the run: the first PR after the wipe deletes this file, the
# workflow and the tool, and marks Quote 35 ENDED (Quote 27 binds again).

locals {
  wipe_github_environment = "wipe-2026-10-31"

  # Deletes are refused before 18:00 IST on Tuesday 2026-10-27 (Quote 42;
  # was 18:00 IST on Saturday 2026-10-31 under Quote 35).
  wipe_deletes_from = "2026-10-27T12:30:00Z"
  # The old-root snapshot may be deleted only from 16:30 IST on Wednesday
  # 2026-10-28, after the new box's first session (Quote 42).
  wipe_snapshot_delete_from = "2026-10-28T11:00:00Z"
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
  # 2026-10-01.csv.gz, 2026-10-27T09.csv.gz, 2026-10-27T09.sha-<hex>.csv.gz.
  # One 2026-dated pattern per table is granted; wipe_deny_dates below
  # refuses every key dated 2026-10-28 or later, so the grant that is left is
  # exactly "dated before 2026-10-28" (Quote 42).
  wipe_partition_arns = [
    for t in local.wipe_tables :
    "${aws_s3_bucket.tv_cold.arn}/questdb-partitions/${t}/2026-??-??*"
  ]

  # The rebuild's whole-table copies of the market-data tables
  # (rebuild-2026-10-22/tables/<table>/<sha256>.csv.gz, Quote 42). Keep-list
  # tables copied there (top_volume_rank) are not in wipe_tables.
  wipe_rebuild_table_arns = [
    for t in local.wipe_tables :
    "${aws_s3_bucket.tv_cold.arn}/rebuild-2026-10-22/tables/${t}/*"
  ]

  # <root>/<IST date>/<file>. The quarantine roots are Quote 42's.
  wipe_dated_roots = [
    "raw-frames",
    "seal-spill",
    "top-volume-spill",
    "tick-quarantine",
    "depth-quarantine",
    "top-volume-quarantine",
  ]

  # Every key whose path has a "/<date>" segment dated 2026-10-28 or later
  # (partition names, dated folders) is refused, whatever the root.
  wipe_deny_dates = ["2026-10-28", "2026-10-29", "2026-10-3?", "2026-11-", "2026-12-"]

  wipe_list_prefixes = concat(
    [for t in local.wipe_tables : "questdb-partitions/${t}/"],
    [for r in local.wipe_dated_roots : "${r}/"],
    ["rebuild-2026-10-22/tables/"],
  )
}

resource "aws_iam_role" "wipe_2026_10_31" {
  name                 = "tv-${var.environment}-wipe-2026-10-31"
  description          = "One-off Quotes 35 and 42: delete pre-2026-10-28 market-data copies and the old-root snapshot. Remove after the run."
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
        Sid    = "DeleteVersionsDatedBefore28October"
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
          local.wipe_rebuild_table_arns,
          [for r in local.wipe_dated_roots : "${aws_s3_bucket.tv_cold.arn}/${r}/2026-??-??/*"],
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
            "aws:CurrentTime" = local.wipe_snapshot_delete_from
          }
          DateLessThan = {
            "aws:CurrentTime" = local.wipe_expires_at
          }
        }
      },
      {
        Sid      = "NeverAnythingDated28OctoberOrLater"
        Effect   = "Deny"
        Action   = ["s3:*"]
        Resource = [for d in local.wipe_deny_dates : "${aws_s3_bucket.tv_cold.arn}/*/${d}*"]
      },
      {
        Sid    = "NeverDeploysLogsSebiOrKeepCopies"
        Effect = "Deny"
        Action = ["s3:*"]
        Resource = [
          "${aws_s3_bucket.tv_cold.arn}/deploys/*",
          "${aws_s3_bucket.tv_cold.arn}/sebi-preserve/*",
          "${aws_s3_bucket.tv_cold.arn}/logs/*",
          "${aws_s3_bucket.tv_cold.arn}/rebuild-2026-10-22/keep/*",
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
  description = "One-off Quotes 35 and 42 wipe role; removed after the run"
  value       = aws_iam_role.wipe_2026_10_31.arn
}
