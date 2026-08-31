#!/usr/bin/env bash
# Sweeps the syscall pool cap across a benchmark suite, one cap per process, and reports the
# median of the passes per workload with ratios against the cap the formula yields.
#
# The cap order is reversed on even passes for the same reason the suite modes alternate the engine
# order: drift over the sitting must not line up with the variable under test.
#
#   lore-io/examples/pool-sweep.sh warm 2 8,16,24,32,48,64
#   lore-io/examples/pool-sweep.sh cold 2
#
# Set LORE_BENCH_DIR to place the data on the filesystem under test. `cold` expects `prepare-cold`
# to have run already.
set -u -o pipefail

mode="${1:-warm}"
passes="${2:-2}"
caps="${3:-8,16,24,32,48,64}"
# The cap ratios are quoted against this one, which is what `min(2 x cores, 32)` yields.
baseline="${BASELINE_CAP:-32}"
bench="${BENCH_BIN:-./target/release/examples/bench}"
[ -x "$bench" ] || bench="$bench.exe"
out="${OUT_PREFIX:-/tmp/pool-sweep-$mode}"

IFS=',' read -r -a cap_list <<<"$caps"
: >"$out.tsv"

for pass in $(seq 1 "$passes"); do
  order=("${cap_list[@]}")
  if [ $((pass % 2)) -eq 0 ]; then
    order=()
    for ((i = ${#cap_list[@]} - 1; i >= 0; i--)); do order+=("${cap_list[i]}"); done
  fi
  for cap in "${order[@]}"; do
    echo "=== pass $pass/$passes cap $cap ===" >&2
    LORE_IO_POOL_THREADS="$cap" "$bench" "$mode" loreio 2>&1 | tee -a "$out.log" |
      awk -v cap="$cap" -v pass="$pass" 'NF == 7 && $3 + 0 > 0 && $6 + 0 > 0 { print cap "\t" pass "\t" $2 "\t" $6 }' \
        >>"$out.tsv"
    sync 2>/dev/null || true
    sleep 3
  done
done

awk -v baseline="$baseline" -F'\t' '
{
  key = $3 SUBSEP $1
  values[key] = values[key] " " $4
  if (!(($3) in seen_workload)) { order[++workloads] = $3; seen_workload[$3] = 1 }
  if (!(($1) in seen_cap)) { caps[++capcount] = $1 + 0; seen_cap[$1] = 1 }
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
  for (i = 1; i <= capcount; i++)
    for (j = i + 1; j <= capcount; j++)
      if (caps[j] < caps[i]) { swap = caps[i]; caps[i] = caps[j]; caps[j] = swap }

  printf "%-30s", "workload (ops/s)"
  for (i = 1; i <= capcount; i++) printf " %10s", caps[i]
  printf "\n"
  for (w = 1; w <= workloads; w++) {
    printf "%-30s", order[w]
    for (i = 1; i <= capcount; i++) printf " %10.0f", median(values[order[w], caps[i]])
    printf "\n"
  }

  printf "\n%-30s", "ratio vs cap " baseline
  for (i = 1; i <= capcount; i++) printf " %10s", caps[i]
  printf "\n"
  for (w = 1; w <= workloads; w++) {
    base = median(values[order[w], baseline])
    printf "%-30s", order[w]
    for (i = 1; i <= capcount; i++)
      printf " %9.2fx", (base > 0 ? median(values[order[w], caps[i]]) / base : 0)
    printf "\n"
  }
}
' "$out.tsv"
