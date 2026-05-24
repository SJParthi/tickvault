# Terraform S3 Backend Bootstrap

> **Status:** Opt-in. First `terraform apply` uses LOCAL state file
> (`terraform.tfstate` in this directory). That file lives on your Mac.
> If your Mac SSD fails OR you delete the folder, the state is lost and
> `terraform plan` will think every resource needs creation — leading
> to duplicate VPCs, duplicate EIPs (= new Dhan IP registration = 7-day
> cooldown trap).
>
> **Migrating to S3 backend protects against that.** Run these steps
> AFTER first `terraform apply` succeeds, BEFORE you trust the state
> for anything operational.

## Why S3 + DynamoDB

| Component | Role |
|---|---|
| S3 bucket | Stores the Terraform state file (durable, versioned) |
| DynamoDB table | State lock (prevents 2 simultaneous `terraform apply` from corrupting state) |

## One-time setup

```bash
# Pick a globally-unique bucket name (must include your AWS account ID)
ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
BUCKET="tv-terraform-state-${ACCOUNT_ID}"
LOCK_TABLE="tv-terraform-locks"
REGION="ap-south-1"

# 1. Create the S3 bucket (private, versioned, encrypted)
aws s3api create-bucket \
  --bucket "${BUCKET}" \
  --region "${REGION}" \
  --create-bucket-configuration LocationConstraint=${REGION}

aws s3api put-bucket-versioning \
  --bucket "${BUCKET}" \
  --versioning-configuration Status=Enabled

aws s3api put-bucket-encryption \
  --bucket "${BUCKET}" \
  --server-side-encryption-configuration \
  '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"}}]}'

aws s3api put-public-access-block \
  --bucket "${BUCKET}" \
  --public-access-block-configuration \
  BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true

# 2. Create the DynamoDB lock table (on-demand pricing, ~₹0 for our usage)
aws dynamodb create-table \
  --region "${REGION}" \
  --table-name "${LOCK_TABLE}" \
  --attribute-definitions AttributeName=LockID,AttributeType=S \
  --key-schema AttributeName=LockID,KeyType=HASH \
  --billing-mode PAY_PER_REQUEST

# Wait for the table to become ACTIVE (~30s)
aws dynamodb wait table-exists --region "${REGION}" --table-name "${LOCK_TABLE}"

# 3. Edit versions.tf — uncomment the backend "s3" block and fill in:
#    bucket         = "${BUCKET}"     # the exact value from above
#    key            = "dlt/prod/terraform.tfstate"
#    region         = "${REGION}"
#    encrypt        = true
#    dynamodb_table = "${LOCK_TABLE}"

# 4. Migrate state
terraform init -migrate-state
# When asked "Do you want to copy existing state to the new backend?" — type yes

# 5. Verify
terraform state list   # should show all your resources
aws s3 ls s3://${BUCKET}/dlt/prod/    # should show terraform.tfstate
```

After step 4 you can delete the local `terraform.tfstate` and
`terraform.tfstate.backup` files. The state lives in S3 forever, locked
during apply, versioned for rollback.

## What this protects against

| Disaster | Without S3 backend | With S3 backend |
|---|---|---|
| Mac SSD failure | All state lost → next apply creates DUPLICATE VPC/EIP/etc. → 7-day Dhan IP cooldown | State intact in S3 → new Mac runs `terraform init` → resumes seamlessly |
| Two operators run `terraform apply` simultaneously | State file corruption | DynamoDB lock blocks the second apply |
| Accidental `terraform destroy` | No undo | S3 versioning preserves prior state |
| Code review needs to see "what would change" | Reviewer can't run plan without local state | Reviewer runs `terraform plan` against shared S3 state |

## Cost

| Component | Free tier? | Monthly cost |
|---|---|---|
| S3 bucket | ✅ Up to 5 GB free | ₹0 (state file is ~50 KB) |
| DynamoDB on-demand | ✅ 25 RCU + 25 WCU free | ₹0 (lock table sees ~10 operations/month) |
| **Total** | | **₹0** |
