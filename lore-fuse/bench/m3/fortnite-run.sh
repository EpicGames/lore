#!/usr/bin/env bash
# One run of fortnite-experiments.md, end to end: the sync or clone, the project file generation
# and the build, then the disk the run occupies, then, unless the run failed, the cleanup that
# frees it for the next run. Everything the run produces lands in its own directory,
# $FN_RUNS/<run>-<time>/, kept whether the run succeeds or not, so a run that fails is rerun
# alone. The measurement runs on any exit, an error or an interrupt included.
#
# A run that fails keeps its tree, or the shared store and external directories of a FUSE run,
# for investigation; one whose status is discard is cleaned up, since only a cold rerun replaces
# it. A run refuses to start while data it would delete or reuse exists (p4: its workspace root;
# lore: its tree and the shared store; the FUSE runs: the shared store and external directories),
# whether a failed run kept it or a killed one left it. `fortnite-run.sh clean` deletes all of it.
#
# fuse-warm clones over FUSE like the other FUSE runs, with no prefetch and no recording, over the
# shared store a successful fuse-prefetch keeps for it: warm fetched data, a cold page cache and a
# build from scratch. It refuses to start without that store, and its cleanup deletes the store.
#
# local-build generates the project files and builds in the ordinary clone at FN_LORE_TREE after
# dropping the page cache: the cold build on an ordinary file system the FUSE runs compare with.
# It refuses to start unless that tree is at BENCH_REV with no changes and no build outputs, and
# keeps the tree, the build's outputs inside it: clean it before another local-build.
# Usage: fortnite-run.sh p4|lore|fuse-record|fuse-prefetch|fuse-warm|local-build|clean
#
# Environment: M2_ENV, the experiments' env.sh, which sets LORE, LORE_GLOBAL_PATH, XDG_DATA_HOME,
# BENCH_REV, P4CLIENT, M2_CORES, M2_ACTIONS and FN_RUNS, FN_VIEW, FN_TARGET, FN_CL, FN_MOUNT,
# FN_LORE_TREE and FN_MIN_FREE_GIB (the space p4 and lore check for before they start). Optional:
#   FN_NET                  interface whose received bytes the FUSE runs sample (default: the
#                           route to the server)
#   FN_RECORDING            the recording fuse-prefetch reads (default: that of the latest
#                           fuse-record run whose status is ok)
#   FN_TREE_PREFETCH_DEPTH  LORE_TREE_PREFETCH_DEPTH of the FUSE runs (default 0; set and
#                           empty for the service's default)
#
# The run directory holds:
#   results.log      the timing lines, the disk used and the status, also appended, prefixed with
#                    the run directory's name, to $FN_RUNS/results.log
#   status           ok, or the reason the run failed or is to be discarded
#   inputs/          the view, the environment, lore --version and, for p4, the client spec
#   lore-logs/       the Lore log the run wrote (lore-logs-before/: one found at the start)
#   recording/       fuse-record's access recording
#   <run>-<time>/    build.sh's logs: build.log, ubt.log, gpf.log, gpf-ubt.log, steps.log and
#                    summary.log; its uba/ is deleted
#   *.log            the sync or clone log, and cold-build.sh's service, clone, env and net logs
# Exits 0 when the run succeeded undisturbed.
set -u
here=$(cd "$(dirname "$0")" && pwd)

run=${1:-}
case $run in
    p4 | lore | fuse-record | fuse-prefetch | fuse-warm | local-build | clean) ;;
    *)
        echo "usage: $0 p4|lore|fuse-record|fuse-prefetch|fuse-warm|local-build|clean" >&2
        exit 64
        ;;
esac
: "${M2_ENV:?set M2_ENV to the experiments env.sh}"
# shellcheck source=/dev/null
source "$M2_ENV"
: "${FN_RUNS:?}" "${FN_VIEW:?}" "${FN_TARGET:?}" "${FN_CL:?}" "${FN_MOUNT:?}" "${FN_LORE_TREE:?}"
: "${FN_MIN_FREE_GIB:?}" "${BENCH_REV:?}" "${P4CLIENT:?}" "${XDG_DATA_HOME:?}" "${LORE_GLOBAL_PATH:?}"
dir=
export M2_OUT=$FN_RUNS
if [ "$run" != clean ]; then
    dir=$FN_RUNS/$run-$(date +%Y%m%d-%H%M%S)
    mkdir -p "$dir/inputs"
    export M2_OUT=$dir
