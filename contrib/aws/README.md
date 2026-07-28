# Lore on AWS

Deploy Lore on AWS with NVMe-cached edge nodes for high-throughput game asset delivery.

This example uses **c8gd.8xlarge** Graviton instances (32 vCPU, 64 GB RAM, 1.9 TB NVMe, 25 Gbps network) — the recommended instance type for Lore. The NVMe instance store serves as a local fragment cache, delivering sub-millisecond reads for `lore clone` while S3 provides durable storage.

> Region is configurable via `var.region` (default: `us-west-2`).

## Quick start

### 1. Build and push the container image

From the Lore repo root:

```sh
docker buildx build --platform linux/arm64 -f lore-server/Dockerfile -t loreserver:v0.8.3 --load .
```

> If building on an x86 host, [register QEMU](https://docs.docker.com/build/building/multi-platform/#qemu) first:
> `docker run --rm --privileged multiarch/qemu-user-static --reset -p yes`

Push to ECR (replace `<ACCOUNT_ID>` and `<REGION>`):

```sh
aws ecr get-login-password --region <REGION> | docker login --username AWS --password-stdin <ACCOUNT_ID>.dkr.ecr.<REGION>.amazonaws.com
aws ecr create-repository --repository-name loreserver --region <REGION>
docker tag loreserver:v0.8.3 <ACCOUNT_ID>.dkr.ecr.<REGION>.amazonaws.com/loreserver:v0.8.3
docker push <ACCOUNT_ID>.dkr.ecr.<REGION>.amazonaws.com/loreserver:v0.8.3
```

### 2. Deploy

```sh
cd contrib/aws
cp terraform.tfvars.example terraform.tfvars
```

Edit `terraform.tfvars`:

```hcl
region          = "us-west-2"
container_image = "<ACCOUNT_ID>.dkr.ecr.us-west-2.amazonaws.com/loreserver:v0.8.3"
allowed_cidrs   = ["10.0.0.0/8"]  # Your VPC or VPN CIDR
```

```sh
terraform init
terraform apply
```

First apply may need a second run (DynamoDB PITR timing race).

### 3. Connect

Services run in private subnets. Access requires connectivity to the VPC (e.g., NLB in public subnets, AWS Client VPN, VPC peering, or a bastion host).

Export the CA certificate so the Lore client trusts the server:

```sh
terraform output -raw ca_certificate_pem > lore-ca.pem
cat /etc/ssl/certs/ca-certificates.crt lore-ca.pem > combined-ca.pem
export SSL_CERT_FILE=combined-ca.pem
```

Create a repository and push your first asset:

```sh
lore repository create lores://edge.lore.internal:41337/my-game
lore clone lores://edge.lore.internal:41337/my-game ./my-game
cp /path/to/assets/* ./my-game/
cd my-game
lore stage .
lore commit "initial import"
lore push
```

Clone from another machine:

```sh
lore clone lores://edge.lore.internal:41337/my-game ./my-game
```

**Note:** `edge.lore.internal` resolves via Cloud Map private DNS inside the VPC.

## Architecture

| Component | Instance | Purpose |
|-----------|----------|---------|
| Primary (ECS on EC2) | c8gd.8xlarge | Composite store: NVMe cache + S3 durable. Serves replication to edge. |
| Edge (ECS on EC2) | c8gd.8xlarge | Composite store: NVMe cache + replicated durable (QUIC to primary). Client-facing. |
| Cloud Map DNS | — | Service discovery (`primary.lore.internal`, `edge.lore.internal`) |
| VPC | — | Private subnets, NAT, S3/DynamoDB gateway endpoints |
| TLS CA | — | Self-signed; establishes trust between nodes and clients |

**Startup:** Health check grace periods allow the primary (120s) and edge (300s) to initialize without being marked unhealthy. The edge's retry configuration handles Cloud Map DNS propagation delays automatically. On first deploy, edge nodes may restart 1-2 times while DNS propagates — this is expected and self-resolving.

**Edge limitations:** The edge uses `remote` mutable store mode (proxies to primary). Administrative commands like `lore repository create` and `lore repository list` must go to the primary directly. All client-facing operations (clone, push, branch, merge, sync) work through the edge.

### Data flow

```
Client ──lores://──→ Edge (NVMe cache hit → instant response)
                         │ cache miss
                         ├──quics://41340───→ Primary (NVMe cache → S3 fallback)
                         └──lores://41337──→ Primary (branch resolution)
```

> **Instance sizing:** Use node sizes without network bandwidth caps (32+ vCPU) for production. This example uses c8gd.8xlarge (NVMe + Graviton).

## Verify

Check services are running:

```sh
aws ecs describe-services --cluster lore-cluster --services lore lore-edge \
  --query 'services[].{name:serviceName,running:runningCount}' --region us-west-2
```

Check server logs:

```sh
aws logs tail /ecs/lore --since 5m --region us-west-2
```

## Testing

This example ships with a test plan and an automated e2e script in `tests/`:

| File | Purpose |
|------|---------|
| `tests/plan.tftest.hcl` | Terraform plan-level validation (offline, free) |
| `tests/e2e-test.sh` | Automated end-to-end validation (requires live infrastructure) |
| `tests/TEST_PLAN.md` | Manual runbook — step-by-step deployment + validation |

The e2e test mirrors the [official Lore quickstart](https://epicgames.github.io/lore/tutorials/quickstart/) against this infrastructure, exercising all client operations through both the primary and edge:

```sh
./tests/e2e-test.sh us-west-2
```

What it validates:
- Health checks (HTTP 200 from primary and edge)
- Full data path via primary (create → stage → commit → push → clone)
- Full client workflow via edge (clone → branch → merge → push → sync)
- Data integrity (MD5 checksums match across all paths)

See `tests/TEST_PLAN.md` for the manual equivalent with AWS resource validation.

## Customize

| Need | What to change |
|------|----------------|
| Smaller instances (dev/test) | Set `instance_type = "c8gd.xlarge"` — same architecture, less capacity |
| External access | Add an NLB in public subnets |
| Authentication | Set `LORE__SERVER__AUTH__JWK__ENDPOINT` ([docs](https://epicgames.github.io/lore/reference/lore-server-config/#authentication)) |
| More edge nodes | Increase ASG `min_size`/`max_size`/`desired_capacity` + edge service `desired_count` |
| Dynamic scaling | Add an `aws_ecs_capacity_provider` with managed scaling (see the capacity note in `compute.tf`) |
| Faster edge startup | Consider adding a startup probe that polls `primary.lore.internal` before starting loreserver |
| Presigned URLs | Already configured via HMAC key in Secrets Manager |
| Production hardening | Add `deletion_protection_enabled = true` to DynamoDB tables |

Full server configuration: [Lore Server config reference](https://epicgames.github.io/lore/reference/lore-server-config/)

## Destroy

The S3 bucket has `force_destroy = false` (prevents accidental data loss). Teardown takes ~5 minutes. To destroy:

```sh
aws s3 rm s3://$(terraform output -raw s3_bucket) --recursive
terraform destroy
```

If destroy fails on Cloud Map services ("Service contains registered instances"), scale to zero first:

```sh
aws ecs update-service --cluster lore-cluster --service lore --desired-count 0 --region us-west-2
aws ecs update-service --cluster lore-cluster --service lore-edge --desired-count 0 --region us-west-2
sleep 30
terraform destroy
```

For dev/test where you want one-command teardown, add `force_destroy = true` to the `aws_s3_bucket` resource.

## Prerequisites

- [Terraform](https://developer.hashicorp.com/terraform/install) >= 1.7
- AWS credentials with VPC, ECS, EC2, S3, DynamoDB, IAM, Secrets Manager, Cloud Map, Auto Scaling, SSM permissions
- Docker (to build the ARM64 container image)

## Design

This example implements the architecture described in the [Lore System Design](https://epicgames.github.io/lore/explanation/system-design/) — S3 as the immutable store, DynamoDB as the mutable store, edge nodes as a hot cache tier with `quics://` replication to a centralized primary.
