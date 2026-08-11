# Plan-level validation — runs without AWS credentials.
# Catches stale resource schemas, broken variable wiring, and
# Terraform/provider version incompatibilities.
#
# Run: terraform test

mock_provider "aws" {}
mock_provider "tls" {}
mock_provider "random" {}

override_data {
  target = data.aws_availability_zones.available
  values = {
    names = ["us-west-2a", "us-west-2b"]
  }
}

override_data {
  target = data.aws_ssm_parameter.ecs_ami
  values = {
    value = "ami-0123456789abcdef0"
  }
}

variables {
  container_image = "123456789012.dkr.ecr.us-west-2.amazonaws.com/loreserver:v0.8.3"
  allowed_cidrs   = ["10.0.0.0/8"]
  region          = "us-west-2"
  name            = "lore"
}

run "cluster_and_services_configured" {
  command = plan

  assert {
    condition     = aws_ecs_cluster.this.name == "lore-cluster"
    error_message = "Cluster name should be 'lore-cluster'"
  }

  assert {
    condition     = aws_ecs_service.lore.name == "lore"
    error_message = "Primary service name should be 'lore'"
  }

  assert {
    condition     = aws_ecs_service.edge.name == "lore-edge"
    error_message = "Edge service name should be 'lore-edge'"
  }
}

run "storage_schemas_correct" {
  command = plan

  assert {
    condition     = aws_dynamodb_table.fragments.hash_key == "hash"
    error_message = "Fragments table hash key must be 'hash'"
  }

  assert {
    condition     = aws_dynamodb_table.fragments.range_key == "repository_context"
    error_message = "Fragments table range key must be 'repository_context'"
  }

  assert {
    condition     = aws_dynamodb_table.metadata.hash_key == "hash"
    error_message = "Metadata table hash key must be 'hash'"
  }

  assert {
    condition     = aws_dynamodb_table.mutable.hash_key == "repository_id"
    error_message = "Mutable table hash key must be 'repository_id'"
  }

  assert {
    condition     = aws_dynamodb_table.locks.hash_key == "hash"
    error_message = "Locks table hash key must be 'hash'"
  }

  assert {
    condition     = aws_dynamodb_table.locks.range_key == "repositoryBranch"
    error_message = "Locks table range key must be 'repositoryBranch'"
  }
}

run "service_discovery_configured" {
  command = plan

  assert {
    condition     = aws_service_discovery_private_dns_namespace.this.name == "lore.internal"
    error_message = "Cloud Map namespace should be 'lore.internal'"
  }

  assert {
    condition     = aws_service_discovery_service.lore.name == "primary"
    error_message = "Cloud Map service name should be 'primary'"
  }
}

run "ec2_infrastructure_configured" {
  command = plan

  assert {
    condition     = aws_launch_template.ecs.instance_type == "c8gd.8xlarge"
    error_message = "Launch template should use c8gd.8xlarge"
  }

  assert {
    condition     = aws_autoscaling_group.ecs.min_size == 2
    error_message = "ASG min size should be 2 (primary + edge)"
  }
}
