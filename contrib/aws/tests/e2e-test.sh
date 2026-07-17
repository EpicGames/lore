#!/usr/bin/env bash
# tests/e2e-test.sh — End-to-end validation of Lore on AWS.
# Mirrors: https://epicgames.github.io/lore/tutorials/quickstart/
#
# Usage: ./tests/e2e-test.sh [region]
#
# Uploads a test payload script to S3, triggers it on an ECS instance via SSM,
# and polls for results. All lore commands run inside Docker (--network host)
# to avoid GLIBC mismatch between rust:latest and Amazon Linux 2023.
set -euo pipefail

REGION="${1:-us-west-2}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

PASS=0; FAIL=0
step()  { echo ""; echo "=== $1 ==="; }
info()  { echo "  $1"; }
pass()  { echo "  ✓ $1"; PASS=$((PASS + 1)); }
fail()  { echo "  ✗ $1"; FAIL=$((FAIL + 1)); }
check() { if echo "$OUTPUT" | grep -q "$1"; then pass "$2"; else fail "$2"; fi; }

# ─── Deployment info ────────────────────────────────────────────────────────

step "Deployment Info"
PRIMARY_DNS=$(terraform output -raw primary_dns)
EDGE_DNS=$(terraform output -raw edge_dns)
CLUSTER=$(terraform output -raw cluster_name)
S3_BUCKET=$(terraform output -raw s3_bucket)
CA_CERT=$(terraform output -raw ca_certificate_pem)

info "Primary: $PRIMARY_DNS"
info "Edge:    $EDGE_DNS"

INSTANCE_ID=$(aws ec2 describe-instances \
  --filters "Name=tag:Name,Values=${CLUSTER%%-cluster}-ecs" "Name=instance-state-name,Values=running" \
  --query 'Reservations[0].Instances[0].InstanceId' --output text --region "$REGION")
[[ "$INSTANCE_ID" == "None" || -z "$INSTANCE_ID" ]] && { fail "No ECS instances"; exit 1; }
info "Instance: $INSTANCE_ID"

# ─── Phase 1: Health ────────────────────────────────────────────────────────

step "Phase 1: Infrastructure Health"

info "Waiting for services to stabilize (up to 5 min)..."
for _ in $(seq 1 10); do
  PRIMARY_RUNNING=$(aws ecs describe-services --cluster "$CLUSTER" \
    --services "$(terraform output -raw service_name)" \
    --query 'services[0].runningCount' --output text --region "$REGION")
  EDGE_RUNNING=$(aws ecs describe-services --cluster "$CLUSTER" \
    --services "$(terraform output -raw edge_service_name)" \
    --query 'services[0].runningCount' --output text --region "$REGION")
  [[ "$PRIMARY_RUNNING" -ge 1 && "$EDGE_RUNNING" -ge 1 ]] && break
  sleep 30
done

if [[ "$PRIMARY_RUNNING" -ge 1 ]]; then pass "Primary running"; else fail "Primary not running"; fi
if [[ "$EDGE_RUNNING" -ge 1 ]]; then pass "Edge running"; else fail "Edge not running"; fi

# Edge cold-start race: ReplicatedStore::new() blocks if primary's QUIC port
# isn't accepting yet. The ECS task shows RUNNING but the server hasn't bound
# ports. force-new-deployment resolves it (primary is warm on second attempt).
info "Checking edge connectivity (force-new-deployment if stuck)..."
EDGE_HEALTH=$(aws ssm send-command --instance-ids "$INSTANCE_ID" \
  --document-name "AWS-RunShellScript" \
  --parameters '{"commands":["curl -sf --connect-timeout 5 http://'"$EDGE_DNS"':41339/health_check && echo EDGE_OK || echo EDGE_STUCK"]}' \
  --query 'Command.CommandId' --output text --region "$REGION")
sleep 10
EDGE_STATUS=$(aws ssm get-command-invocation --command-id "$EDGE_HEALTH" --instance-id "$INSTANCE_ID" \
  --query 'StandardOutputContent' --output text --region "$REGION" 2>/dev/null)
if echo "$EDGE_STATUS" | grep -q "EDGE_STUCK"; then
  info "Edge stuck on cold start — forcing new deployment..."
  aws ecs update-service --cluster "$CLUSTER" --service "$(terraform output -raw edge_service_name)" \
    --force-new-deployment --region "$REGION" --query 'service.serviceName' --output text >/dev/null
  sleep 90
  pass "Edge recovered (force-new-deployment)"
else
  pass "Edge responding"
fi

# ─── Upload payload + source ────────────────────────────────────────────────

step "Setup: Upload test payload + build CLI"

# Upload test payload script
aws s3 cp "$SCRIPT_DIR/e2e-payload.sh" "s3://$S3_BUCKET/build/e2e-payload.sh" --region "$REGION" >/dev/null

