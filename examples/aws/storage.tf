# =============================================================================
# S3 — Fragment payloads (immutable store)
# =============================================================================

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

  tags = local.tags
}

# =============================================================================
# DynamoDB — Distributed locks
# Key schema + GSIs from lore-aws/src/store/lock_store.rs
# =============================================================================

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
    hash_key        = "ownerId"
    range_key       = "repositoryBranch"
    projection_type = "ALL"
  }

  global_secondary_index {
    name            = "repo-branch"
    hash_key        = "repository"
    range_key       = "branch"
    projection_type = "ALL"
  }

  global_secondary_index {
    name            = "repo-branch-description"
    hash_key        = "repositoryBranch"
    range_key       = "description"
    projection_type = "ALL"
  }

  tags = local.tags
}
