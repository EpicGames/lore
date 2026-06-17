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

  service_registries {
    registry_arn = aws_service_discovery_service.lore.arn
  }

  tags = local.tags
}

# =============================================================================
# Cloud Map — Service discovery for edge → primary
# =============================================================================

resource "aws_service_discovery_private_dns_namespace" "this" {
  name = "${local.name}.internal"
  vpc  = aws_vpc.this.id
  tags = local.tags
}

resource "aws_service_discovery_service" "lore" {
  name = "primary"

  dns_config {
    namespace_id = aws_service_discovery_private_dns_namespace.this.id
    dns_records {
      ttl  = 10
      type = "A"
    }
    routing_policy = "MULTIVALUE"
  }

  health_check_custom_config {
    failure_threshold = 1
  }

  tags = local.tags
}

# =============================================================================
# Edge Pod — Caching node with replicated + remote stores
# =============================================================================

resource "aws_ecs_task_definition" "edge" {
  family                   = "${local.name}-edge"
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

      # Edge stores: replicated immutable (pulls from primary) + remote mutable (proxies to primary)
      { name = "LORE__IMMUTABLE_STORE__MODE", value = "replicated" },
      { name = "LORE__IMMUTABLE_STORE__REPLICATED__REMOTE_URL", value = "lore://primary.${local.name}.internal:${local.port_quic_grpc}" },
      { name = "LORE__IMMUTABLE_STORE__REPLICATED__PERIODIC_CLIENT_REFRESH_SECS", value = "300" },
      { name = "LORE__IMMUTABLE_STORE__REPLICATED__REGENERATE_RETRY__INITIAL_BACKOFF_MS", value = "100" },
      { name = "LORE__IMMUTABLE_STORE__REPLICATED__REGENERATE_RETRY__MAX_BACKOFF_MS", value = "5000" },
      { name = "LORE__IMMUTABLE_STORE__REPLICATED__REGENERATE_RETRY__MAX_ATTEMPTS", value = "10" },
      { name = "LORE__MUTABLE_STORE__MODE", value = "remote" },
      { name = "LORE__MUTABLE_STORE__REMOTE__REMOTE_URL", value = "lore://primary.${local.name}.internal:${local.port_quic_grpc}" },
      { name = "LORE__LOCK_STORE__MODE", value = "local" },
    ]

    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"         = aws_cloudwatch_log_group.lore.name
        "awslogs-region"        = var.region
        "awslogs-stream-prefix" = "edge"
      }
    }
  }])

  tags = local.tags
}

resource "aws_ecs_service" "edge" {
  name            = "${local.name}-edge"
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.edge.arn
  desired_count   = 1
  launch_type     = "FARGATE"

  network_configuration {
    subnets          = aws_subnet.private[*].id
    security_groups  = [aws_security_group.lore.id]
    assign_public_ip = false
  }

  depends_on = [aws_ecs_service.lore]

  tags = local.tags
}