fi
# shellcheck source=common.sh
source "$here/common.sh"

url=${M2_URL:-lore.internal.epicgames.net/Fortnite}
logs=$XDG_DATA_HOME/lore/logs
data=$LORE_GLOBAL_PATH/data
reason=
started=
keep_store=
p4_root=$(p4 -ztag client -o "$P4CLIENT" | sed -n 's/^\.\.\. Root //p')
tree=
keep_tree=
case $run in
    p4) tree=$p4_root ;;
    lore) tree=$FN_LORE_TREE ;;
    local-build)
        tree=$FN_LORE_TREE
        keep_tree=1
        ;;
esac

# Appends a line to the run's results and to the experiments' results.
note() {
    echo "$(basename "$dir") $*" | tee -a "$dir/results.log" "$FN_RUNS/results.log"
}

# Ends the run as failed (or discarded) with the reason; the exit handler records it.
fail() {
    reason=$*
    exit 1
}

# Stops this environment's service and detaches its mount, so nothing below reads or deletes
# through the mount.
stop_service() {
    local waited=0
    "$LORE" service stop --force > /dev/null 2>&1
    while [ -n "$(m2_service_pids)" ] && [ $waited -lt 60 ]; do
        sleep 1
        waited=$((waited + 1))
    done
    if grep -q " $FN_MOUNT fuse" /proc/self/mounts; then fusermount3 -u -z "$FN_MOUNT"; fi
}

# Notes the disk, in MiB, the run occupies: its tree (a p4 workspace or an ordinary clone, with the
# store and the build outputs inside it), the shared store and external directories (a FUSE run's
# fetched data and build outputs) and its builds' UBA directories. Never the mount.
disk_used() {
    local path line=disk_mib
    local ubas=()
    [ -z "$dir" ] || ubas=("$dir"/*/uba)
    for path in ${tree:+"$tree"} "$data/shared_store" "$data/external" "${ubas[@]}"; do
        [ -e "$path" ] && line+=" $path=$(du -sm "$path" | cut -f1)"
    done
    note "$line"
}

# Deletes what the run wrote outside its directory, and its UBA directories; for p4 empties the
# have list first. The store registry is emptied with the shared store, as cold-vfs.sh does: an
# entry for a deleted store fails the next clone over the shared store. rmdir refuses a mount
# point that is not empty.
remove() {
    local ubas=() store=("$data/shared_store")
    cd /
    if [ "$tree" = "$p4_root" ]; then p4 -c "$P4CLIENT" flush -q "//$P4CLIENT/...#none" > /dev/null 2>&1; fi
    [ -z "$dir" ] || ubas=("$dir"/*/uba)
    [ -z "$keep_store" ] || store=()
    local trees=()
    [ -z "$tree" ] || [ -n "$keep_tree" ] || trees=("$tree")
    rm -rf "${trees[@]}" "${store[@]}" "$data/external" "${ubas[@]}"
    mkdir -p "$data/stores" && echo 'entries = []' > "$data/stores/registry.toml"
    [ ! -d "$FN_MOUNT" ] || rmdir "$FN_MOUNT"
}

finish() {
    local code=$?
    trap - EXIT INT TERM
    stop_service
    [ -z "$started" ] || disk_used
    [ ! -d "$logs" ] || mv "$logs" "$dir/lore-logs"
    [ -n "$reason" ] || [ $code -eq 0 ] || reason="failed: exit $code"
    case $reason in
        failed:*) ;;
        *)
            # A successful fuse-prefetch keeps its shared store, warm, for fuse-warm.
            [ -n "$reason" ] || [ "$run" != fuse-prefetch ] || keep_store=1
            remove
            ;;
    esac
    echo "${reason:-ok}" > "$dir/status"
    note "status ${reason:-ok}"
    case $started$reason in
        1failed:*) note "kept: $(existing ${tree:+"$tree"} "$data/shared_store" "$data/external")" ;;
    esac
    [ -z "$keep_store" ] || note "kept for fuse-warm: $data/shared_store"
    [ "$started" != 1 ] || [ -z "$keep_tree" ] || note "kept: $tree, with its build outputs"
    note "free $(df -h --output=avail "$FN_RUNS" | tail -1 | tr -d ' ')"
    exit $code
}

