# =============================================================================
# VPC — minimal 2-AZ layout with public + private subnets
# =============================================================================

resource "aws_vpc" "this" {
  cidr_block           = "10.0.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true
  tags                 = merge(local.tags, { Name = "${local.name}-vpc" })
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id
  tags   = merge(local.tags, { Name = "${local.name}-igw" })
}

resource "aws_subnet" "public" {
  count                   = 2
  vpc_id                  = aws_vpc.this.id
  cidr_block              = cidrsubnet(aws_vpc.this.cidr_block, 8, count.index)
  availability_zone       = data.aws_availability_zones.available.names[count.index]
  map_public_ip_on_launch = true
  tags                    = merge(local.tags, { Name = "${local.name}-public-${count.index}" })
}

resource "aws_subnet" "private" {
  count             = 2
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(aws_vpc.this.cidr_block, 8, count.index + 10)
  availability_zone = data.aws_availability_zones.available.names[count.index]
  tags              = merge(local.tags, { Name = "${local.name}-private-${count.index}" })
}

resource "aws_eip" "nat" {
  domain = "vpc"
  tags   = merge(local.tags, { Name = "${local.name}-nat-eip" })
}

resource "aws_nat_gateway" "this" {
  allocation_id = aws_eip.nat.id
  subnet_id     = aws_subnet.public[0].id
  tags          = merge(local.tags, { Name = "${local.name}-nat" })
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id
  tags   = merge(local.tags, { Name = "${local.name}-public-rt" })
}

resource "aws_route" "public_internet" {
  route_table_id         = aws_route_table.public.id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = aws_internet_gateway.this.id
}

resource "aws_route_table_association" "public" {
  count          = 2
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table" "private" {
  vpc_id = aws_vpc.this.id
  tags   = merge(local.tags, { Name = "${local.name}-private-rt" })
}

resource "aws_route" "private_nat" {
  route_table_id         = aws_route_table.private.id
  destination_cidr_block = "0.0.0.0/0"
  nat_gateway_id         = aws_nat_gateway.this.id
}

resource "aws_route_table_association" "private" {
  count          = 2
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private.id
}

# =============================================================================
# Security Group — Lore server
# =============================================================================

resource "aws_security_group" "lore" {
  name_prefix = "${local.name}-server-"
  description = "Lore server ports"
  vpc_id      = aws_vpc.this.id
  tags        = merge(local.tags, { Name = "${local.name}-server-sg" })

  lifecycle { create_before_destroy = true }
}

# Client access: QUIC (UDP) + gRPC (TCP) on 41337
resource "aws_vpc_security_group_ingress_rule" "client_quic" {
  for_each          = toset(var.allowed_cidrs)
  security_group_id = aws_security_group.lore.id
  from_port         = local.port_quic_grpc
  to_port           = local.port_quic_grpc
  ip_protocol       = "udp"
  cidr_ipv4         = each.value
  description       = "Lore client (QUIC)"
}

resource "aws_vpc_security_group_ingress_rule" "client_grpc" {
  for_each          = toset(var.allowed_cidrs)
  security_group_id = aws_security_group.lore.id
  from_port         = local.port_quic_grpc
  to_port           = local.port_quic_grpc
  ip_protocol       = "tcp"
  cidr_ipv4         = each.value
  description       = "Lore client (gRPC)"
}

# HTTP health checks + presigned URLs
resource "aws_vpc_security_group_ingress_rule" "client_http" {
  for_each          = toset(var.allowed_cidrs)
  security_group_id = aws_security_group.lore.id
  from_port         = local.port_http
  to_port           = local.port_http
  ip_protocol       = "tcp"
  cidr_ipv4         = each.value
  description       = "Lore client (HTTP)"
}

# Internal: QUIC replication (edge → primary on 41340 UDP)
resource "aws_vpc_security_group_ingress_rule" "replication_quic" {
  security_group_id            = aws_security_group.lore.id
  from_port                    = 41340
  to_port                      = 41340
  ip_protocol                  = "udp"
  referenced_security_group_id = aws_security_group.lore.id
  description                  = "Lore replication (QUIC)"
}

# Internal: gRPC (edge → primary on 41337 TCP for remote mutable store)
resource "aws_vpc_security_group_ingress_rule" "internal_grpc" {
  security_group_id            = aws_security_group.lore.id
  from_port                    = 41337
  to_port                      = 41337
  ip_protocol                  = "tcp"
  referenced_security_group_id = aws_security_group.lore.id
  description                  = "Lore branch resolution (gRPC)"
}

# Internal: QUIC (edge → primary on 41337 UDP for replicated immutable store)
resource "aws_vpc_security_group_ingress_rule" "internal_quic" {
  security_group_id            = aws_security_group.lore.id
  from_port                    = 41337
  to_port                      = 41337
  ip_protocol                  = "udp"
  referenced_security_group_id = aws_security_group.lore.id
  description                  = "Lore data transfer (QUIC)"
}

resource "aws_vpc_security_group_egress_rule" "all" {
  security_group_id = aws_security_group.lore.id
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
  description       = "All outbound"
}

# =============================================================================
# VPC Endpoints — S3 and DynamoDB (avoid NAT costs for AWS API traffic)
# =============================================================================

resource "aws_vpc_endpoint" "s3" {
  vpc_id          = aws_vpc.this.id
  service_name    = "com.amazonaws.${var.region}.s3"
  route_table_ids = [aws_route_table.private.id]
  tags            = merge(local.tags, { Name = "${local.name}-s3-endpoint" })
}

resource "aws_vpc_endpoint" "dynamodb" {
  vpc_id          = aws_vpc.this.id
  service_name    = "com.amazonaws.${var.region}.dynamodb"
  route_table_ids = [aws_route_table.private.id]
  tags            = merge(local.tags, { Name = "${local.name}-dynamodb-endpoint" })
}
