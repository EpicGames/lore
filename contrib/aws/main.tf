provider "aws" {
  region = var.region
}

locals {
  name = var.name
  tags = { ManagedBy = "terraform", Project = "lore" }

  # Ports — match lore-server/config/default.toml
  port_quic_grpc   = 41337 # QUIC (UDP) + gRPC (TCP)
  port_http        = 41339 # Health checks, presigned URLs
  port_replication = 41340 # QUIC internal replication (UDP)
}

data "aws_availability_zones" "available" {
  state = "available"
}
