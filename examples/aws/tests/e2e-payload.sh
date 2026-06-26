#!/usr/bin/env bash
# Remote test payload — runs ON the ECS instance inside Docker.
# Uploaded to S3, downloaded via presigned URL, executed via SSM.
# Requires: /tmp/combined-ca.pem exists, /tmp/lore-src/target/release/lore built.
set -e

LORE=/src/lore
PRIMARY="$1"
EDGE="$2"
REPO="e2e-$(date +%s)"

export SSL_CERT_FILE=/certs/ca.pem

echo "=== PHASE 2: PRIMARY DATA PATH ==="
echo "--- repository create ---"
mkdir -p "/tmp/$REPO" && cd "/tmp/$REPO"
$LORE repository create "lores://$PRIMARY:41337/$REPO"
echo REPO_CREATE_OK

echo "--- stage ---"
echo "Hello, Lore on AWS" > hello.txt
dd if=/dev/urandom of=asset.bin bs=1M count=10 2>/dev/null
$LORE stage hello.txt asset.bin
echo STAGE_OK

echo "--- commit ---"
$LORE --non-interactive commit "Initial revision"
echo COMMIT_OK

echo "--- push ---"
$LORE push
echo PUSH_OK

echo "--- clone back via primary ---"
$LORE clone "lores://$PRIMARY:41337/$REPO" /tmp/primary-clone
PUSH_MD5=$(md5sum "/tmp/$REPO/asset.bin" | cut -d' ' -f1)
CLONE_MD5=$(md5sum /tmp/primary-clone/asset.bin | cut -d' ' -f1)
echo "PUSH_MD5=$PUSH_MD5"
echo "CLONE_MD5=$CLONE_MD5"
[ "$PUSH_MD5" = "$CLONE_MD5" ] && echo INTEGRITY_OK || echo INTEGRITY_FAIL

echo ""
echo "=== PHASE 3: EDGE FULL CLIENT WORKFLOW ==="
echo "--- clone via edge ---"
$LORE clone "lores://$EDGE:41337/$REPO" /tmp/edge-clone
echo CLONE_OK

echo "--- branch create + commit + push via edge ---"
cd /tmp/edge-clone
$LORE branch create e2e-feature
echo "Feature added on branch via edge" > feature.txt
$LORE stage feature.txt
$LORE --non-interactive commit "Add feature on branch"
$LORE push
echo BRANCH_OK

echo "--- branch switch + merge ---"
$LORE branch switch main
$LORE branch merge e2e-feature --message "Merge e2e-feature into main"
$LORE push
echo MERGE_OK

echo "--- history ---"
$LORE history
echo HISTORY_OK

echo "--- sync on primary clone ---"
cd /tmp/primary-clone
$LORE sync
[ -f /tmp/primary-clone/feature.txt ] && echo SYNC_OK || echo SYNC_FAIL

echo "--- status ---"
$LORE status
$LORE status | grep -q "in sync" && echo STATUS_OK || echo STATUS_FAIL

echo ""
echo "=== PHASE 4: FINAL VERIFICATION ==="
REV_A=$($LORE status | grep -o "revision [0-9]*" | head -1)
cd /tmp/edge-clone
REV_B=$($LORE status | grep -o "revision [0-9]*" | head -1)
echo "PRIMARY=$REV_A"
echo "EDGE=$REV_B"
[ "$REV_A" = "$REV_B" ] && echo REVISIONS_MATCH || echo REVISIONS_MISMATCH

diff <(md5sum /tmp/primary-clone/asset.bin | cut -d' ' -f1) <(md5sum /tmp/edge-clone/asset.bin | cut -d' ' -f1) && echo ASSET_MATCH
[ -f /tmp/primary-clone/feature.txt ] && [ -f /tmp/edge-clone/feature.txt ] && echo FEATURE_MATCH

echo ""
echo "=== ALL PHASES COMPLETE ==="
