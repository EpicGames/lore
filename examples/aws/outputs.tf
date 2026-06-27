output "cluster_name" {
  description = "ECS cluster name"
  value       = aws_ecs_cluster.this.name
}

output "service_name" {
  description = "ECS service name (primary)"
  value       = aws_ecs_service.lore.name
}

output "edge_service_name" {
  description = "ECS service name (edge)"
  value       = aws_ecs_service.edge.name
}

output "primary_dns" {
  description = "Cloud Map DNS for primary (used by edge pods)"
  value       = "primary.${aws_service_discovery_private_dns_namespace.this.name}"
}

output "edge_dns" {
  description = "Cloud Map DNS for edge (used by clients)"
  value       = "edge.${aws_service_discovery_private_dns_namespace.this.name}"
}

output "s3_bucket" {
  description = "S3 bucket for fragment storage"
  value       = aws_s3_bucket.fragments.id
}

output "log_group" {
  description = "CloudWatch log group"
  value       = aws_cloudwatch_log_group.lore.name
}

output "ca_certificate_pem" {
  description = "CA certificate — clients need this to trust the server's TLS cert"
  value       = local.ca_pem
}
