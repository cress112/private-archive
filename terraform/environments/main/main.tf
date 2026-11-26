locals {
  bucket_name = "cress-archived-data"
  role_name   = "private-archive-role"
  user_name   = "private-archive-source"
}

resource "aws_s3_bucket" "archive" {
  bucket        = local.bucket_name
  force_destroy = false
}

resource "aws_s3_bucket_public_access_block" "archive" {
  bucket                  = aws_s3_bucket.archive.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# A newly created S3 bucket defaults to unversioned. Do not issue
# PutBucketVersioning: Suspended is a different state from never-versioned.

resource "aws_s3_bucket_server_side_encryption_configuration" "archive" {
  bucket = aws_s3_bucket.archive.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "incomplete_uploads" {
  bucket = aws_s3_bucket.archive.id
  rule {
    id     = "abort-incomplete-multipart-after-one-day"
    status = "Enabled"
    filter {}
    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }
}

# This identity may only assume the archive role. Operators create its access
# keys in the AWS console; no access key or login profile is managed in state.
resource "aws_iam_user" "source" {
  name = local.user_name
}

data "aws_iam_policy_document" "archive_trust" {
  version = "2012-10-17"

  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]

    principals {
      type        = "AWS"
      identifiers = [aws_iam_user.source.arn]
    }
  }
}

resource "aws_iam_role" "archive" {
  name                 = local.role_name
  max_session_duration = 3600
  assume_role_policy   = data.aws_iam_policy_document.archive_trust.json
}

data "aws_iam_policy_document" "assume_archive" {
  version = "2012-10-17"

  statement {
    effect    = "Allow"
    actions   = ["sts:AssumeRole"]
    resources = [aws_iam_role.archive.arn]
  }
}

resource "aws_iam_user_policy" "assume_archive" {
  name   = "assume-private-archive-only"
  user   = aws_iam_user.source.name
  policy = data.aws_iam_policy_document.assume_archive.json
}

data "aws_iam_policy_document" "archive" {
  version = "2012-10-17"

  statement {
    effect    = "Allow"
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.archive.arn]
  }

  statement {
    effect = "Allow"
    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:DeleteObject",
      "s3:RestoreObject",
      "s3:AbortMultipartUpload",
    ]
    resources = ["${aws_s3_bucket.archive.arn}/*"]
  }
}

resource "aws_iam_role_policy" "archive" {
  name   = "archive-objects-only"
  role   = aws_iam_role.archive.id
  policy = data.aws_iam_policy_document.archive.json
}

output "bucket_name" { value = aws_s3_bucket.archive.bucket }
output "region" { value = "us-east-1" }
output "role_arn" { value = aws_iam_role.archive.arn }
output "source_user_name" { value = aws_iam_user.source.name }
