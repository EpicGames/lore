variable "container_image" {
  description = "Loreserver container image URI in ECR"
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
