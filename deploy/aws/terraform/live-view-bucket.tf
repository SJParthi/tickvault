# Plan PR MV (2026-11-06 live incident): the live-view bucket.
#
# Measured live 2026-11-06: every portal Stocks movers click took 2.6-8.0 s
# because it went portal -> Lambda -> SSM -> box helper -> API -> QuestDB,
# though the database part was 0.1-0.4 s. The app now publishes the movers
# universe here (crates/app/src/movers_universe_publisher.rs, every 5 s in
# session, 60 s otherwise) and the operator-control Lambda reads it and
# selects in memory (crates/aws-lambdas/src/operator_stock_movers.rs); a
# missing or stale object falls back to the SSM path.
#
# Deliberately NOT tv-<env>-cold: that bucket is versioned and may never
# expire anything (operator Quotes 27 + 28), so a 5-second overwrite there
# would keep every version forever. This bucket holds a disposable view:
# no versioning, objects expire after 2 days, public access blocked, SSE-S3,
# TLS only. Cost (Assumed, not billed yet): ~108,000 PUTs a month, ~$0.55.
#
# IAM: the instance role may PutObject on the one key only (main.tf); the
# operator-control Lambda may GetObject on the one key only
# (operator-control-lambda.tf). Shape pinned by
# crates/common/tests/live_view_bucket_guard.rs.

resource "aws_s3_bucket" "tv_live_view" {
  bucket = "tv-${var.environment}-live-view"
}

resource "aws_s3_bucket_public_access_block" "tv_live_view" {
  bucket                  = aws_s3_bucket.tv_live_view.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "tv_live_view" {
  bucket = aws_s3_bucket.tv_live_view.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

# A view, not a record: anything left here expires after 2 days.
resource "aws_s3_bucket_lifecycle_configuration" "tv_live_view" {
  bucket = aws_s3_bucket.tv_live_view.id

  rule {
    id     = "live-view-expire"
    status = "Enabled"

    filter {}

    expiration {
      days = 2
    }
  }
}

data "aws_iam_policy_document" "tv_live_view_tls_only" {
  statement {
    sid     = "DenyNonTls"
    effect  = "Deny"
    actions = ["s3:*"]
    resources = [
      aws_s3_bucket.tv_live_view.arn,
      "${aws_s3_bucket.tv_live_view.arn}/*",
    ]

    principals {
      type        = "*"
      identifiers = ["*"]
    }

    condition {
      test     = "Bool"
      variable = "aws:SecureTransport"
      values   = ["false"]
    }
  }
}

resource "aws_s3_bucket_policy" "tv_live_view" {
  bucket = aws_s3_bucket.tv_live_view.id
  policy = data.aws_iam_policy_document.tv_live_view_tls_only.json

  # The public access block must exist before a bucket policy is attached.
  depends_on = [aws_s3_bucket_public_access_block.tv_live_view]
}
