#!/usr/bin/env bash
# Times one Unreal build of <target> in <tree> on a quiet machine.
# Usage: build.sh <tree> <target> <label> [extra UBT arguments...]
# M2_SKIP_QUIET=1 skips the quiet wait, for a caller that waited before a step it times too.
# M2_UBT_ARGS holds further UnrealBuildTool arguments for the build, space-separated (-NoLink).
# M2_IGNORE_LINK_FAILURE=1 counts a build whose only errors are the linker's as succeeded: its
# steps.log line keeps the real exit and adds link_failure_ignored, and the summary gives exit 0.
# M2_GENERATE_PROJECT_FILES=1 generates the project files first, as a step timed of its own, with
# the tree's root GenerateProjectFiles.sh: it sets up the debugger helpers (~/.lldbinit when
# missing, ~/.config/Epic/GDBPrinters), then builds UnrealBuildTool and runs it with -projectfiles.
# Writes <M2_OUT>/<label>-<time>/: build.log, ubt.log (UnrealBuildTool's own log, timestamped),
# steps.log (one line per step, <step>_s=<seconds> exit=<status>), summary.log, and with the
# generation gpf.log and gpf-ubt.log. The summary line is
#   label target seconds exit KEEP|DISCARD
# with the build's seconds and exit; a generation that fails gives its exit, and the build is
# skipped. DISCARD marks a run that overlapped a foreign build or test process.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
source "$here/common.sh"

tree=$1 target=$2 label=$3
shift 3
out=$M2_OUT/$label-$(date +%Y%m%d-%H%M%S)
mkdir -p "$out"

project=()
case $target in
    Fortnite*) project=(-project="$tree/FortniteGame/FortniteGame.uproject") ;;
esac

# UBT otherwise sizes its parallelism by free memory, which varies from run to run, and keeps
# a UBA content store under ~/.epic that would warm every later build.
ubt=(-MaxParallelActions="${M2_ACTIONS:-12}" -UBARootDir="$out/uba")
read -r -a extra <<< "${M2_UBT_ARGS:-}"
ubt+=("${extra[@]}")

[ -n "${M2_SKIP_QUIET:-}" ] || m2_wait_quiet "$out/wait.log"
monitor=$(m2_start_intruder_monitor "$out/intruder")

# Whether a failed build's log holds linker errors and no compiler error.
link_only_failure() {
    grep -a -q -E '^ld\.lld: error:|linker command failed' "$1" &&
        ! grep -a -q -E ':[0-9]+:[0-9]+: (fatal )?error:|[0-9]+ errors? generated' "$1"
}
status=0
seconds=0
if [ -n "${M2_GENERATE_PROJECT_FILES:-}" ]; then
    generate=("${project[@]}")
    [ ${#project[@]} -eq 0 ] || generate+=(-game)
    start=$(m2_now)
    taskset -c "$M2_CORES" "$tree/GenerateProjectFiles.sh" \
        "${generate[@]}" -Log="$out/gpf-ubt.log" -Timestamps > "$out/gpf.log" 2>&1
    status=$?
    echo "gpf_s=$(m2_elapsed "$start" "$(m2_now)") exit=$status" | tee -a "$out/steps.log"
fi
if [ $status -eq 0 ]; then
    start=$(m2_now)
    ( cd "$tree" && taskset -c "$M2_CORES" Engine/Build/BatchFiles/RunUBT.sh \
        "$target" Linux Development "${project[@]}" "${ubt[@]}" -Log="$out/ubt.log" -Timestamps "$@" ) \
        > "$out/build.log" 2>&1
    status=$?
    seconds=$(m2_elapsed "$start" "$(m2_now)")
    if [ $status -ne 0 ] && [ -n "${M2_IGNORE_LINK_FAILURE:-}" ] && link_only_failure "$out/build.log"; then
        echo "build_s=$seconds exit=$status link_failure_ignored" | tee -a "$out/steps.log"
        status=0
    else
        echo "build_s=$seconds exit=$status" | tee -a "$out/steps.log"
    fi
fi
kill "$monitor" 2>/dev/null
wait "$monitor" 2>/dev/null
verdict=KEEP
[ -s "$out/intruder" ] && verdict=DISCARD
echo "$label $target $seconds $status $verdict" | tee "$out/summary.log"
echo "$out"
