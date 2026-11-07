#!/usr/bin/env bash
# Runs the cold build comparison: <reps> real-clone builds, then <reps> VFS builds per mode, each
# VFS build timed from the cold clone. Appends one line per run to <M2_OUT>/matrix.log:
#   <label> clone_s build_s total_s exit verdict
# Usage: matrix.sh <real tree> <view> <target> <reps> [recording]
# Modes: `real`, `vfs-tree` (default tree prefetch), and `vfs-recording` when a recording is given.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
source "$here/common.sh"

real=$1 view=$2 target=$3 reps=$4 recording=${5:-}
mount=${M2_MOUNT:-/tmp/lore-fuse-m2/mnt/fn}
log=$M2_OUT/matrix.log
mkdir -p "$M2_OUT"

# The summary line build.sh prints: label target seconds exit verdict.
run_build() {
    local tree=$1 label=$2
    "$here/build.sh" "$tree" "$target" "$label" | tail -2 | head -1
}

for i in $(seq 1 "$reps"); do
    "$here/reset-real.sh" "$real" || { echo "reset failed" >&2; exit 1; }
    m2_drop_caches
    read -r _ _ seconds status verdict <<< "$(run_build "$real" "real-$i")"
    echo "real-$i 0 $seconds $seconds $status $verdict" >> "$log"
done

modes=(vfs-tree)
[ -n "$recording" ] && modes+=(vfs-recording)
for mode in "${modes[@]}"; do
    for i in $(seq 1 "$reps"); do
        m2_wait_quiet "$M2_OUT/wait-$mode-$i.log"
        prefetch=
        [ "$mode" = vfs-recording ] && prefetch=$recording
        clone=$("$here/cold-vfs.sh" "$view" "$mount" ${prefetch:+"$prefetch"} 2>/dev/null | tail -1)
        read -r _ _ seconds status verdict <<< "$(M2_SKIP_QUIET=1 run_build "$mount" "$mode-$i")"
        echo "$mode-$i $clone $seconds $(echo "$clone + $seconds" | bc) $status $verdict" >> "$log"
    done
done
cat "$log"
