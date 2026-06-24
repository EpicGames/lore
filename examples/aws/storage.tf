# =============================================================================
# S3 — Fragment payloads (immutable store)
# =============================================================================

# force_destroy defaults to false — the bucket cannot be destroyed with data inside.
# For dev/test teardown, set force_destroy = true or empty the bucket before destroy.
resource "aws_s3_bucket" "fragments" {
  bucket_prefix = "${local.name}-fragments-"
  tags          = local.tags
}

resource "aws_s3_bucket_versioning" "fragments" {
  bucket = aws_s3_bucket.fragments.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "fragments" {
  bucket = aws_s3_bucket.fragments.id
  rule {
    apply_server_side_encryption_by_default { sse_algorithm = "AES256" }
  }
}

resource "aws_s3_bucket_public_access_block" "fragments" {
  bucket                  = aws_s3_bucket.fragments.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_lifecycle_configuration" "fragments" {
  bucket = aws_s3_bucket.fragments.id

  rule {
    id     = "abort-incomplete-multipart"
    status = "Enabled"
    filter {}
    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }
}

# =============================================================================
# DynamoDB — Fragment associations
# Key schema from lore-aws/src/store/immutable_store.rs
# =============================================================================

resource "aws_dynamodb_table" "fragments" {
  name         = "${local.name}-fragments"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "hash"
  range_key    = "repository_context"

  attribute {
    name = "hash"
    type = "B"
  }
  attribute {
    name = "repository_context"
    type = "B"
  }

  point_in_time_recovery { enabled = true }

  tags = local.tags
}

# =============================================================================
# DynamoDB — Fragment metadata (hash-only key, no sort key)
# Key schema from lore-aws/src/store/immutable_store.rs
# =============================================================================

resource "aws_dynamodb_table" "metadata" {
  name         = "${local.name}-metadata"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "hash"

  attribute {
    name = "hash"
    type = "B"
  }

  point_in_time_recovery { enabled = true }

  tags = local.tags
}

# =============================================================================
# DynamoDB — Mutable store (branch pointers)
# Key schema from lore-aws/src/store/mutable_store.rs
# =============================================================================

resource "aws_dynamodb_table" "mutable" {
  name         = "${local.name}-mutable"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "repository_id"
  range_key    = "key"

  attribute {
    name = "repository_id"
    type = "B"
  }
  attribute {
    name = "key"
    type = "B"
  }

  point_in_time_recovery { enabled = true }

  tags = local.tags
}

# =============================================================================
# DynamoDB — Distributed locks
# Key schema + GSIs from lore-aws/src/store/lock_store.rs
# =============================================================================

# NOTE: Table-level hash_key/range_key emits a deprecation warning suggesting key_schema,
# but key_schema blocks don't exist at the table level in the provider schema (only in GSIs).
# The warning is premature — no migration path exists yet for table primary keys.

# Deletion protection disabled for teardown convenience.
# Production: add deletion_protection_enabled = true to each table.

resource "aws_dynamodb_table" "locks" {
  name         = "${local.name}-locks"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "hash"
  range_key    = "repositoryBranch"

  attribute {
    name = "hash"
    type = "B"
  }
  attribute {
    name = "repositoryBranch"
    type = "B"
  }
  attribute {
    name = "ownerId"
    type = "S"
  }
  attribute {
    name = "repository"
    type = "B"
  }
  attribute {
    name = "branch"
    type = "B"
  }
  attribute {
    name = "description"
    type = "S"
  }

  global_secondary_index {
    name            = "owner-repo-branch"
    projection_type = "ALL"

    key_schema {
      attribute_name = "ownerId"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "repositoryBranch"
      key_type       = "RANGE"
    }
  }

  global_secondary_index {
    name            = "repo-branch"
    projection_type = "ALL"

    key_schema {
      attribute_name = "repository"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "branch"
      key_type       = "RANGE"
    }
  }

  global_secondary_index {
    name            = "repo-branch-description"
    projection_type = "ALL"

    key_schema {
      attribute_name = "repositoryBranch"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "description"
      key_type       = "RANGE"
    }
  }

  point_in_time_recovery { enabled = true }

  tags = local.tags
}