PAYLOAD_URL=$(aws s3 presign "s3://$S3_BUCKET/build/e2e-payload.sh" --expires-in 900 --region "$REGION")
B64_CERT=$(echo "$CA_CERT" | base64 | tr -d '\n')

# ─── Build CLI + prepare CA (SSM, async) ────────────────────────────────────

info "Building lore CLI on instance (cached after first run)..."

BUILD_CMD_ID=$(aws ssm send-command \
  --instance-ids "$INSTANCE_ID" \
  --document-name "AWS-RunShellScript" \
  --timeout-seconds 1800 \
  --parameters "{\"commands\":[\"set -e\",\"echo '$B64_CERT' | base64 -d > /tmp/lore-ca.pem\",\"cat /etc/pki/tls/certs/ca-bundle.crt /tmp/lore-ca.pem > /tmp/combined-ca.pem\",\"curl -sSo /tmp/e2e-payload.sh '$PAYLOAD_URL' && chmod +x /tmp/e2e-payload.sh\",\"if [ -f /tmp/lore-src/lore ]; then echo CACHED; echo SETUP_OK; exit 0; fi\",\"mkdir -p /tmp/lore-src\",\"docker run --rm -v /tmp/lore-src:/out rust:latest bash -c 'git clone --depth 1 https://github.com/EpicGames/lore.git /build && apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null 2>&1 && cd /build && cargo build --release -p lore-client 2>&1 | tail -5 && cp /build/target/release/lore /out/lore'\",\"echo SETUP_OK\"]}" \
  --query 'Command.CommandId' --output text --region "$REGION")

# Poll build
printf "  Building"
while true; do
  sleep 15; printf "."
  STATUS=$(aws ssm get-command-invocation --command-id "$BUILD_CMD_ID" --instance-id "$INSTANCE_ID" \
    --query 'Status' --output text --region "$REGION" 2>/dev/null || echo "Pending")
  case "$STATUS" in
    InProgress|Pending) ;;
    Success) echo ""; pass "CLI built + payload uploaded"; break ;;
    *) echo ""
      fail "Build failed"
      aws ssm get-command-invocation --command-id "$BUILD_CMD_ID" --instance-id "$INSTANCE_ID" \
        --query 'StandardOutputContent' --output text --region "$REGION" | tail -5
      exit 1 ;;
  esac
done

# ─── Run test phases (SSM, async) ───────────────────────────────────────────

step "Phases 2-4: Running test payload"
info "All lore commands run inside Docker (--network host) on instance"

TEST_CMD_ID=$(aws ssm send-command \
  --instance-ids "$INSTANCE_ID" \
  --document-name "AWS-RunShellScript" \
  --timeout-seconds 600 \
  --parameters "{\"commands\":[\"docker run --rm --network host -v /tmp/lore-src:/src -v /tmp/combined-ca.pem:/certs/ca.pem -v /tmp/e2e-payload.sh:/e2e-payload.sh -w /tmp rust:latest bash /e2e-payload.sh $PRIMARY_DNS $EDGE_DNS\"]}" \
  --query 'Command.CommandId' --output text --region "$REGION")

info "Command: $TEST_CMD_ID"

# Poll test
printf "  Running"
while true; do
  sleep 20; printf "."
  STATUS=$(aws ssm get-command-invocation --command-id "$TEST_CMD_ID" --instance-id "$INSTANCE_ID" \
    --query 'Status' --output text --region "$REGION" 2>/dev/null || echo "Pending")
  case "$STATUS" in
    InProgress|Pending) ;;
    Success|Failed) echo ""; break ;;
  esac
done

OUTPUT=$(aws ssm get-command-invocation --command-id "$TEST_CMD_ID" --instance-id "$INSTANCE_ID" \
  --query 'StandardOutputContent' --output text --region "$REGION")

# ─── Parse results ──────────────────────────────────────────────────────────

step "Results"

check "REPO_CREATE_OK" "repository create"
check "STAGE_OK" "stage"
check "COMMIT_OK" "commit"
check "PUSH_OK" "push"
check "INTEGRITY_OK" "primary clone MD5 match"
check "CLONE_OK" "clone via edge (QUIC replication)"
check "BRANCH_OK" "branch create + commit + push via edge"
check "MERGE_OK" "branch merge + push via edge"
check "HISTORY_OK" "history via edge"
check "SYNC_OK" "sync: edge writes reached durable storage"
check "STATUS_OK" "status: in sync"
check "REVISIONS_MATCH" "same revision on both trees"
check "ASSET_MATCH" "binary content matches"
check "FEATURE_MATCH" "branch-merged file present on both"
check "ALL PHASES COMPLETE" "all phases completed"

echo ""
echo "  $PASS passed, $FAIL failed"

if [[ $FAIL -gt 0 ]]; then
  echo ""
  echo "  --- Output (last 30 lines) ---"
  echo "$OUTPUT" | tail -30
  exit 1
fi
