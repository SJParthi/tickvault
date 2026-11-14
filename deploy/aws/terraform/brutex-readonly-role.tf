# Read-only role for Brutex (owner Quote 60, 2026-11-14, card "Role your Mac
# login switches into (Recommended)").
#
# Brutex reads the daily export and the archive from the owner's Mac by
# switching into this role from the owner's existing login (one profile line
# in ~/.aws/config: role_arn = the brutex_readonly_role_arn output,
# source_profile = the owner's login). No user, password or access key is
# created anywhere.
#
# Trust: this account only (its root, so only identities this account's own
# IAM policies allow to assume it). Permissions: list and read three prefixes
# of the cold bucket, nothing else: no write, no delete, no tagging, no ACL,
# no policy, no other bucket. Pinned by
# crates/common/tests/brutex_readonly_role_guard.rs. The bucket uses the
# default S3-managed encryption, so no KMS grant is needed.

resource "aws_iam_role" "brutex_readonly" {
  name                 = "tv-${var.environment}-brutex-readonly"
  description          = "Brutex read-only access to the cold bucket's export, instrument snapshots and raw frames (owner Quote 60)"
  max_session_duration = 43200
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { AWS = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:root" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "brutex_readonly" {
  name = "tv-${var.environment}-brutex-readonly"
  role = aws_iam_role.brutex_readonly.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "ListThreePrefixes"
        Effect   = "Allow"
        Action   = ["s3:ListBucket"]
        Resource = "arn:aws:s3:::tv-${var.environment}-cold"
        Condition = {
          StringLike = {
            "s3:prefix" = ["questdb-partitions/*", "instrument-snapshots/*", "raw-frames/*"]
          }
        }
      },
      {
        Sid    = "ReadThreePrefixes"
        Effect = "Allow"
        Action = ["s3:GetObject", "s3:GetObjectAttributes"]
        Resource = [
          "arn:aws:s3:::tv-${var.environment}-cold/questdb-partitions/*",
          "arn:aws:s3:::tv-${var.environment}-cold/instrument-snapshots/*",
          "arn:aws:s3:::tv-${var.environment}-cold/raw-frames/*",
        ]
      },
    ]
  })
}
