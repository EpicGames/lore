# =============================================================================
# ECS on EC2 — c8gd.8xlarge with NVMe instance store for fragment caching
#
# This is the recommended deployment for Lore. The NVMe instance store provides
# sub-millisecond fragment reads for clones, while S3 provides durability.
# c8gd.8xlarge: 32 vCPU, 64 GB RAM, 1x 1.9 TB NVMe, 25 Gbps network.
# =============================================================================

data "aws_ssm_parameter" "ecs_ami" {
  name = "/aws/service/ecs/optimized-ami/amazon-linux-2023/arm64/recommended/image_id"
}

resource "aws_ecs_cluster" "this" {
  name = "${local.name}-cluster"

  setting {
    name  = "containerInsights"
    value = "enabled"
  }

  tags = local.tags
}

resource "aws_cloudwatch_log_group" "lore" {
  name              = "/ecs/${local.name}"
  retention_in_days = 7
  tags              = local.tags
}

# =============================================================================
# Launch Template + ASG — ECS-managed instances with NVMe setup
# =============================================================================

resource "aws_launch_template" "ecs" {
  name_prefix   = "${local.name}-ecs-"
  image_id      = data.aws_ssm_parameter.ecs_ami.value
  instance_type = var.instance_type

  iam_instance_profile {
    arn = aws_iam_instance_profile.ecs_instance.arn
  }

  vpc_security_group_ids = [aws_security_group.lore.id]

  user_data = base64encode(templatefile("${path.module}/user_data.sh.tpl", {
    cluster_name = aws_ecs_cluster.this.name
    mount_path   = "/srv/urc"
  }))

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  tag_specifications {
    resource_type = "instance"
    tags          = merge(local.tags, { Name = "${local.name}-ecs" })
  }

  tags = local.tags
}

resource "aws_autoscaling_group" "ecs" {
  name_prefix         = "${local.name}-ecs-"
  min_size            = 2
  max_size            = 2
  desired_capacity    = 2
  vpc_zone_identifier = aws_subnet.private[*].id

  launch_template {
    id      = aws_launch_template.ecs.id
    version = "$Latest"
  }

  # This example uses fixed capacity (min = max = desired), so no scale-in
  # events can occur and scale-in protection provides no value. See the
  # capacity note below for why there is no ECS capacity provider.
  protect_from_scale_in = false

  # Allows terraform destroy to delete the ASG without waiting for graceful
  # instance termination. Remove for production if you want graceful drain
  # before ASG deletion.
  force_delete = true

  tag {
    key                 = "Name"
    value               = "${local.name}-ecs"
    propagate_at_launch = true
  }

  lifecycle {
    ignore_changes = [desired_capacity]
  }
}

# =============================================================================
# Capacity — services use launch_type = "EC2" against the ASG directly.
#
# No capacity provider: this example runs fixed capacity (min = max = desired),
# so managed scaling, managed termination protection, and managed draining
# provide no value here — and their automation (per-instance scale-in
# protection, capacity reconciliation, termination lifecycle hooks) deadlocks
# `terraform destroy`, leaving ECS services stuck DRAINING past the provider's
# 20-minute timeout. If you adapt this example for dynamic scaling, add an
# aws_ecs_capacity_provider with managed scaling and switch the services from
# launch_type to a capacity_provider_strategy.
# =============================================================================

# =============================================================================
# Primary — Composite store (NVMe cache + durable S3), serves replication
# =============================================================================

