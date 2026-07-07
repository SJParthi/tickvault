# S8 / Phase 8.2 follow-up: GitHub Actions OIDC role for deploy-aws.yml
#
# Creates an IAM role that GitHub Actions can assume via OIDC (no
# long-lived credentials stored as repository secrets).
#
# The deploy-aws workflow references this role via `secrets.AWS_ROLE_ARN`.
# After `terraform apply`, copy the `github_oidc_role_arn` output into
# the repo's GitHub Actions secrets (Settings > Secrets and variables >
# Actions > New repository secret > name: AWS_ROLE_ARN).
#
# How it works:
#   1. GitHub's OIDC provider issues a short-lived JWT for every workflow run
#   2. The JWT contains claims like `repo:SJParthi/tickvault:ref:...`
#   3. This role's trust policy says "assume me ONLY if the claim matches
#      our repo + the workflow is deploy-aws.yml"
#   4. The workflow uses aws-actions/configure-aws-credentials@v4 to swap
#      the JWT for temporary AWS credentials (max 1 hour)
#
# Security properties:
#   - No long-lived AWS access keys in GitHub secrets
#   - Credentials expire within 1 hour
#   - Only the deploy-aws workflow can assume this role (subject match)
#   - Only on the main branch or v*.*.* tags (ref match)

variable "github_repo_full_name" {
  description = "GitHub repo full name, e.g. SJParthi/tickvault"
  type        = string
  default     = "SJParthi/tickvault"
}

# ---------------------------------------------------------------------------
# OIDC identity provider (one per account, reused across all repos)
# ---------------------------------------------------------------------------
#
# Managed as a `resource` (not `data`) so the first `terraform apply` in a
# fresh AWS account creates it. If the account ALREADY has this provider
# from prior setup, the operator must `terraform import` it once:
#
#   terraform import aws_iam_openid_connect_provider.github \
#     arn:aws:iam::<ACCOUNT_ID>:oidc-provider/token.actions.githubusercontent.com
#
# AWS auto-validates the thumbprint against the real GitHub cert chain
# (since 2023), so the explicit thumbprint below is a stable well-known
# value but is not security-critical.

resource "aws_iam_openid_connect_provider" "github" {
  url             = "https://token.actions.githubusercontent.com"
  client_id_list  = ["sts.amazonaws.com"]
  thumbprint_list = ["6938fd4d98bab03faadb97b34396831e3780aea1"]
}

# ---------------------------------------------------------------------------
# IAM role assumable ONLY by the specific GitHub workflow on main / tags
# ---------------------------------------------------------------------------

resource "aws_iam_role" "github_deploy" {
  name = "tv-${var.environment}-github-deploy"

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
        }
        StringLike = {
          # Restrict to our repo + main branch OR v*.*.* release tags.
          # The sub claim format is:
          #   repo:<org>/<repo>:ref:refs/heads/<branch>
          #   repo:<org>/<repo>:ref:refs/tags/<tag>
          "token.actions.githubusercontent.com:sub" = [
            "repo:${var.github_repo_full_name}:ref:refs/heads/main",
            "repo:${var.github_repo_full_name}:ref:refs/tags/v*",
            "repo:${var.github_repo_full_name}:environment:prod"
          ]
        }
      }
    }]
  })
}

# ---------------------------------------------------------------------------
# Policy: only the exact actions the deploy workflow needs
# ---------------------------------------------------------------------------

resource "aws_iam_role_policy" "github_deploy" {
  name = "tv-${var.environment}-github-deploy-policy"
  role = aws_iam_role.github_deploy.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # SSM RunCommand on the specific instance
        Effect = "Allow"
        Action = [
          "ssm:SendCommand",
          "ssm:GetCommandInvocation",
          "ssm:StartSession",
        ]
        Resource = [
          aws_instance.tv_app.arn,
          "arn:aws:ssm:${var.aws_region}::document/AWS-RunShellScript",
          "arn:aws:ssm:${var.aws_region}:*:session/*"
        ]
      },
      {
        # S3 upload of the binary artifact
        Effect = "Allow"
        Action = [
          "s3:PutObject",
          "s3:GetObject",
          "s3:ListBucket",
        ]
        Resource = [
          aws_s3_bucket.tv_cold.arn,
          "${aws_s3_bucket.tv_cold.arn}/deploys/*"
        ]
      },
      {
        # SNS publish for success/failure notifications
        Effect = "Allow"
        Action = [
          "sns:Publish",
        ]
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        # B9 deploy provenance: after a VERIFIED swap, deploy-aws.yml
        # records the deployed binary's git SHA to this one parameter.
        # Read back by the operator-portal footer + the deploy-watchdog
        # binary-vs-main mismatch metric. Scoped to exactly this ARN.
        Effect   = "Allow"
        Action   = ["ssm:PutParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/deploy/binary-git-sha"
      },
      {
        # NSE-holiday intentional-stop marker (2026-07-07 round-3 review
        # fix): aws-autopilot.sh reads /tickvault/<env>/holiday-stop-date
        # (stamped by deploy/aws/holiday-gate.sh before its holiday
        # self-stop) so its every-15-min up-window self-start never fights
        # the intentional holiday stop — the pre-fix boot/stop war could
        # bracket the 09:20 IST alarm-gate sample and restore the holiday
        # false page. Read-only, scoped to exactly that one parameter.
        Effect   = "Allow"
        Action   = ["ssm:GetParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/holiday-stop-date"
      },
      {
        # LOG-INGESTION-SMOKE (deploy-aws.yml): after the verified swap, the
        # workflow polls filter-log-events on the app log group to prove the
        # CloudWatch agent is actually shipping fresh log events (incident
        # 2026-07-06: app healthy, shipper silently dead after the
        # data/logs/machine/ reorg). Non-fatal check; least privilege —
        # scoped to the app log group + its streams only.
        Effect   = "Allow"
        Action   = ["logs:FilterLogEvents"]
        Resource = "${aws_cloudwatch_log_group.tv_app.arn}:*"
      },
      {
        # EC2 describe — read-only, used by the workflow to fetch
        # the instance state during the post-deploy monitor
        Effect = "Allow"
        Action = [
          "ec2:DescribeInstances",
          "ec2:DescribeInstanceStatus",
        ]
        Resource = "*"
      }
    ]
  })
}

# ---------------------------------------------------------------------------
# Outputs — copy these into GitHub repository secrets
# ---------------------------------------------------------------------------

output "github_oidc_role_arn" {
  description = "Copy this into GitHub repo secret AWS_ROLE_ARN"
  value       = aws_iam_role.github_deploy.arn
}

output "github_oidc_setup_instructions" {
  description = "Paste these commands into the repo secrets UI"
  value       = <<-EOT
    1. Open https://github.com/${var.github_repo_full_name}/settings/secrets/actions
    2. Click 'New repository secret'
    3. Create 3 secrets:
       AWS_ROLE_ARN    = ${aws_iam_role.github_deploy.arn}
       AWS_ACCOUNT_ID  = <run 'aws sts get-caller-identity --query Account --output text'>
       EC2_INSTANCE_ID = ${aws_instance.tv_app.id}
    4. Open 'Variables' tab, create:
       AWS_REGION      = ${var.aws_region}
  EOT
}
