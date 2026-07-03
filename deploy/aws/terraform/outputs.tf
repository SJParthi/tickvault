# S7-Step2 / Phase 8.1: Terraform outputs.

output "instance_id" {
  description = "EC2 instance ID — use in aws ssm start-session --target"
  value       = aws_instance.tv_app.id
}

output "elastic_ip" {
  description = "Static public IP (register with Dhan IP whitelist) when enable_eip=true; otherwise the instance's auto-assigned public IP (changes on each stop/start — fine for the no-orders data-pull window)."
  value       = var.enable_eip ? aws_eip.tv_app[0].public_ip : aws_instance.tv_app.public_ip
}

output "vpc_id" {
  description = "VPC ID for troubleshooting"
  value       = aws_vpc.dlt.id
}

output "sns_topic_arn" {
  description = "SNS topic ARN for CloudWatch alarm targets"
  value       = aws_sns_topic.tv_alerts.arn
}

output "log_group_name" {
  description = "CloudWatch log group for systemd journal + app logs"
  value       = aws_cloudwatch_log_group.tv_app.name
}

output "cold_bucket_name" {
  description = "S3 cold-storage bucket for tick archival"
  value       = aws_s3_bucket.tv_cold.id
}

output "ssh_command" {
  description = "One-liner to SSH in after the instance is running"
  value       = "ssh -i ~/.ssh/${var.key_name}.pem ec2-user@${var.enable_eip ? aws_eip.tv_app[0].public_ip : aws_instance.tv_app.public_ip}"
}

output "ssm_session_command" {
  description = "One-liner to open a Session Manager shell (no SSH key needed)"
  value       = "aws ssm start-session --region ${var.aws_region} --target ${aws_instance.tv_app.id}"
}

output "dashboard_url" {
  description = "Direct URL to the CloudWatch operator dashboard (open in any browser)"
  value       = "https://${var.aws_region}.console.aws.amazon.com/cloudwatch/home?region=${var.aws_region}#dashboards/dashboard/${aws_cloudwatch_dashboard.operator.dashboard_name}"
}

output "portal_git_sha" {
  description = "B9 deploy provenance: git SHA of the repo tree this terraform state was last applied from (CI-set; \"unknown\" for local applies)"
  value       = var.portal_git_sha
}

output "binary_git_sha_ssm_param" {
  description = "B9 deploy provenance: SSM param holding the git SHA of the last successfully deployed binary (written by deploy-aws.yml after a verified swap)"
  value       = "/tickvault/${var.environment}/deploy/binary-git-sha"
}
