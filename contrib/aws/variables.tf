variable "container_image" {
  description = "Loreserver container image URI (linux/arm64). Must be v0.8.3 or later, built from lore-server/Dockerfile."
  type        = string
}

variable "allowed_cidrs" {
  description = "CIDR blocks allowed to connect to Lore (e.g., your VPN or office IP)"
  type        = list(string)
}

variable "region" {
  description = "AWS region"
  type        = string
  default     = "us-west-2"
}

variable "name" {
  description = "Name prefix for all resources"
  type        = string
  default     = "lore"
}

variable "instance_type" {
  description = "EC2 instance type for ECS. c8gd.8xlarge recommended: 32 vCPU, 64 GB, 1.9 TB NVMe, 25 Gbps network."
  type        = string
  default     = "c8gd.8xlarge"
}