resource "aws_ecs_task_definition" "lore" {
  family                   = local.name
  requires_compatibilities = ["EC2"]
  network_mode             = "awsvpc"
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.task.arn

  runtime_platform {
    operating_system_family = "LINUX"
    cpu_architecture        = "ARM64"
  }

  volume {
    name      = "instance-store-cache"
    host_path = "/srv/urc"
  }

  volume {
    name = "certs"
  }

  container_definitions = jsonencode([
    {
      name      = "init-certs"
      image     = "public.ecr.aws/amazonlinux/amazonlinux:minimal"
      essential = false
      command   = ["sh", "-c", "echo \"$CERT\" > /certs/fullchain.crt && echo \"$KEY\" > /certs/server.key && chmod 600 /certs/server.key && echo \"$CA\" > /certs/ca.pem"]

      secrets = [
        { name = "CERT", valueFrom = "${aws_secretsmanager_secret.tls.arn}:fullchain::" },
        { name = "KEY", valueFrom = "${aws_secretsmanager_secret.tls.arn}:key::" },
        { name = "CA", valueFrom = "${aws_secretsmanager_secret.tls.arn}:ca::" },
      ]

      mountPoints       = [{ sourceVolume = "certs", containerPath = "/certs", readOnly = false }]
      memoryReservation = 64

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.lore.name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "init"
        }
      }
    },
    {
      name      = "loreserver"
      image     = var.container_image
      essential = true

      dependsOn         = [{ containerName = "init-certs", condition = "SUCCESS" }]
      memoryReservation = 8192

      portMappings = [
        { containerPort = local.port_quic_grpc, protocol = "tcp" },
        { containerPort = local.port_quic_grpc, protocol = "udp" },
        { containerPort = local.port_http, protocol = "tcp" },
        { containerPort = local.port_replication, protocol = "udp" },
      ]

      mountPoints = [
        { sourceVolume = "instance-store-cache", containerPath = "/srv/urc", readOnly = false },
        { sourceVolume = "certs", containerPath = "/certs", readOnly = true },
      ]

      secrets = [
        { name = "LORE__SERVER__HTTP__PRESIGNED_URL_HMAC_KEY", valueFrom = aws_secretsmanager_secret.hmac.arn },
      ]

      environment = [
        { name = "LORE_ENV", value = "docker" },
        { name = "LORE_CONFIG_PATH", value = "/etc/lore/config" },

        # TLS
        { name = "LORE__SERVER__QUIC__CERTIFICATE__CERT_FILE", value = "/certs/fullchain.crt" },
        { name = "LORE__SERVER__QUIC__CERTIFICATE__PKEY_FILE", value = "/certs/server.key" },
        { name = "LORE__SERVER__GRPC__CERTIFICATE__CERT_FILE", value = "/certs/fullchain.crt" },
        { name = "LORE__SERVER__GRPC__CERTIFICATE__PKEY_FILE", value = "/certs/server.key" },

        # Internal QUIC for edge replication
        { name = "LORE__SERVER__QUIC_INTERNAL__ENABLED", value = "true" },
        { name = "LORE__SERVER__QUIC_INTERNAL__CERTIFICATE__CERT_FILE", value = "/certs/fullchain.crt" },
        { name = "LORE__SERVER__QUIC_INTERNAL__CERTIFICATE__PKEY_FILE", value = "/certs/server.key" },
        { name = "LORE__SERVER__QUIC_INTERNAL__VERIFY_CLIENT_CERTS", value = "false" },

        # Storage: composite (NVMe cache + S3 durable)
        { name = "LORE__IMMUTABLE_STORE__MODE", value = "composite" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__MODE", value = "local" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__PATH", value = "/srv/urc" },
        # 80% of c8gd.8xlarge NVMe (1.9 TB). Reserves 20% for xfs metadata/journal.
        # The fragment cache is the only consumer of the instance store.
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__MAX_SIZE", value = "1520000000000" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__FLUSH_DELAY_SECONDS", value = "10" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__MODE", value = "aws" },
        { name = "LORE__MUTABLE_STORE__MODE", value = "aws" },
        { name = "LORE__LOCK_STORE__MODE", value = "aws" },

        # AWS plugin config
        { name = "LORE__PLUGINS__AWS__IMMUTABLE_STORE__S3_BUCKET", value = aws_s3_bucket.fragments.id },
        { name = "LORE__PLUGINS__AWS__IMMUTABLE_STORE__DYNAMODB_FRAGMENTS_TABLE", value = aws_dynamodb_table.fragments.name },
        { name = "LORE__PLUGINS__AWS__IMMUTABLE_STORE__DYNAMODB_METADATA_TABLE", value = aws_dynamodb_table.metadata.name },
        { name = "LORE__PLUGINS__AWS__MUTABLE_STORE__DYNAMODB_TABLE", value = aws_dynamodb_table.mutable.name },
        { name = "LORE__PLUGINS__AWS__LOCK_STORE__DYNAMODB_TABLE", value = aws_dynamodb_table.locks.name },
      ]

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.lore.name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "lore"
        }
      }
    },
  ])

  tags = local.tags
}

resource "aws_ecs_service" "lore" {
  name            = local.name
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.lore.arn
  desired_count   = 1

  health_check_grace_period_seconds = 120

  launch_type = "EC2"

  network_configuration {
    subnets         = aws_subnet.private[*].id
    security_groups = [aws_security_group.lore.id]
  }

  service_registries {
    registry_arn = aws_service_discovery_service.lore.arn
  }

  placement_constraints {
    type = "distinctInstance"
  }

  tags = local.tags
}

# =============================================================================
# Cloud Map — Service discovery for edge → primary and client → edge
#
# NOTE: terraform destroy may fail if ECS tasks are still registered. If this
# happens, scale services to 0 and wait 30s before re-running destroy.
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

  health_check_custom_config {}

  tags = local.tags
}

resource "aws_service_discovery_service" "edge" {
  name = "edge"

  dns_config {
    namespace_id = aws_service_discovery_private_dns_namespace.this.id
    dns_records {
      ttl  = 10
      type = "A"
    }
    routing_policy = "MULTIVALUE"
  }

  health_check_custom_config {}

  tags = local.tags
}

# =============================================================================
# Edge — Composite store (NVMe cache + replicated durable via QUIC to primary)
# =============================================================================

