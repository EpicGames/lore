# Test Plan: Lore on AWS — E2E Validation

Manual steps to deploy, validate, and tear down the `contrib/aws/` infrastructure.
Run these steps to confirm the example works end-to-end before merging.

> **Source:** These steps mirror the official [Lore quickstart](https://epicgames.github.io/lore/tutorials/quickstart/)
> adapted for the AWS deployment topology (primary + edge, TLS, Cloud Map).

## Overview

| Phase | Validates |
|-------|-----------|
| [0 — Build Image](#phase-0-build-and-push-container-image) | Server container image built and available |
| [1 — Deploy](#phase-1-deploy) | Infrastructure deploys successfully |
| [2 — Health](#phase-2-infrastructure-health) | Both servers running and responding |
| [3 — Primary](#phase-3-primary-server-data-path-write-tier) | Push and clone work through the primary with data integrity |
| [4 — Edge](#phase-4-edge--full-client-workflow) | All client operations work through the edge end-to-end |
| [5 — Resources](#phase-5-aws-resource-validation) | AWS resources correctly configured and populated |
| [6 — Teardown](#phase-6-destroy) | All resources removed cleanly |

## Prerequisites

- Region: `us-west-2` (replace throughout if different)
- AWS credentials with ECS, EC2, ECR, S3, DynamoDB, IAM, SSM, Cloud Map permissions
- `terraform`, `aws` CLI, `docker`, and `jq` installed locally
- [Lore repo](https://github.com/EpicGames/lore) cloned locally (for building the container image)

---

## Phase 0: Build and Push Container Image

> **Source:** https://epicgames.github.io/lore/how-to/deploy-local-lore-server/#run-with-docker

### 0.1 Build the ARM64 image

From the Lore repo root:

```bash
cd /path/to/lore
docker buildx build --platform linux/arm64 -f lore-server/Dockerfile -t loreserver:v0.8.3 --load .
```

If building on an x86 host, register QEMU first:

```bash
docker run --rm --privileged multiarch/qemu-user-static --reset -p yes
```

**Expected:** Build completes in ~5 min. Final lines:

```
#17 exporting to docker image format
#17 sending tarball X.Xs done
#17 DONE X.Xs
```

### 0.2 Verify image architecture

```bash
docker inspect loreserver:v0.8.3 --format '{{.Architecture}}'
```

**Expected:**

```
arm64
```

If it shows `amd64`, the build didn't cross-compile. Re-run with `--platform linux/arm64` and ensure QEMU is registered. An x86 image on Graviton instances causes `exec format error` at container start.

### 0.3 Create ECR repository (if first time)

```bash
aws ecr create-repository --repository-name loreserver --region us-west-2 \
  --query 'repository.repositoryUri' --output text 2>/dev/null \
  && echo "✓ Created" \
  || echo "✓ Already exists"
```

### 0.4 Push to ECR

```bash
ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
REGION=us-west-2
ECR_URI="$ACCOUNT_ID.dkr.ecr.$REGION.amazonaws.com/loreserver:v0.8.3"

aws ecr get-login-password --region $REGION | docker login --username AWS --password-stdin "$ACCOUNT_ID.dkr.ecr.$REGION.amazonaws.com"
docker tag loreserver:v0.8.3 "$ECR_URI"
docker push "$ECR_URI"
```

**Expected:** Push completes. Verify:

```bash
aws ecr describe-images --repository-name loreserver --region us-west-2 \
  --query 'imageDetails[?imageTags[0]==`v0.8.3`].{tag:imageTags[0],pushed:imagePushedAt}' \
  --output table
```

**Expected:**

```
------------------------------------------------
|                DescribeImages                |
+------------------------------------+---------+
|               pushed               |   tag   |
+------------------------------------+---------+
|  <today's date>                    |  v0.8.3 |
+------------------------------------+---------+
```

One row with today's date confirms the image landed. If empty — the push failed or the tag is wrong.

### 0.5 Create terraform.tfvars

> **Requires:** `$ECR_URI` set from Phase 0.4. If running in a fresh shell, reconstruct it: `ECR_URI="<ACCOUNT_ID>.dkr.ecr.us-west-2.amazonaws.com/loreserver:v0.8.3"`

```bash
cd contrib/aws
cat > terraform.tfvars <<EOF
region          = "us-west-2"
container_image = "$ECR_URI"
allowed_cidrs   = ["10.0.0.0/16"]
EOF
```

---

## Phase 1: Deploy

### 1.1 Apply

```bash
cd contrib/aws
terraform init
terraform apply
```

**Expected plan output:**

```
Plan: 67 to add, 0 to change, 0 to destroy.

Changes to Outputs:
  + cluster_name       = "lore-cluster"
  + edge_dns           = "edge.lore.internal"
  + primary_dns        = "primary.lore.internal"
  + s3_bucket          = (known after apply)
  + service_name       = "lore"
  ...
```

Type `yes` to approve. Completes in ~5 min.

If apply fails with a DynamoDB PITR error, re-run `terraform apply` — this is a known timing race on first deploy.

### 1.2 Capture outputs

```bash
terraform output -raw cluster_name
terraform output -raw service_name
terraform output -raw edge_service_name
terraform output -raw primary_dns
terraform output -raw edge_dns
terraform output -raw s3_bucket
terraform output -raw log_group
terraform output -raw ca_certificate_pem > lore-ca.pem
```

Record these — you'll use them in every subsequent step.

### 1.3 Verify tasks are running (not crash-looping)

Wait 3–5 min after apply, then:

> **Note:** `date -d` is GNU coreutils (Linux). On macOS use `date -v-5M +%s000`, or portably: `$(python3 -c 'import time; print(int((time.time()-300)*1000))')`.

```bash
aws logs filter-log-events --log-group-name "/ecs/lore" \
  --start-time $(date -d '5 minutes ago' +%s000) --limit 3 \
  --query 'events[].message' --output text --region us-west-2
```

**Expected:** JSON log lines containing `"Server is up"` or startup messages (config loading, plugin registration).

**If you see:** `exec /usr/local/bin/loreserver: exec format error` — the image architecture is wrong. The ECR image is x86 but the instances are ARM64. Re-build with `--platform linux/arm64 --load`, re-push, then force new deployment:

```bash
aws ecs update-service --cluster lore-cluster --service lore --force-new-deployment --region us-west-2 --query 'service.serviceName' --output text
aws ecs update-service --cluster lore-cluster --service lore-edge --force-new-deployment --region us-west-2 --query 'service.serviceName' --output text
```

---

## Phase 2: Infrastructure Health

### 2.1 ECS services running

Service names follow the naming convention: cluster = `lore-cluster`, primary = `lore`, edge = `lore-edge`.

```bash
aws ecs describe-services \
  --cluster lore-cluster \
  --services lore lore-edge \
  --query 'services[].{name:serviceName,status:status,running:runningCount,desired:desiredCount}' \
  --output table --region us-west-2
```

**Expected:**

```
-----------------------------------------------
|              DescribeServices               |
+---------+-------------+----------+----------+
| desired |    name     | running  | status   |
+---------+-------------+----------+----------+
|  1      |  lore       |  1       |  ACTIVE  |
|  1      |  lore-edge  |  1       |  ACTIVE  |
+---------+-------------+----------+----------+
```

If `running = 0`, wait 3–5 min (instances launching + image pull). Re-run until both show `1`. On first deploy, the edge may cycle once due to a cold-start race (the edge's QUIC connection to the primary blocks if the primary isn't ready yet). A container health check automatically replaces the stuck task — total time to edge healthy is ~7 min from initial apply.

### 2.2 Server logs confirm startup

```bash
aws logs filter-log-events \
  --log-group-name $(terraform output -raw log_group) \
  --filter-pattern '"Server is up"' --limit 5 \
  --query 'events[].message' --output json --region us-west-2 \
  | jq -r '.[] | fromjson | "\(.["log.logger"]): \(.message)"'
```

**Expected:** Two lines (primary + edge):

```
lore_server::server: Server is up, waiting for shutdown signal
lore_server::server: Server is up, waiting for shutdown signal
```

### 2.3 Health check (HTTP 200)

Run from inside the VPC via SSM:

```bash
INSTANCE_ID=$(aws ec2 describe-instances \
  --filters "Name=tag:Name,Values=lore-ecs" \
            "Name=instance-state-name,Values=running" \
  --query 'Reservations[0].Instances[0].InstanceId' \
  --output text --region us-west-2)

aws ssm start-session --target "$INSTANCE_ID" --region us-west-2
```

**[instance]** — inside the SSM session:

```bash
curl -i http://primary.lore.internal:41339/health_check
curl -i http://edge.lore.internal:41339/health_check
```

**Expected:** Both return `HTTP/1.1 200 OK` with empty body.

✅ **What this validates:** Both servers started successfully and are reachable inside the VPC.

---

## Phase 3: Primary Server Data Path (write tier)

> Mirrors [quickstart Steps 2–5](https://epicgames.github.io/lore/tutorials/quickstart/#step-2-verify-server-health-and-create-a-repository): create → stage → commit → push → clone.
>
> The **primary server** (write tier) holds durable storage (S3 + DynamoDB). This phase validates the direct data path without edge replication.
>
> Labels: **[local]** = your machine · **[instance]** = inside the SSM session
>
> Phase 3.1 runs entirely from `[local]` via send-command (setup).
> Phase 3.2 onward runs `[instance]` (interactive lore workflow).

### 3.1 Setup: Build CLI + CA bundle

All from `[local]` — no interactive session needed yet.

The ECS instances are ARM64 (Graviton) running Amazon Linux 2023. There's no prebuilt `lore` CLI for `aarch64-linux`. We build inside Docker on the instance via send-command.

**[local]** — push CA bundle:

```bash
B64_CERT=$(terraform output -raw ca_certificate_pem | base64 | tr -d '\n')
CMD_ID=$(aws ssm send-command --instance-ids "$INSTANCE_ID" \
  --document-name "AWS-RunShellScript" \
  --parameters "{\"commands\":[\"echo $B64_CERT | base64 -d > /tmp/lore-ca.pem\",\"cat /etc/pki/tls/certs/ca-bundle.crt /tmp/lore-ca.pem > /tmp/combined-ca.pem\",\"echo CA_BUNDLE_OK\"]}" \
  --query 'Command.CommandId' --output text --region us-west-2)
echo "Command: $CMD_ID"
```

**Expected:**

```
Command: a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

**[local]** — build lore CLI (~4 min cold, cached after):

```bash
BUILD_CMD_ID=$(aws ssm send-command --instance-ids "$INSTANCE_ID" \
  --document-name "AWS-RunShellScript" --timeout-seconds 900 \
  --parameters '{"commands":["mkdir -p /tmp/lore-src","docker run --rm -v /tmp/lore-src:/out rust:latest bash -c '"'"'git clone --depth 1 https://github.com/EpicGames/lore.git /build && apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null && cd /build && cargo build --release -p lore-client && cp /build/target/release/lore /out/lore && echo BUILD_OK'"'"'"]}' \
  --query 'Command.CommandId' --output text --region us-west-2)
echo "Build command: $BUILD_CMD_ID"
```

**Expected:**

```
Build command: a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

**[local]** — poll until complete (re-run every 30s):

```bash
aws ssm get-command-invocation --command-id "$BUILD_CMD_ID" --instance-id "$INSTANCE_ID" \
  --query '{status:Status,output:StandardOutputContent}' --output table --region us-west-2
```

While in progress you'll see:

```
--------------------------
|  GetCommandInvocation  |
+---------+--------------+
| output  |   status     |
+---------+--------------+
|         |  InProgress  |
+---------+--------------+
```

Re-run every 30 seconds until you see:

**Expected (after ~4 min):**

```
-------------------------------
|   GetCommandInvocation      |
+------------+----------------+
|   output   |    status      |
+------------+----------------+
|  BUILD_OK  |    Success     |
+------------+----------------+
```

If `status: Failed` or still `InProgress` after 10 minutes — check the output field for errors (usually missing disk space or network timeout on `git clone`).

**[local]** — start the interactive session for the rest of the test:

```bash
aws ssm start-session --target "$INSTANCE_ID" --region us-west-2
```

**[instance]** — start an interactive Docker container with lore, network access, and TLS trust. This is your "test client" — all remaining lore commands run inside it:

> **Note:** First run pulls `rust:latest` (~1.5 GB) which adds 2–3 min. Subsequent runs use the cached layer.

```bash
sudo docker run -it --rm --network host \
  -v /tmp/lore-src:/out \
  -v /tmp/combined-ca.pem:/certs/ca.pem \
  -e SSL_CERT_FILE=/certs/ca.pem \
  -w /tmp rust:latest bash
```

**Expected:** You get a `root@<id>:/tmp#` prompt inside the container.

Set up a convenience alias inside the container:

```bash
alias lore=/out/lore
lore --version
```

**Expected:** A version string like `lore 0.8.x-...`. The exact version depends on the Lore commit checked out during the build.

All steps below (3.2–4.6) run inside this container prompt.

### 3.2 Create repository ([step 2](https://epicgames.github.io/lore/tutorials/quickstart/#step-2-verify-server-health-and-create-a-repository))

```bash
mkdir e2e-test && cd e2e-test
lore repository create lores://primary.lore.internal:41337/e2e-test
```

**Expected:** `Created repository e2e-test in /tmp/e2e-test with ID <hex>`

### 3.3 Add files + stage ([step 3](https://epicgames.github.io/lore/tutorials/quickstart/#step-3-add-files-and-stage-them))

```bash
echo "Hello, Lore on AWS" > hello.txt
dd if=/dev/urandom of=asset.bin bs=1M count=10 2>/dev/null
lore stage hello.txt asset.bin
lore status --scan
```

**Expected:** Status shows `A hello.txt` and `A asset.bin` staged.

### 3.4 Commit ([step 4](https://epicgames.github.io/lore/tutorials/quickstart/#step-4-commit-the-revision))

```bash
lore commit "Initial revision"
```

**Expected:** `Commit succeeded` with revision 1.

### 3.5 Push ([step 5](https://epicgames.github.io/lore/tutorials/quickstart/#step-5-push-to-the-server))

```bash
lore push
```

**Expected:** `Pushed revision 1 -> <hash> to branch main`

### 3.6 Clone back via primary (write tier)

```bash
rm -rf /tmp/primary-clone
lore clone lores://primary.lore.internal:41337/e2e-test /tmp/primary-clone
md5sum /tmp/e2e-test/asset.bin /tmp/primary-clone/asset.bin
```

**Expected:**

```
Cloning repository <id> branch main into /tmp/primary-clone
Pull state <hash>
Cloned 2/2 files (10.00 MiB/10.00 MiB)
Branch main revision <hash>
Clone complete in X.XXs
<md5hash>  /tmp/e2e-test/asset.bin
<md5hash>  /tmp/primary-clone/asset.bin
```

Both MD5 hashes must be identical (same hex string on both lines).

✅ **What this validates:** The primary server stores data durably (S3 for fragments, DynamoDB for metadata) and serves it back with byte-for-byte integrity.

---

## Phase 4: Edge — Full Client Workflow

> Mirrors [quickstart Steps 6–9](https://epicgames.github.io/lore/tutorials/quickstart/#step-6-set-up-a-shared-store-and-clone-a-second-working-tree): clone → branch → merge → push → sync.
>
> The **edge server** (read tier) caches fragments locally and proxies to the primary. This phase validates that all client operations work through the edge — proving QUIC fragment replication (`quics://` port 41340) and `lores://` branch resolution (port 41337).

### 4.1 Clone via edge ([step 6](https://epicgames.github.io/lore/tutorials/quickstart/#step-6-set-up-a-shared-store-and-clone-a-second-working-tree))

```bash
rm -rf /tmp/edge-clone
lore clone lores://edge.lore.internal:41337/e2e-test /tmp/edge-clone
md5sum /tmp/edge-clone/asset.bin
```

**Expected:** Same MD5 as primary. Proves edge fetched fragments via `quics://primary...:41340`.

### 4.2 Branch create + commit ([step 7](https://epicgames.github.io/lore/tutorials/quickstart/#step-7-create-a-branch-and-commit-on-it))

```bash
cd /tmp/edge-clone
lore branch create e2e-feature
echo "Feature added on branch via edge" > feature.txt
lore stage feature.txt
lore commit "Add feature on branch"
lore push
```

**Expected:** `Pushed revision 2 -> <hash> to branch e2e-feature`

### 4.3 Merge branch into main ([step 8](https://epicgames.github.io/lore/tutorials/quickstart/#step-8-merge-the-branch-into-main))

```bash
lore branch switch main
lore branch merge e2e-feature --message "Merge e2e-feature into main"
lore push
```

**Expected:** Merge succeeds with no conflicts. Push reports revision 3.

### 4.4 History via edge

```bash
lore history
```

**Expected:** Shows at least the initial commit and the merge commit on the `main` branch. Branch-only commits (e.g., rev 2 on `e2e-feature`) may not appear in main's linear history depending on the Lore version's history walk strategy.

### 4.5 Sync on primary clone ([step 9](https://epicgames.github.io/lore/tutorials/quickstart/#step-9-sync-the-second-working-tree))

```bash
cd /tmp/primary-clone
lore sync
cat feature.txt
```

**Expected:** `feature.txt` contains "Feature added on branch via edge". Proves edge writes reached durable S3/DynamoDB and are visible from primary.

### 4.6 Status — in sync

```bash
lore status
```

**Expected:** `Local branch in sync with remote` at revision 3.

✅ **What this validates:** Clients can clone, branch, merge, and push through the edge server. The edge correctly fetches fragments from the primary and resolves branches in real time. Data pushed via the edge reaches durable storage (S3 + DynamoDB) and is visible from the primary.

---

## Phase 5: AWS Resource Validation

> Exit the Docker container and SSM session (`exit` twice). All commands below run **[local]**.

Confirm the underlying AWS resources were created correctly and are functioning as expected.

### 5.1 S3 fragments stored

```bash
aws s3 ls "s3://$(terraform output -raw s3_bucket)/" --summarize --region us-west-2 | tail -3
```

**Expected:** `Total Objects: >0`, `Total Size: >10MB`

### 5.2 DynamoDB tables populated

```bash
for table in $(aws dynamodb list-tables --query "TableNames[?contains(@,'lore')]" --output text --region us-west-2); do
  COUNT=$(aws dynamodb scan --table-name "$table" --select COUNT --query 'Count' --output text --region us-west-2)
  echo "  $table: $COUNT items"
done
```

**Expected:**
- `*-fragments`: items (fragment associations)
- `*-metadata`: items (fragment metadata)
- `*-mutable`: items (branch pointers)
- `*-locks`: 0 (no active locks expected)

### 5.3 Cloud Map services registered

```bash
NAMESPACE=$(aws servicediscovery list-namespaces \
  --query "Namespaces[?contains(Name,'lore')].Id" --output text --region us-west-2)
echo "Namespace: $NAMESPACE"
```

**Expected:** A namespace ID like `ns-xxxxxxxxxxxx`.

```bash
PRIMARY_SERVICE_ID=$(aws servicediscovery list-services \
  --filters "Name=NAMESPACE_ID,Values=$NAMESPACE" \
  --query "Services[?Name=='primary'].Id" --output text --region us-west-2)
echo "Primary service: $PRIMARY_SERVICE_ID"
```

**Expected:** A service ID like `srv-xxxxxxxxxxxx`. If empty — the primary service isn't registered in Cloud Map.

```bash
aws servicediscovery list-instances \
  --service-id "$PRIMARY_SERVICE_ID" \
  --query 'Instances[].Attributes' --output json --region us-west-2
```

**Expected:**

```json
[
    {
        "AVAILABILITY_ZONE": "us-west-2x",
        "AWS_INIT_HEALTH_STATUS": "HEALTHY",
        "AWS_INSTANCE_IPV4": "10.x.x.x",
        "ECS_CLUSTER_NAME": "lore-cluster",
        "ECS_SERVICE_NAME": "lore",
        ...
    }
]
```

One entry with `AWS_INSTANCE_IPV4` set to a private IP and `AWS_INIT_HEALTH_STATUS: HEALTHY` confirms the primary is registered in Cloud Map and discoverable by the edge.

### 5.4 Security group rules

```bash
SG_ID=$(aws ec2 describe-security-groups \
  --filters "Name=group-name,Values=*lore*server*" \
  --query 'SecurityGroups[0].GroupId' --output text --region us-west-2)

aws ec2 describe-security-group-rules \
  --filters "Name=group-id,Values=$SG_ID" \
  --query 'SecurityGroupRules[?IsEgress==`false`].{port:FromPort,proto:IpProtocol,desc:Description}' \
  --output table --region us-west-2
```

**Expected:**

```
-----------------------------------------------------------
|               DescribeSecurityGroupRules                |
+--------------------------------------+--------+---------+
|                 desc                 | port   |  proto  |
+--------------------------------------+--------+---------+
|  Lore client (gRPC)                  |  41337 |  tcp    |
|  Lore client (QUIC)                  |  41337 |  udp    |
|  Lore client (HTTP)                  |  41339 |  tcp    |
|  Lore replication (QUIC)             |  41340 |  udp    |
|  Lore branch resolution (gRPC)       |  41337 |  tcp    |
|  Lore data transfer (QUIC)           |  41337 |  udp    |
+--------------------------------------+--------+---------+
```

Six inbound rules covering client access (gRPC + QUIC + HTTP) and internal node communication (replication + branch resolution + data transfer).

### 5.5 VPC endpoints active

```bash
aws ec2 describe-vpc-endpoints \
  --filters "Name=tag:Name,Values=*lore*" \
  --query 'VpcEndpoints[].{service:ServiceName,state:State}' \
  --output table --region us-west-2
```

**Expected:** S3 and DynamoDB gateway endpoints, both `available`.

### 5.6 Task IAM roles

```bash
# Find the primary task role (has S3 + DynamoDB access)
PRIMARY_ROLE=$(aws iam list-roles --query "Roles[?contains(RoleName,'lore-task')].RoleName" --output text --region us-west-2)
echo "Primary role: $PRIMARY_ROLE"
aws iam list-role-policies --role-name "$PRIMARY_ROLE" --output table --region us-west-2
aws iam list-attached-role-policies --role-name "$PRIMARY_ROLE" --output table --region us-west-2
```

**Expected:** Primary role has `s3-...` and `dynamodb-...` policies (inline or managed — check both tables).

```bash
# Find the edge task role (should have no policies — proxies everything through primary)
EDGE_ROLE=$(aws iam list-roles --query "Roles[?contains(RoleName,'lore-edge-task')].RoleName" --output text --region us-west-2)
echo "Edge role: $EDGE_ROLE"
aws iam list-role-policies --role-name "$EDGE_ROLE" --output table --region us-west-2
aws iam list-attached-role-policies --role-name "$EDGE_ROLE" --output table --region us-west-2
```

**Expected:** Both tables empty for edge — confirms least-privilege design (edge has no direct AWS access, proxies everything via the primary).

---

## Phase 6: Destroy

The S3 bucket has versioning enabled. `aws s3 rm --recursive` removes current objects but leaves version history and delete markers, which blocks bucket deletion. Delete all versions first:

> **Note:** `list-object-versions` returns at most 1000 entries per call. For buckets with heavy usage (many pushes), you may need to loop until `NextKeyMarker` is empty. For a typical test run (<1000 versions) the single call below is sufficient.

```bash
BUCKET=$(terraform output -raw s3_bucket)
aws s3api list-object-versions --bucket "$BUCKET" --region us-west-2 \
  --query '{Objects: Versions[].{Key:Key,VersionId:VersionId}}' --output json \
  | aws s3api delete-objects --bucket "$BUCKET" --region us-west-2 --delete file:///dev/stdin >/dev/null
aws s3api list-object-versions --bucket "$BUCKET" --region us-west-2 \
  --query '{Objects: DeleteMarkers[].{Key:Key,VersionId:VersionId}}' --output json \
  | aws s3api delete-objects --bucket "$BUCKET" --region us-west-2 --delete file:///dev/stdin >/dev/null
terraform destroy
```

**Expected:** Completes in ~5 min. If destroy fails (provider crash, timeout, or Cloud Map dependency error), re-run `terraform destroy` — ECS services will already be draining and the retry typically succeeds. If it still fails after 2 attempts:

```bash
aws ecs update-service --cluster lore-cluster \
  --service lore --desired-count 0 --region us-west-2
aws ecs update-service --cluster lore-cluster \
  --service lore-edge --desired-count 0 --region us-west-2
sleep 30
terraform destroy
```

---
