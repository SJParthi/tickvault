# S7-Step2 / Phase 8.1: Terraform outputs.

output "instance_id" {
  description = "EC2 instance ID — use in aws ssm start-session --target"
  value       = aws_instance.dlt_app.id
}

output "elastic_ip" {
  description = "Static public IP — register this with Dhan IP whitelist"
  value       = aws_eip.dlt_app.public_ip
}

output "vpc_id" {
  description = "VPC ID for troubleshooting"
  value       = aws_vpc.dlt.id
}

output "sns_topic_arn" {
  description = "SNS topic ARN for CloudWatch alarm targets"
  value       = aws_sns_topic.dlt_alerts.arn
}

output "log_group_name" {
  description = "CloudWatch log group for systemd journal + app logs"
  value       = aws_cloudwatch_log_group.dlt_app.name
}

output "cold_bucket_name" {
  description = "S3 cold-storage bucket for tick archival"
  value       = aws_s3_bucket.dlt_cold.id
}

output "ssh_command" {
  description = "One-liner to SSH in after the instance is running"
  value       = "ssh -i ~/.ssh/${var.key_name}.pem ubuntu@${aws_eip.dlt_app.public_ip}"
}

output "ssm_session_command" {
  description = "One-liner to open a Session Manager shell (no SSH key needed)"
  value       = "aws ssm start-session --region ${var.aws_region} --target ${aws_instance.dlt_app.id}"
}
