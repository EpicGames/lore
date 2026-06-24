#!/usr/bin/env bash
# scripts/e2e-test.sh — End-to-end validation of Lore push + clone via the edge node.
# Requires: terraform apply completed, AWS credentials, SSM access to instances.
# Platforms: Linux, macOS (runs remotely on Graviton instance via SSM)
#
# Usage: ./scripts/e2e-test.sh [region]
#
# Builds the Lore CLI from source inside a Docker container on one of the
# ECS instances, then pushes a 10MB test file and clones it back to verify
# data integrity through the full storage chain (NVMe cache → S3 → replication).
set -euo pipefail

REGION="${1:-us-west-2}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EXAMPLE_DIR="$SCRIPT_DIR/.."

cd "$EXAMPLE_DIR"

echo "=== E2E Test: Lore on AWS ==="

# Get deployment info from terraform
S3_BUCKET=$(terraform output -raw s3_bucket)
CA_CERT=$(terraform output -raw ca_certificate_pem)
EDGE_DNS=$(terraform output -raw edge_dns)
PRIMARY_DNS=$(terraform output -raw primary_dns)
echo "  Bucket: $S3_BUCKET"
echo "  Edge:   $EDGE_DNS"
echo "  Primary: $PRIMARY_DNS"

# Find an instance to run on (uses ECS-managed tag, not the Name tag)
CLUSTER=$(terraform output -raw cluster_name)
INSTANCE_ID=$(aws ec2 describe-instances \
  --filters "Name=tag:aws:ecs:clusterName,Values=$CLUSTER" 'Name=instance-state-name,Values=running' \
  --query 'Reservations[0].Instances[0].InstanceId' \
  --output text --region "$REGION")
echo "  Instance: $INSTANCE_ID"

# Upload source if not already present
if ! aws s3 ls "s3://$S3_BUCKET/build/lore-src.tar.gz" --region "$REGION" >/dev/null 2>&1; then
  echo "  Uploading Lore source to S3..."
  REPO_ROOT="$(cd "$EXAMPLE_DIR/../.." && pwd)"
  tar -czf /tmp/lore-src.tar.gz -C "$REPO_ROOT" \
    --exclude=target --exclude=.git --exclude='examples/aws/.terraform*' \
    --exclude='*.tfstate*' --exclude='*.tfvars' .
  aws s3 cp /tmp/lore-src.tar.gz "s3://$S3_BUCKET/build/lore-src.tar.gz" --region "$REGION"
fi

PRESIGNED_URL=$(aws s3 presign "s3://$S3_BUCKET/build/lore-src.tar.gz" --expires-in 900 --region "$REGION")

# Write the CA cert for the combined bundle
echo "$CA_CERT" > /tmp/e2e-ca.pem

echo ""
echo "=== Building Lore CLI on $INSTANCE_ID (takes ~4 min) ==="

COMMAND_ID=$(aws ssm send-command \
  --instance-ids "$INSTANCE_ID" \
  --document-name "AWS-RunShellScript" \
  --parameters "commands=[
    \"set -ex\",
    \"curl -sSo /tmp/lore-src.tar.gz '$PRESIGNED_URL'\",
    \"rm -rf /tmp/lore-src && mkdir -p /tmp/lore-src && tar -xzf /tmp/lore-src.tar.gz -C /tmp/lore-src\",
    \"echo '$CA_CERT' > /tmp/lore-ca.pem && cat /etc/pki/tls/certs/ca-bundle.crt /tmp/lore-ca.pem > /tmp/combined-ca.pem\",
    \"docker run --rm --network host -v /tmp/lore-src:/src -v /tmp/combined-ca.pem:/certs/ca.pem -w /src -e SSL_CERT_FILE=/certs/ca.pem rust:latest bash -c 'apt-get update -qq && apt-get install -y -qq pkg-config libssl-dev protobuf-compiler >/dev/null 2>&1 && cargo build --release -p lore-client 2>&1 | tail -3 && echo BUILD_OK && REPO=e2e-\$(date +%s) && ./target/release/lore --version && echo === CREATE REPO \$REPO === && ./target/release/lore repository create lores://$PRIMARY_DNS:41337/\$REPO && echo === CLONE === && ./target/release/lore clone lores://$PRIMARY_DNS:41337/\$REPO /tmp/e2e && echo === ADD 10MB FILE === && dd if=/dev/urandom of=/tmp/e2e/asset.bin bs=1M count=10 2>&1 && cd /tmp/e2e && echo === STAGE === && /src/target/release/lore stage asset.bin && echo === COMMIT === && /src/target/release/lore commit --non-interactive e2e-test && echo === PUSH === && /src/target/release/lore push && echo === CLONE BACK === && rm -rf /tmp/clone && /src/target/release/lore clone lores://$PRIMARY_DNS:41337/\$REPO /tmp/clone && echo === VERIFY === && md5sum /tmp/e2e/asset.bin /tmp/clone/asset.bin'\"
  ]" \
  --timeout-seconds 900 \
  --query 'Command.CommandId' \
  --output text \
  --region "$REGION")

echo "  Command: $COMMAND_ID"
echo "  Waiting for completion..."

# Poll until done
while true; do
  sleep 30
  STATUS=$(aws ssm get-command-invocation \
    --command-id "$COMMAND_ID" \
    --instance-id "$INSTANCE_ID" \
    --query 'Status' \
    --output text \
    --region "$REGION" 2>/dev/null || echo "Pending")
  
  case "$STATUS" in
    InProgress|Pending) echo "  ... still running" ;;
    Success)
      echo ""
      echo "=== SUCCESS ==="
      aws ssm get-command-invocation \
        --command-id "$COMMAND_ID" \
        --instance-id "$INSTANCE_ID" \
        --query 'StandardOutputContent' \
        --output text \
        --region "$REGION" | grep -A1 "VERIFY"
      echo ""
      echo "✓ Push + Clone verified. MD5 checksums match."
      exit 0
      ;;
    *)
      echo ""
      echo "=== FAILED (status: $STATUS) ==="
      aws ssm get-command-invocation \
        --command-id "$COMMAND_ID" \
        --instance-id "$INSTANCE_ID" \
        --query 'StandardOutputContent' \
        --output text \
        --region "$REGION" | tail -20
      echo "---STDERR---"
      aws ssm get-command-invocation \
        --command-id "$COMMAND_ID" \
        --instance-id "$INSTANCE_ID" \
        --query 'StandardErrorContent' \
        --output text \
        --region "$REGION" | grep -v "^++" | grep -v "MII\|BEGIN\|END" | tail -10
      exit 1
      ;;
  esac
done
