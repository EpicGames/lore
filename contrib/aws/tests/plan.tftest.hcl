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
  container_image = "123456789012.dkr.ecr.us-west-2.amazonaws.com/loreserver:v0.8.7"
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
    condition     = aws_dynamodb_table.fragment_state.hash_key == "hash"
    error_message = "Fragment state table hash key must be 'hash'"
  }

  assert {
    condition     = aws_dynamodb_table.fragment_state.range_key == null
    error_message = "Fragment state table must have no range key"
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

# Container definitions are unknown until apply, so the primary's environment cannot be
# asserted here — the lookup these runs check is what feeds it.
run "no_fragment_metadata_table_by_default" {
  command = plan

  assert {
    condition     = length(data.aws_dynamodb_table.fragment_metadata) == 0
    error_message = "A new deployment must not look up a fragment metadata table"
  }

  assert {
    condition     = output.fragment_metadata_table == null
    error_message = "A new deployment must not configure the primary with a fragment metadata table"
  }
}

run "existing_fragment_metadata_table_adopted" {
  command = plan

  variables {
    fragment_metadata_table = "lore-metadata"
  }

  assert {
    condition     = data.aws_dynamodb_table.fragment_metadata[0].name == "lore-metadata"
    error_message = "The existing table must be looked up, not created"
  }

  assert {
    condition     = output.fragment_metadata_table == "lore-metadata"
    error_message = "Setting fragment_metadata_table must configure the primary to read it"
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
