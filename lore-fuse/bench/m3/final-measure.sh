#!/usr/bin/env bash
# A revision's closing measurements on one quiet machine, in order, each step gating the next:
# verify.sh, the release binary and latency test, ../m2/latency.sh three times through
# quiet-run.sh, then compare.sh's three pairs of cold builds against a base binary.
# Usage: final-measure.sh <tag> <base binary>
# The release binary is copied to M2_OUT/bin-<tag>/lore, so later builds of the checkout leave it
# alone; M2_OUT/final-<tag>.log records each step.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
source "$here/common.sh"
root=$(cd "$here/../../.." && pwd)

tag=$1 base=$2
log=$M2_OUT/final-$tag.log
bin=$M2_OUT/bin-$tag/lore
mkdir -p "$M2_OUT"
note() { echo "$(date +%T) $*" >> "$log"; }

summary=$("$here/verify.sh" "$tag" | tail -1)
note "verify summary $summary"
if grep -v '^done$' "$summary" | grep -qv 'exit=0$'; then
    note "verify failed; stopping"
    exit 1
fi

cd "$root" || exit 1
cargo build --release -p lore-client > "$M2_OUT/final-$tag-release.log" 2>&1 \
    || { note "release build failed"; exit 1; }
{ mkdir -p "$(dirname "$bin")" && cp target/release/lore "$bin"; } || { note "copy failed"; exit 1; }
note "binary $bin at $(lore history 1 --oneline --offline 2> /dev/null | head -1)"
# Built here, so latency.sh's own build inside the quiet run finds it current.
cargo test --release -p lore-revision --test fuse_lookup_latency --no-run \
    > "$M2_OUT/final-$tag-latency-build.log" 2>&1 || { note "latency build failed"; exit 1; }

for i in 1 2 3; do
    "$here/quiet-run.sh" "latency-$tag" "$root/lore-fuse/bench/m2/latency.sh" "$tag-$i"
    note "latency run $i exit $?"
done

"$here/compare.sh" "$base" "$bin" 3 "cold-$tag"
note "cold builds exit $?"
note "done"
