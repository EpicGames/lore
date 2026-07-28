#!/bin/bash
set -euo pipefail

# Format NVMe instance store and register with ECS cluster.
# c8gd.8xlarge has 1x 1.9 TB NVMe SSD.

MOUNT_PATH="${mount_path}"
ECS_CLUSTER="${cluster_name}"

# Detect NVMe instance store devices (exclude EBS)
INSTANCE_STORE_DEVICES=()
for device in /dev/nvme*n1; do
  [ -e "$device" ] || continue
  devname=$(basename "$device")
  model=$(cat "/sys/block/$devname/device/model" 2>/dev/null || echo "")
  if [[ "$model" == *"Instance Storage"* ]]; then
    INSTANCE_STORE_DEVICES+=("$device")
  fi
done

# Format and mount
if [ $${#INSTANCE_STORE_DEVICES[@]} -gt 0 ]; then
  mkfs.xfs -f "$${INSTANCE_STORE_DEVICES[0]}"
  mkdir -p "$MOUNT_PATH"
  mount -o noatime,nodiratime,discard "$${INSTANCE_STORE_DEVICES[0]}" "$MOUNT_PATH"
  chmod 777 "$MOUNT_PATH"
else
  mkdir -p "$MOUNT_PATH"
  echo "WARNING: No NVMe instance store found. Using root volume at $MOUNT_PATH"
fi

# Configure ECS agent
cat >> /etc/ecs/ecs.config <<EOF
ECS_CLUSTER=$ECS_CLUSTER
ECS_ENABLE_TASK_IAM_ROLE=true
ECS_ENABLE_TASK_ENI=true
ECS_AVAILABLE_LOGGING_DRIVERS=["json-file","awslogs"]
EOF
