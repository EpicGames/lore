# =============================================================================
# IAM — ECS task role (S3 + DynamoDB access) and execution role (ECR + logs)
# =============================================================================

# Task role — what the loreserver container can do
resource "aws_iam_role" "task" {
  name_prefix = "${local.name}-task-"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action    = "sts:AssumeRole"
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
    }]
  })
  tags = local.tags
}

resource "aws_iam_role_policy" "task_s3" {
  name_prefix = "s3-"
  role        = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:DeleteObjectVersion",
        "s3:ListBucket",
        "s3:ListBucketVersions",
      ]
      Resource = [
        aws_s3_bucket.fragments.arn,
        "${aws_s3_bucket.fragments.arn}/*",
      ]
    }]
  })
}

resource "aws_iam_role_policy" "task_dynamodb" {
  name_prefix = "dynamodb-"
  role        = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "dynamodb:GetItem",
        "dynamodb:PutItem",
        "dynamodb:DeleteItem",
        "dynamodb:Query",
        "dynamodb:BatchGetItem",
        "dynamodb:DescribeTable",
        "dynamodb:TransactWriteItems",
      ]
      Resource = [
        aws_dynamodb_table.fragments.arn,
        aws_dynamodb_table.metadata.arn,
        aws_dynamodb_table.mutable.arn,
        aws_dynamodb_table.locks.arn,
        "${aws_dynamodb_table.locks.arn}/index/*",
      ]
    }]
  })
}

# Execution role — what ECS needs to start the task (pull image, write logs, read secrets)
resource "aws_iam_role" "execution" {
  name_prefix = "${local.name}-exec-"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action    = "sts:AssumeRole"
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
    }]
  })
  tags = local.tags
}

resource "aws_iam_role_policy_attachment" "execution_ecr" {
  role       = aws_iam_role.execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}
