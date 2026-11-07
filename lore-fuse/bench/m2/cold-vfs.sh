#!/usr/bin/env bash
# Makes a cold virtual clone: stops the benchmark service, deletes its shared store, external
# directories and registry entries, drops the page cache, starts the service and clones.
# Usage: cold-vfs.sh <view> <mount path> [prefetch file]
# M2_KEEP_STORE=1 keeps the shared store, for a clone whose reads the store already holds.
# Prints the clone's wall time in seconds as the last line.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
source "$here/common.sh"

view=$1 mount=$2 prefetch=${3:-}
url=${M2_URL:-lore.internal.epicgames.net/Fortnite}
data=$LORE_GLOBAL_PATH/data

"$LORE" service stop --force > /dev/null 2>&1
while [ -n "$(m2_service_pids)" ]; do sleep 1; done
if grep -q " $mount fuse.lore " /proc/self/mounts; then
    fusermount3 -u -z "$mount"
fi
[ -n "${M2_KEEP_STORE:-}" ] || rm -rf "$data/shared_store"
rm -rf "$data/external" "$mount"
mkdir -p "$data/stores"
echo 'entries = []' > "$data/stores/registry.toml"
m2_drop_caches

"$LORE" service start > "${M2_SERVICE_LOG:-/tmp/lore-fuse-m2/service.log}" 2>&1 || exit 1

args=(clone --vfs fuse --use-shared-store --revision "$BENCH_REV" --view "$view")
[ -n "$prefetch" ] && args+=(--prefetch "$prefetch")
start=$(m2_now)
"$LORE" "${args[@]}" "$url" "$mount" >&2
status=$?
end=$(m2_now)
[ $status -eq 0 ] || exit $status
m2_elapsed "$start" "$end"