resource "aws_ecs_task_definition" "edge" {
  family                   = "${local.name}-edge"
  requires_compatibilities = ["EC2"]
  network_mode             = "awsvpc"
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.edge_task.arn

  runtime_platform {
    operating_system_family = "LINUX"
    cpu_architecture        = "ARM64"
  }

  volume {
    name      = "instance-store-cache"
    host_path = "/srv/urc"
  }

  volume {
    name = "certs"
  }

  container_definitions = jsonencode([
    {
      name      = "init-certs"
      image     = "public.ecr.aws/amazonlinux/amazonlinux:minimal"
      essential = false
      command   = ["sh", "-c", "echo \"$CERT\" > /certs/fullchain.crt && echo \"$KEY\" > /certs/server.key && chmod 600 /certs/server.key && cat /etc/pki/tls/certs/ca-bundle.crt > /certs/ca.pem && echo \"$CA\" >> /certs/ca.pem"]

      secrets = [
        { name = "CERT", valueFrom = "${aws_secretsmanager_secret.tls.arn}:fullchain::" },
        { name = "KEY", valueFrom = "${aws_secretsmanager_secret.tls.arn}:key::" },
        { name = "CA", valueFrom = "${aws_secretsmanager_secret.tls.arn}:ca::" },
      ]

      mountPoints       = [{ sourceVolume = "certs", containerPath = "/certs", readOnly = false }]
      memoryReservation = 64

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.lore.name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "edge-init"
        }
      }
    },
    {
      name      = "loreserver"
      image     = var.container_image
      essential = true

      dependsOn         = [{ containerName = "init-certs", condition = "SUCCESS" }]
      memoryReservation = 8192

      portMappings = [
        { containerPort = local.port_quic_grpc, protocol = "tcp" },
        { containerPort = local.port_quic_grpc, protocol = "udp" },
        { containerPort = local.port_http, protocol = "tcp" },
      ]

      mountPoints = [
        { sourceVolume = "instance-store-cache", containerPath = "/srv/urc", readOnly = false },
        { sourceVolume = "certs", containerPath = "/certs", readOnly = true },
      ]

      secrets = [
        { name = "LORE__SERVER__HTTP__PRESIGNED_URL_HMAC_KEY", valueFrom = aws_secretsmanager_secret.hmac.arn },
      ]

      environment = [
        { name = "LORE_ENV", value = "docker" },
        { name = "LORE_CONFIG_PATH", value = "/etc/lore/config" },
        { name = "SSL_CERT_FILE", value = "/certs/ca.pem" },

        # TLS for client-facing endpoints
        { name = "LORE__SERVER__QUIC__CERTIFICATE__CERT_FILE", value = "/certs/fullchain.crt" },
        { name = "LORE__SERVER__QUIC__CERTIFICATE__PKEY_FILE", value = "/certs/server.key" },
        { name = "LORE__SERVER__GRPC__CERTIFICATE__CERT_FILE", value = "/certs/fullchain.crt" },
        { name = "LORE__SERVER__GRPC__CERTIFICATE__PKEY_FILE", value = "/certs/server.key" },

        # Storage: composite (NVMe cache + replicated durable via QUIC to primary)
        { name = "LORE__IMMUTABLE_STORE__MODE", value = "composite" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__MODE", value = "local" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__PATH", value = "/srv/urc" },
        # 80% of c8gd.8xlarge NVMe (1.9 TB). Reserves 20% for xfs metadata/journal.
        # The fragment cache is the only consumer of the instance store.
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__MAX_SIZE", value = "1520000000000" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__FLUSH_DELAY_SECONDS", value = "10" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__MODE", value = "replicated" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__REPLICATED__REMOTE_URL", value = "quics://primary.${local.name}.internal:${local.port_replication}" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__REPLICATED__PERIODIC_CLIENT_REFRESH_SECS", value = "180" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__REPLICATED__REGENERATE_RETRY__INITIAL_BACKOFF_MS", value = "100" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__REPLICATED__REGENERATE_RETRY__MAX_BACKOFF_MS", value = "1000" },
        { name = "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__REPLICATED__REGENERATE_RETRY__MAX_ATTEMPTS", value = "10" },

        # Branch resolution proxied to primary
        { name = "LORE__MUTABLE_STORE__MODE", value = "remote" },
        { name = "LORE__MUTABLE_STORE__REMOTE__REMOTE_URL", value = "lores://primary.${local.name}.internal:${local.port_quic_grpc}" },
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

      # Self-healing: if ReplicatedStore::new() blocks on cold deploy (primary
      # not yet accepting QUIC), the server never binds ports. After startPeriod
      # + retries, ECS replaces the task — by then the primary is warm.
      # Note: CMD-SHELL runs via /bin/sh; must invoke bash explicitly for /dev/tcp.
      healthCheck = {
        command     = ["CMD-SHELL", "/usr/bin/bash -c 'echo > /dev/tcp/localhost/${local.port_http}' 2>/dev/null"]
        startPeriod = 120
        interval    = 10
        timeout     = 5
        retries     = 6
      }
    },
  ])

  tags = local.tags
}

resource "aws_ecs_service" "edge" {
  name            = "${local.name}-edge"
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.edge.arn
  desired_count   = 1

  health_check_grace_period_seconds = 300

  launch_type = "EC2"

  network_configuration {
    subnets         = aws_subnet.private[*].id
    security_groups = [aws_security_group.lore.id]
  }

  service_registries {
    registry_arn = aws_service_discovery_service.edge.arn
  }

  placement_constraints {
    type = "distinctInstance"
  }

  depends_on = [aws_ecs_service.lore]

  tags = local.tags
}
