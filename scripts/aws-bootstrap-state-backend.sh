#!/usr/bin/env bash
# aws-bootstrap-state-backend.sh — idempotent S3 state bucket + DynamoDB lock table.
#
# Called by .github/workflows/terraform-apply.yml at the start of every
# run. Safe to re-execute: each step checks existence first, only creates
# if missing.
#
# Usage:
#   bash scripts/aws-bootstrap-state-backend.sh <aws_account_id> <aws_region>
#
# Resources created (each idempotent):
#   1. S3 bucket  tv-terraform-state-<account_id>  — versioned, encrypted,
#                                                    public-blocked
#   2. DynamoDB   tv-terraform-locks                — PAY_PER_REQUEST,
#                                                    LockID hash key
#
# Cost: free tier (S3 ~50 KB state file, DynamoDB ~10 ops/month).

set -euo pipefail

ACCOUNT_ID="${1:-}"
REGION="${2:-}"

if [ -z "${ACCOUNT_ID}" ] || [ -z "${REGION}" ]; then
  echo "Usage: $0 <aws_account_id> <aws_region>" >&2
  exit 1
fi

BUCKET="tv-terraform-state-${ACCOUNT_ID}"
LOCK_TABLE="tv-terraform-locks"

log() {
  echo "[aws-bootstrap-state-backend] $*"
}

# ─────────────────────────────────────────────────────────────────
# 1. S3 bucket
# ─────────────────────────────────────────────────────────────────
log "Checking S3 bucket ${BUCKET}..."
if aws s3api head-bucket --bucket "${BUCKET}" --region "${REGION}" 2>/dev/null; then
  log "S3 bucket ${BUCKET} already exists — skipping create"
else
  log "Creating S3 bucket ${BUCKET} in ${REGION}..."
  # us-east-1 is the only region where LocationConstraint must be omitted;
  # all other regions (including ap-south-1) require it.
  if [ "${REGION}" = "us-east-1" ]; then
    aws s3api create-bucket --bucket "${BUCKET}" --region "${REGION}"
  else
    aws s3api create-bucket \
      --bucket "${BUCKET}" \
      --region "${REGION}" \
      --create-bucket-configuration "LocationConstraint=${REGION}"
  fi
  log "S3 bucket created"
fi

log "Enabling versioning..."
aws s3api put-bucket-versioning \
  --bucket "${BUCKET}" \
  --versioning-configuration Status=Enabled

log "Enabling AES256 encryption at rest..."
aws s3api put-bucket-encryption \
  --bucket "${BUCKET}" \
  --server-side-encryption-configuration \
  '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"}}]}'

log "Blocking all public access..."
aws s3api put-public-access-block \
  --bucket "${BUCKET}" \
  --public-access-block-configuration \
  BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true

# ─────────────────────────────────────────────────────────────────
# 2. DynamoDB lock table
# ─────────────────────────────────────────────────────────────────
log "Checking DynamoDB table ${LOCK_TABLE}..."
if aws dynamodb describe-table --table-name "${LOCK_TABLE}" --region "${REGION}" >/dev/null 2>&1; then
  log "DynamoDB table ${LOCK_TABLE} already exists — skipping create"
else
  log "Creating DynamoDB table ${LOCK_TABLE}..."
  aws dynamodb create-table \
    --region "${REGION}" \
    --table-name "${LOCK_TABLE}" \
    --attribute-definitions AttributeName=LockID,AttributeType=S \
    --key-schema AttributeName=LockID,KeyType=HASH \
    --billing-mode PAY_PER_REQUEST

  log "Waiting for table to become ACTIVE (up to ~30s)..."
  aws dynamodb wait table-exists --region "${REGION}" --table-name "${LOCK_TABLE}"
  log "DynamoDB lock table ready"
fi

log "Bootstrap complete. State bucket: ${BUCKET}, lock table: ${LOCK_TABLE}"
