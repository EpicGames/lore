#!/usr/bin/env bash
# Runs one test of a test binary <runs> times, alone, from the current directory; run it from the
# test's crate directory, as cargo does. One line per run in <out>/summary.log, then "done"; each
# failing run's output is kept as <out>/fail-<n>.log.
# Usage: repeat-test.sh <test binary> <test name> <runs> <out dir>
set -u
bin=$1 name=$2 runs=$3 out=$4
mkdir -p "$out"
for i in $(seq 1 "$runs"); do
    "$bin" --exact "$name" > "$out/run.log" 2>&1
    status=$?
    echo "run $i exit=$status" >> "$out/summary.log"
    [ "$status" -eq 0 ] || cp "$out/run.log" "$out/fail-$i.log"
done
echo "done" >> "$out/summary.log"
