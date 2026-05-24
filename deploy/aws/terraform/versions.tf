# S7-Step2 / Phase 8.1: Terraform version + provider pins.
#
# Single region deployment: ap-south-1 (Mumbai) — required for
# low-latency access to NSE / Dhan API gateway.

terraform {
  required_version = ">= 1.9.0, < 2.0.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.80"
    }
  }

  # Remote state stored in S3, locked via DynamoDB. Bucket name is
  # injected at `terraform init` time via `-backend-config="bucket=..."`
  # (see .github/workflows/terraform-apply.yml) so the account-specific
  # bucket name doesn't have to live in the repo. The
  # scripts/aws-bootstrap-state-backend.sh script auto-creates the bucket
  # + DynamoDB lock table idempotently before init runs.
  backend "s3" {
    key            = "dlt/prod/terraform.tfstate"
    region         = "ap-south-1"
    encrypt        = true
    dynamodb_table = "tv-terraform-locks"
    # bucket = "tv-terraform-state-<account-id>" — injected via -backend-config
  }
}

provider "aws" {
  region = var.aws_region

  default_tags {
    tags = {
      Project     = "tickvault"
      Environment = var.environment
      ManagedBy   = "terraform"
      Repo        = "github.com/SJParthi/tickvault"
    }
  }
}
