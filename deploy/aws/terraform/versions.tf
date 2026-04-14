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

  # Remote state stored in S3 after first apply.
  # Uncomment and fill in after running `terraform init` with local state,
  # creating the bucket, then migrating with `terraform init -migrate-state`.
  #
  # backend "s3" {
  #   bucket         = "dlt-terraform-state-<your-account-id>"
  #   key            = "dlt/prod/terraform.tfstate"
  #   region         = "ap-south-1"
  #   encrypt        = true
  #   dynamodb_table = "dlt-terraform-locks"
  # }
}

provider "aws" {
  region = var.aws_region

  default_tags {
    tags = {
      Project     = "dhan-live-trader"
      Environment = var.environment
      ManagedBy   = "terraform"
      Repo        = "github.com/SJParthi/dhan-live-trader"
    }
  }
}
