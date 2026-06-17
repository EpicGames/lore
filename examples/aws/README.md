# Lore on AWS

Terraform configuration that deploys a Lore server on AWS with durable S3/DynamoDB storage using ECS Fargate.

## What this creates

- VPC with public and private subnets (2 AZs)
- S3 bucket for fragment storage (immutable store)
- 4 DynamoDB tables (fragments, metadata, mutable store, locks)
- ECS Fargate service running the loreserver container
- VPC endpoints for S3 and DynamoDB (reduces NAT costs)
- CloudWatch log group

## Prerequisites

- [Terraform](https://developer.hashicorp.com/terraform/install) >= 1.5
- AWS credentials configured (`aws configure` or environment variables)
- A `loreserver` container image in ECR — build from the repo root:

```sh
docker build -f lore-server/Dockerfile -t loreserver .

aws ecr get-login-password --region us-west-2 | docker login --username AWS --password-stdin <ACCOUNT_ID>.dkr.ecr.us-west-2.amazonaws.com
aws ecr create-repository --repository-name loreserver --region us-west-2
docker tag loreserver:latest <ACCOUNT_ID>.dkr.ecr.us-west-2.amazonaws.com/loreserver:latest
docker push <ACCOUNT_ID>.dkr.ecr.us-west-2.amazonaws.com/loreserver:latest
```

## Deploy

```sh
cd examples/aws
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars — set your container_image URI and allowed_cidrs
terraform init
terraform apply
```

## Connect

Get the task IP (Fargate assigns a private IP in the VPC):

```sh
TASK_ARN=$(aws ecs list-tasks --cluster lore-cluster --service-name lore --query 'taskArns[0]' --output text)
TASK_IP=$(aws ecs describe-tasks --cluster lore-cluster --tasks "$TASK_ARN" \
  --query 'tasks[0].attachments[0].details[?name==`privateIPv4Address`].value' --output text)
echo "$TASK_IP"
```

The server generates an ephemeral self-signed certificate on startup. For local testing, skip TLS verification or use `lore://` (plain gRPC, QUIC still has TLS):

```sh
lore clone lore://${TASK_IP}:41337/my-repo
```

For production, configure real TLS certificates (see Customize below) and use `lores://`.

## Verify

Check the service is running:

```sh
aws ecs describe-services --cluster lore-cluster --services lore \
  --query 'services[0].{status:status,running:runningCount}'
```

Check server logs:

```sh
aws logs tail /ecs/lore --since 5m
```

## Customize

This example uses the simplest viable configuration. For production:

- **TLS** — mount real certificates and set `LORE__SERVER__QUIC__CERTIFICATE__CERT_FILE` / `PKEY_FILE` (and the same for `GRPC`). See [Server configuration reference](https://epicgames.github.io/lore/reference/lore-server-config/#certificate-block).
- **Auth** — configure `LORE__SERVER__AUTH__JWK__ENDPOINT` to validate JWTs. See [Authentication](https://epicgames.github.io/lore/reference/lore-server-config/#authentication).
- **Caching** — switch from Fargate to EC2 with NVMe instances and use `LORE__IMMUTABLE_STORE__MODE=composite` for a local cache in front of S3.
- **Replication** — add edge nodes with `LORE__IMMUTABLE_STORE__MODE=replicated` for multi-region. See [Topology](https://epicgames.github.io/lore/reference/lore-server-config/#topology-settings).
- **HMAC** — set `LORE__SERVER__HTTP__PRESIGNED_URL_HMAC_KEY` (hex, ≥32 bytes) to enable presigned URLs for direct client-to-S3 transfers.

## Destroy

```sh
terraform destroy
```

Teardown takes 2–3 minutes (VPC, NAT gateway deletion).
