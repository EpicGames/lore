# Lore on AWS

Terraform configuration that deploys a Lore server on AWS with durable S3/DynamoDB storage using ECS Fargate.

> Region is configurable via `var.region` (default: `us-west-2`).

## What this creates

- VPC with public and private subnets (2 AZs)
- S3 bucket for fragment storage (immutable store)
- 4 DynamoDB tables (fragments, metadata, mutable store, locks)
- ECS Fargate primary service with S3/DynamoDB storage
- ECS Fargate edge service with replicated storage (caches from primary)
- Cloud Map private DNS for edge → primary service discovery
- Self-signed TLS CA + server certificate (inter-node trust)
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

The Dockerfile builds the `loreserver` binary from the workspace, which includes
the `lore-aws` crate. The server's `main()` calls `register_all_plugins()` at
startup, registering the AWS (S3 + DynamoDB) and HashiCorp (Consul) plugins
automatically. No custom binary or fork is needed.

## Deploy

```sh
cd examples/aws
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars — set your container_image URI and allowed_cidrs
terraform init
terraform apply
```

## Connect

The ECS services run in private subnets. Connect from within the VPC
(e.g., an EC2 instance, VPN, or AWS Client VPN).

Export the CA certificate so the client trusts the server's QUIC endpoint:

```sh
terraform output -raw ca_certificate_pem > lore-ca.pem
export SSL_CERT_FILE=lore-ca.pem
```

Clients connect to the **edge** service (it replicates from the primary
automatically). Get the edge task IP:

```sh
TASK_ARN=$(aws ecs list-tasks --cluster lore-cluster --service-name lore-edge --query 'taskArns[0]' --output text)
TASK_IP=$(aws ecs describe-tasks --cluster lore-cluster --tasks "$TASK_ARN" \
  --query 'tasks[0].attachments[0].details[?name==`privateIPv4Address`].value' --output text)
echo "$TASK_IP"
```

From a host inside the VPC:

```sh
lore clone lore://${TASK_IP}:41337/my-repo
```

> `lore://` uses QUIC (TLS) for data and plain gRPC for the control plane.
> The edge pod's gRPC is not TLS-configured, so `lore://` works directly.
> For `lores://` (gRPC+TLS), configure certificates on the edge pod (see Customize).

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

- **Ingress** — add an NLB, AWS Client VPN, or bastion host for access from outside the VPC.
- **TLS** — mount real certificates and set `LORE__SERVER__QUIC__CERTIFICATE__CERT_FILE` / `PKEY_FILE` (and the same for `GRPC`). See [Server configuration reference](https://epicgames.github.io/lore/reference/lore-server-config/#certificate-block).
- **Auth** — configure `LORE__SERVER__AUTH__JWK__ENDPOINT` to validate JWTs. See [Authentication](https://epicgames.github.io/lore/reference/lore-server-config/#authentication).
- **Caching** — switch from Fargate to EC2 with NVMe instances and use `LORE__IMMUTABLE_STORE__MODE=composite` for a local cache in front of S3.
- **Replication** — add more edge nodes or deploy to other regions. See [Topology](https://epicgames.github.io/lore/reference/lore-server-config/#topology-settings).
- **HMAC** — set `LORE__SERVER__HTTP__PRESIGNED_URL_HMAC_KEY` (hex, ≥32 bytes) to enable presigned URLs for direct client-to-S3 transfers.

## Destroy

```sh
terraform destroy
```

Teardown includes VPC and NAT gateway deletion.
