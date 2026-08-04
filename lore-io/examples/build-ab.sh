#!/usr/bin/env bash
# Interleaved A/B of two bench binaries, which is what BENCHMARKS.md asks for when the variable is
# the build rather than the engine: both binaries in one sitting, the order alternated per round,
# and an engine whose code is identical in both run alongside as the noise floor. If the unchanged
# engine's medians differ between the two groups by more than the effect, the experiment answered
# nothing — comparing two suites run an hour apart is how that goes wrong.
#
#   lore-io/examples/build-ab.sh 4 <baseline-bench-binary> ./target/release/examples/bench
#
# Keep the baseline binary by copying it out of target/ before rebuilding; cargo overwrites it.
set -u -o pipefail

rounds="${1:-4}"
a="${2:?baseline binary}"
b="${3:?candidate binary}"
mode="${MODE:-warm}"
# loreio is the engine under test; blocking is identical in both builds and is the control.
engines="${ENGINES:-loreio blocking}"
out="${OUT_PREFIX:-/tmp/build-ab}"

: >"$out.tsv"

run_one() {
  local label="$1" binary="$2" engine="$3" round="$4"
  echo "=== round $round  $label  $engine ===" >&2
  LORE_BENCH_DIR="${LORE_BENCH_DIR:-}" "$binary" "$mode" "$engine" 2>&1 |
    tee -a "$out.log" |
    awk -v label="$label" -v engine="$engine" -v round="$round" \
      'NF == 7 && $3 + 0 > 0 && $6 + 0 > 0 { print label "\t" engine "\t" round "\t" $2 "\t" $6 }' \
      >>"$out.tsv"
  sleep 3
}

for round in $(seq 1 "$rounds"); do
  for engine in $engines; do
    if [ $((round % 2)) -eq 1 ]; then
      run_one A "$a" "$engine" "$round"
      run_one B "$b" "$engine" "$round"
    else
      run_one B "$b" "$engine" "$round"
      run_one A "$a" "$engine" "$round"
    fi
  done
done

awk -F'\t' '
{
  values[$4, $2, $1] = values[$4, $2, $1] " " $5
  if (!(($4) in seen_workload)) { workload_order[++workloads] = $4; seen_workload[$4] = 1 }
  if (!(($2) in seen_engine)) { engine_order[++engines] = $2; seen_engine[$2] = 1 }
}
function median(list, parts, count, i, j, swap) {
  count = split(list, parts, " ")
  for (i = 1; i <= count; i++)
    for (j = i + 1; j <= count; j++)
      if (parts[j] + 0 < parts[i] + 0) { swap = parts[i]; parts[i] = parts[j]; parts[j] = swap }
  if (count == 0) return 0
  if (count % 2) return parts[(count + 1) / 2] + 0
  return (parts[count / 2] + parts[count / 2 + 1]) / 2
}
END {
  printf "%-28s %-10s %12s %12s %9s\n", "workload", "engine", "A", "B", "B/A"
  for (e = 1; e <= engines; e++) {
    for (w = 1; w <= workloads; w++) {
      first = median(values[workload_order[w], engine_order[e], "A"])
      second = median(values[workload_order[w], engine_order[e], "B"])
      printf "%-28s %-10s %12.0f %12.0f %8.2fx\n", workload_order[w], engine_order[e], first, second,
        (first > 0 ? second / first : 0)
    }
  }
  printf "\nthe control engine'"'"'s B/A column is this experiment'"'"'s noise floor\n"
}
' "$out.tsv"
