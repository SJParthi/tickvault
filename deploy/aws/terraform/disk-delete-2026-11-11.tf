# ---------------------------------------------------------------------------
# ONE-OFF (owner Quote 56, 2026-11-11, card "One-off, these 4 only
# (Recommended)"): a role that can delete ONLY the four Quote 52 leftovers,
# assumable ONLY by the GitHub Actions environment `disk-delete-2026-11-11`
# (.github/workflows/disk-delete-2026-11-11.yml). Every statement expires at
# 2026-11-14 00:00 UTC. This file, the workflow and the Rust tool are deleted
# in the first PR after the run; Quote 52's REJECT on delete permissions then
# binds again unchanged. Never add another id, and never the live root
# vol-07a6586a56466ea83.
# ---------------------------------------------------------------------------

locals {
  disk_delete_2026_11_11_until = "2026-11-14T00:00:00Z"
}

resource "aws_iam_role" "disk_delete_2026_11_11" {
  name                 = "tv-${var.environment}-disk-delete-2026-11-11"
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
          "token.actions.githubusercontent.com:sub" = "repo:${var.github_repo_full_name}:environment:disk-delete-2026-11-11"
        }
        DateLessThan = { "aws:CurrentTime" = local.disk_delete_2026_11_11_until }
      }
    }]
  })

  tags = {
    Name    = "tv-${var.environment}-disk-delete-2026-11-11"
    Purpose = "one-off delete of the four Quote 52 leftovers - Quote 56"
  }
}

resource "aws_iam_role_policy" "disk_delete_2026_11_11" {
  name = "tv-${var.environment}-disk-delete-2026-11-11-policy"
  role = aws_iam_role.disk_delete_2026_11_11.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid    = "ReadToCheckState"
        Effect = "Allow"
        Action = ["ec2:DescribeVolumes", "ec2:DescribeSnapshots"]
        # Describe actions take no resource-level permission.
        Resource  = "*"
        Condition = { DateLessThan = { "aws:CurrentTime" = local.disk_delete_2026_11_11_until } }
      },
      {
        Sid    = "DeleteTheTwoLeftoverVolumesOnly"
        Effect = "Allow"
        Action = ["ec2:DeleteVolume"]
        Resource = [
          "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:volume/vol-0c6ab6e593e39d8c8",
          "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:volume/vol-073ccaa417a0f344b",
        ]
        Condition = { DateLessThan = { "aws:CurrentTime" = local.disk_delete_2026_11_11_until } }
      },
      {
        Sid    = "DeleteTheTwoLeftoverSnapshotsOnly"
        Effect = "Allow"
        Action = ["ec2:DeleteSnapshot"]
        Resource = [
          "arn:aws:ec2:${var.aws_region}::snapshot/snap-090ed9c4f3df0ca61",
          "arn:aws:ec2:${var.aws_region}::snapshot/snap-0573ab07252f67bf3",
        ]
        Condition = { DateLessThan = { "aws:CurrentTime" = local.disk_delete_2026_11_11_until } }
      },
    ]
  })
}