# Times the project file generation and the build with build.sh, which waits for a quiet machine
# first, and notes each step's seconds and the run's total: the sync or clone plus the steps.
build() {    # build <label> <tree> <sync or clone seconds>
    local output summary steps code verdict
    output=$("$here/../m2/build.sh" "$2" "$FN_TARGET" "$1")
    summary=$(tail -2 <<< "$output" | head -1)
    steps=$(tail -1 <<< "$output")/steps.log
    note "$summary"
    if [ -s "$steps" ]; then
        note "steps $(tr '\n' ' ' < "$steps")total_s=$(awk -v fetch="$3" -F '[= ]' \
            '{ s += $2 } END { printf "%.3f", fetch + s }' "$steps")"
    fi
    read -r _ _ _ code verdict <<< "$summary"
    [ "${code:-}" = 0 ] || fail "failed: build exit ${code:-unknown}"
    [ "${verdict:-}" = KEEP ] || fail "discard: a foreign build or test process ran during the build"
}

# The newest file of a glob's matches, which sort by time: run directories by their names'
# <time>, recordings by their <unix seconds>. Empty when nothing matches.
newest() {
    [ -e "${!#}" ] && echo "${!#}"
}

# The recording of the latest fuse-record run whose status is ok.
latest_recording() {
    local runs=("$FN_RUNS"/fuse-record-*/) i
    for ((i = ${#runs[@]} - 1; i >= 0; i--)); do
        [ "$(cat "${runs[i]}status" 2> /dev/null)" = ok ] || continue
        newest "${runs[i]}"recording/*.txt
        return
    done
}

# The paths among the arguments that exist, space-separated.
existing() {
    local path found=()
    for path in "$@"; do
        [ ! -e "$path" ] || found+=("$path")
    done
    echo "${found[*]}"
}

# A tree this script deletes must be at least two levels deep and not the mount.
for path in "$p4_root" "$FN_LORE_TREE"; do
    if [[ $path != /*/?* ]] || [ "$path" = "$FN_MOUNT" ]; then
        echo "$0: refusing to use $path as a tree" >&2
        exit 64
    fi
done

if [ "$run" = clean ]; then
    stop_service
    echo "removing: $(existing "$p4_root" "$FN_LORE_TREE" "$data/shared_store" "$data/external")"
    tree=$p4_root remove
    tree=$FN_LORE_TREE remove
    exit 0
fi

trap finish EXIT
trap 'reason="failed: interrupted"; exit 130' INT TERM

# Inputs, for reading the run later.
cp "$FN_VIEW" "$dir/inputs/"
"$LORE" --version > "$dir/inputs/lore-version" 2>&1
env | grep -E '^(LORE|FN_|M2_|M3_|BENCH_|P4|XDG_|TMPDIR|DOTNET|NUGET|LINUX_MULTIARCH)' \
    | grep -v -i -E 'passwd|token|secret' | sort > "$dir/inputs/env"
[ "$run" != p4 ] || p4 client -o "$P4CLIENT" > "$dir/inputs/p4-client.spec"

# Data a failed run kept or a killed one left, which this run would delete or reuse: refused, not
# deleted. A log written since the last run is kept apart.
stop_service
case $run in
    local-build) blocking= ;;
    p4) blocking=$(existing "$p4_root") ;;
    lore) blocking=$(existing "$FN_LORE_TREE" "$data/shared_store") ;;
    fuse-warm) blocking=$(existing "$data/external") ;;
    *) blocking=$(existing "$data/shared_store" "$data/external") ;;
esac
[ -z "$blocking" ] || fail "failed: kept data exists: $blocking; investigate it, then delete it with $0 clean"
if [ "$run" = fuse-warm ] && [ ! -d "$data/shared_store" ]; then
    fail "failed: no warm store; fuse-warm runs over the shared store a successful fuse-prefetch keeps"
fi
if [ "$run" = local-build ]; then
    # The tree must be the revision alone: no change Lore sees, and no build output it ignores.
    [ -d "$tree" ] || fail "failed: no tree at $tree; clone BENCH_REV there first"
    for output in "$tree/Engine/Intermediate" "$tree/FortniteGame/Intermediate" "$tree/FortniteGame/Binaries/Linux"; do
        [ ! -e "$output" ] || fail "failed: build output $output exists; clean the tree first"
    done
    (cd "$tree" && LORE_USE_SERVICE=0 "$LORE" status --scan) > "$dir/status-before.txt" 2>&1
    if ! grep -q -- "-> $BENCH_REV" "$dir/status-before.txt" ||
        ! grep -q '^No tracked changes' "$dir/status-before.txt"; then
        fail "failed: $tree is not BENCH_REV without changes; see status-before.txt"
    fi
