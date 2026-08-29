variable "container_image" {
  description = "Loreserver container image URI (linux/arm64). Must be v0.8.7 or later — the release that reads the fragment state table this example creates — built from lore-server/Dockerfile."
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

variable "fragment_metadata_table" {
  description = "Name of an EXISTING DynamoDB fragment metadata table, read only for objects stored before fragment metadata moved onto the S3 object. Nothing here creates this table. Leave null on a new deployment — that declares no such object exists."
  type        = string
  default     = null
}

variable "instance_type" {
  description = "EC2 instance type for ECS. c8gd.8xlarge recommended: 32 vCPU, 64 GB, 1.9 TB NVMe, 25 Gbps network."
  type        = string
  default     = "c8gd.8xlarge"
}
