# shellcheck shell=bash
# Shared helpers for the milestone-3 benchmarks. Sourced, not run.
#
# Builds on ../m2/common.sh (M2_ENV, M2_CORES, M2_ACTIONS). Environment (see README "Build
# benchmarks"):
#   M2_OUT     directory for run output, quiet-run.sh's logs included (default /tmp/lore-fuse-m3/runs)
#   BENCH_BIN  lore binary under test; replaces LORE and LORE_SERVICE_EXECUTABLE from M2_ENV
#   M3_MOUNT   mount path of the cold virtual clone (default /tmp/lore-fuse-m3/mnt/fn)

M3_HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
export M2_OUT=${M2_OUT:-/tmp/lore-fuse-m3/runs}
# shellcheck source=../m2/common.sh
source "$M3_HERE/../m2/common.sh"
if [ -n "${BENCH_BIN:-}" ]; then
    export LORE=$BENCH_BIN LORE_SERVICE_EXECUTABLE=$BENCH_BIN
fi
M3_MOUNT=${M3_MOUNT:-/tmp/lore-fuse-m3/mnt/fn}
export QUIET_DIR=${QUIET_DIR:-$M2_OUT}

# Runs cold-build.sh <label> with BENCH_BIN=<binary> through quiet-run.sh under <tag>; any further
# arguments are VAR=value pairs for the run's environment. Returns the kept run's exit status.
m3_quiet_cold_build() {
    local tag=$1 bin=$2 label=$3
    shift 3
    # A killed run leaves the service's mount behind.
    env BENCH_BIN="$bin" "$@" \
        QUIET_CLEANUP="BENCH_BIN='$bin'; source '$M3_HERE/common.sh'; \"\$LORE\" service stop --force; fusermount3 -u -z '$M3_MOUNT'" \
        "$M3_HERE/quiet-run.sh" "$tag" "$M3_HERE/cold-build.sh" "$label"
}
