# Lore on AWS

Deploy a Lore server on AWS with durable S3/DynamoDB storage and an edge node for client access.

> Region is configurable via `var.region` (default: `us-west-2`).

## Quick start

### 1. Build and push the container image

From the Lore repo root:

```sh
docker build -f lore-server/Dockerfile -t loreserver .
```

Push to ECR (replace `<ACCOUNT_ID>` and `<REGION>`):

```sh
aws ecr get-login-password --region <REGION> | docker login --username AWS --password-stdin <ACCOUNT_ID>.dkr.ecr.<REGION>.amazonaws.com
aws ecr create-repository --repository-name loreserver --region <REGION>
docker tag loreserver:latest <ACCOUNT_ID>.dkr.ecr.<REGION>.amazonaws.com/loreserver:latest
docker push <ACCOUNT_ID>.dkr.ecr.<REGION>.amazonaws.com/loreserver:latest
```

### 2. Deploy

```sh
cd examples/aws
cp terraform.tfvars.example terraform.tfvars
```

Edit `terraform.tfvars`:

```hcl
region          = "us-west-2"
container_image = "<ACCOUNT_ID>.dkr.ecr.us-west-2.amazonaws.com/loreserver:latest"
allowed_cidrs   = ["10.0.0.0/8"]  # Your VPC or VPN CIDR
```

```sh
terraform init
terraform apply
```

### 3. Connect

The services run in private subnets — connect from within the VPC (EC2 instance, VPN, or Client VPN).

```sh
# Export the CA so the client trusts the server
terraform output -raw ca_certificate_pem > lore-ca.pem
export SSL_CERT_FILE=lore-ca.pem

# Get the edge node IP
TASK_ARN=$(aws ecs list-tasks --cluster lore-cluster --service-name lore-edge --query 'taskArns[0]' --output text)
TASK_IP=$(aws ecs describe-tasks --cluster lore-cluster --tasks "$TASK_ARN" \
  --query 'tasks[0].attachments[0].details[?name==`privateIPv4Address`].value' --output text)

# Clone a repository
lore clone lore://${TASK_IP}:41337/my-repo
```

## What gets deployed

| Component | Purpose |
|-----------|---------|
| Primary (ECS Fargate) | Stores fragments in S3 and metadata in DynamoDB |
| Edge (ECS Fargate) | Client-facing node that replicates from primary |
| Cloud Map DNS | Edge → primary service discovery |
| VPC | Private subnets, NAT, S3/DynamoDB gateway endpoints |
| TLS CA | Self-signed; establishes trust between nodes |

## Verify

```sh
aws ecs describe-services --cluster lore-cluster --services lore lore-edge \
  --query 'services[].{name:serviceName,running:runningCount}'
```

```sh
aws logs tail /ecs/lore --since 5m
```

## Customize

| Need | What to change |
|------|----------------|
| External access | Add an NLB or AWS Client VPN |
| gRPC TLS for clients | Configure edge certificates, use `lores://` |
| Authentication | Set `LORE__SERVER__AUTH__JWK__ENDPOINT` ([docs](https://epicgames.github.io/lore/reference/lore-server-config/#authentication)) |
| NVMe caching | Switch to EC2, use `composite` store mode |
| More edge nodes | Duplicate the edge service definition |
| Presigned URLs | Set `LORE__SERVER__HTTP__PRESIGNED_URL_HMAC_KEY` (hex, ≥32 bytes) |

Full server configuration: [Lore Server config reference](https://epicgames.github.io/lore/reference/lore-server-config/)

## Destroy

```sh
terraform destroy
```

## Prerequisites

- [Terraform](https://developer.hashicorp.com/terraform/install) >= 1.5
- AWS credentials with VPC, ECS, S3, DynamoDB, IAM, Secrets Manager, Cloud Map permissions
- Docker (to build the container image)
