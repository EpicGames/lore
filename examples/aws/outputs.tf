output "cluster_name" {
  description = "ECS cluster name"
  value       = aws_ecs_cluster.this.name
}

output "service_name" {
  description = "ECS service name"
  value       = aws_ecs_service.lore.name
}

output "s3_bucket" {
  description = "S3 bucket for fragment storage"
  value       = aws_s3_bucket.fragments.id
}

output "log_group" {
  description = "CloudWatch log group"
  value       = aws_cloudwatch_log_group.lore.name
}
