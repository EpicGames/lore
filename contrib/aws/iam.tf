# =============================================================================
# IAM — EC2 instance role, ECS task roles, execution role
# =============================================================================

# EC2 instance role — ECS agent needs to communicate with the ECS API
resource "aws_iam_role" "ecs_instance" {
  name_prefix = "${local.name}-instance-"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action    = "sts:AssumeRole"
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
    }]
  })
  tags = local.tags
}

resource "aws_iam_role_policy_attachment" "ecs_instance_role" {
  role       = aws_iam_role.ecs_instance.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonEC2ContainerServiceforEC2Role"
}

resource "aws_iam_role_policy_attachment" "ecs_instance_ssm" {
  role       = aws_iam_role.ecs_instance.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_instance_profile" "ecs_instance" {
  name_prefix = "${local.name}-instance-"
  role        = aws_iam_role.ecs_instance.name
  tags        = local.tags
}

# Primary task role — S3 + DynamoDB access for durable storage
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

# Edge task role — intentionally empty. Edge proxies all storage operations
# through the primary via gRPC/QUIC, so it needs no direct S3 or DynamoDB access.
resource "aws_iam_role" "edge_task" {
  name_prefix = "${local.name}-edge-task-"
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

# Execution role — ECS agent pulls images, writes logs, reads secrets
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

resource "aws_iam_role_policy" "execution_secrets" {
  name_prefix = "secrets-"
  role        = aws_iam_role.execution.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["secretsmanager:GetSecretValue"]
      Resource = [aws_secretsmanager_secret.tls.arn, aws_secretsmanager_secret.hmac.arn]
    }]
  })
}

# =============================================================================
# HMAC Key — presigned URL feature for fragment transfer between nodes
# =============================================================================

resource "random_id" "hmac" {
  byte_length = 32
}

resource "aws_secretsmanager_secret" "hmac" {
  name_prefix = "${local.name}-hmac-"
  tags        = local.tags
}

resource "aws_secretsmanager_secret_version" "hmac" {
  secret_id     = aws_secretsmanager_secret.hmac.id
  secret_string = random_id.hmac.hex
}