fi
started=1
[ ! -d "$FN_MOUNT" ] || rmdir "$FN_MOUNT"
mkdir -p "$data/stores" && echo 'entries = []' > "$data/stores/registry.toml"
[ ! -d "$logs" ] || mv "$logs" "$dir/lore-logs-before"

if [ "$run" = p4 ] || [ "$run" = lore ]; then
    free=$(df -BG --output=avail "$FN_RUNS" | tail -1 | tr -dc 0-9)
    [ "$free" -ge "$FN_MIN_FREE_GIB" ] || fail "failed: ${free} GiB free, under FN_MIN_FREE_GIB=$FN_MIN_FREE_GIB"
fi

case $run in
    p4)
        if ! mkdir -p "$tree" || ! cd "$tree"; then fail "failed: no p4 root $tree"; fi
        m2_drop_caches || fail "failed: cache drop"
        start=$(m2_now)
        p4 -c "$P4CLIENT" sync --parallel=threads=16 "//$P4CLIENT/...@$FN_CL" > "$dir/p4-sync.log" 2>&1
        code=$?
        sync_s=$(m2_elapsed "$start" "$(m2_now)")
        note "sync_s=$sync_s exit=$code"
        [ $code -eq 0 ] || fail "failed: p4 sync exit $code"
        build p4 "$tree" "$sync_s"
        ;;
    lore)
        m2_drop_caches || fail "failed: cache drop"
        start=$(m2_now)
        LORE_USE_SERVICE=0 "$LORE" clone "$url" "$tree" --revision "$BENCH_REV" --view "$FN_VIEW" \
            --max-connections 4 > "$dir/lore-clone.log" 2>&1
        code=$?
        clone_s=$(m2_elapsed "$start" "$(m2_now)")
        note "clone_s=$clone_s exit=$code"
        [ $code -eq 0 ] || fail "failed: lore clone exit $code"
        build lore "$tree" "$clone_s"
        ;;
    local-build)
        m2_drop_caches || fail "failed: cache drop"
        build local-build "$tree" 0
        ;;
    fuse-record | fuse-prefetch | fuse-warm)
        depth=${FN_TREE_PREFETCH_DEPTH-0}
        if [ -n "$depth" ]; then
            export LORE_TREE_PREFETCH_DEPTH=$depth
        else
            unset LORE_TREE_PREFETCH_DEPTH
        fi
        net=${FN_NET:-$(ip route get "$(getent hosts "${url%%/*}" | awk '{print $1; exit}')" \
            | sed -n 's/.* dev \([^ ]*\).*/\1/p')}
        recording=
        if [ "$run" = fuse-record ]; then
            # The service records only into a directory that exists.
            mkdir -p "$dir/recording"
            export LORE_FUSE_RECORD_DIR=$dir/recording
        elif [ "$run" = fuse-prefetch ]; then
            unset LORE_FUSE_RECORD_DIR
            recording=${FN_RECORDING:-$(latest_recording)}
            [ -s "$recording" ] || fail "failed: no recording; run fuse-record first or set FN_RECORDING"
            note "recording $recording"
        else
            # cold-vfs.sh keeps the store, and empties the registry; the clone finds the store again.
            unset LORE_FUSE_RECORD_DIR
            export M2_KEEP_STORE=1
            note "warm store_mib=$(du -sm "$data/shared_store" | cut -f1)"
        fi
        # cold-build.sh drops the page cache in cold-vfs.sh, which does not check the drop.
        sudo -n /usr/sbin/sysctl -q vm.drop_caches=3 || fail "failed: cache drop"
        summary=$(M3_VIEW=$FN_VIEW M3_RECORDING=$recording M3_TARGET=$FN_TARGET M3_MOUNT=$FN_MOUNT \
            M3_NET=$net "$here/cold-build.sh" "$run")
        code=$?
        note "$summary"
        [ $code -eq 0 ] || fail "failed: cold-build.sh exit $code"
        [ "${summary##* }" = KEEP ] || fail "discard: a foreign build or test process ran during the build"
        if [ "$run" = fuse-record ] && ! [ -s "$(newest "$dir"/recording/*.txt)" ]; then
            fail "failed: no recording written"
        fi
        ;;
esac
