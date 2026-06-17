# =============================================================================
# ECS Cluster + Fargate Service
# =============================================================================

resource "aws_ecs_cluster" "this" {
  name = "${local.name}-cluster"
  tags = local.tags
}

resource "aws_cloudwatch_log_group" "lore" {
  name              = "/ecs/${local.name}"
  retention_in_days = 7
  tags              = local.tags
}

resource "aws_ecs_task_definition" "lore" {
  family                   = local.name
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = "1024"
  memory                   = "2048"
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.task.arn

  container_definitions = jsonencode([{
    name      = "loreserver"
    image     = var.container_image
    essential = true

    portMappings = [
      { containerPort = local.port_quic_grpc, protocol = "tcp" },
      { containerPort = local.port_quic_grpc, protocol = "udp" },
      { containerPort = local.port_http, protocol = "tcp" },
    ]

    environment = [
      { name = "LORE_ENV", value = "docker" },
      { name = "LORE_CONFIG_PATH", value = "/etc/lore/config" },

      # Storage: S3 + DynamoDB via the aws plugin
      { name = "LORE__IMMUTABLE_STORE__MODE", value = "aws" },
      { name = "LORE__MUTABLE_STORE__MODE", value = "aws" },
      { name = "LORE__LOCK_STORE__MODE", value = "aws" },

      # AWS plugin config — resource names from Terraform
      { name = "LORE__PLUGINS__AWS__IMMUTABLE_STORE__S3_BUCKET", value = aws_s3_bucket.fragments.id },
      { name = "LORE__PLUGINS__AWS__IMMUTABLE_STORE__DYNAMODB_FRAGMENTS_TABLE", value = aws_dynamodb_table.fragments.name },
      { name = "LORE__PLUGINS__AWS__IMMUTABLE_STORE__DYNAMODB_METADATA_TABLE", value = aws_dynamodb_table.metadata.name },
      { name = "LORE__PLUGINS__AWS__MUTABLE_STORE__DYNAMODB_TABLE", value = aws_dynamodb_table.mutable.name },
      { name = "LORE__PLUGINS__AWS__LOCK_STORE__DYNAMODB_TABLE", value = aws_dynamodb_table.locks.name },
    ]

    # TLS: The server generates an ephemeral self-signed certificate when no
    # certificate is configured. For production, mount real certs and set:
    #   LORE__SERVER__QUIC__CERTIFICATE__CERT_FILE=/certs/cert.pem
    #   LORE__SERVER__QUIC__CERTIFICATE__PKEY_FILE=/certs/key.pem
    #   LORE__SERVER__GRPC__CERTIFICATE__CERT_FILE=/certs/cert.pem
    #   LORE__SERVER__GRPC__CERTIFICATE__PKEY_FILE=/certs/key.pem

    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"         = aws_cloudwatch_log_group.lore.name
        "awslogs-region"        = var.region
        "awslogs-stream-prefix" = "lore"
      }
    }
  }])

  tags = local.tags
}

resource "aws_ecs_service" "lore" {
  name            = local.name
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.lore.arn
  desired_count   = 1
  launch_type     = "FARGATE"

  network_configuration {
    subnets          = aws_subnet.private[*].id
    security_groups  = [aws_security_group.lore.id]
    assign_public_ip = false
  }

  tags = local.tags
}
