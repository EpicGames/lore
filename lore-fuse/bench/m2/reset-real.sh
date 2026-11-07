#!/usr/bin/env bash
# Returns a real clone to the revision it holds. Build outputs are mostly paths the repository
# ignores (`.p4ignore.txt`: Intermediate, Saved, ...), which no scan reports, so everything
# changed after the marker the last reset left is deleted; a scan then marks what that took of
# tracked files, `lore reset` restores them, and a final scan must report nothing.
# Usage: reset-real.sh <tree> [time]
# Without a marker yet, `time` (a `find -newermt` value) names the first moment to remove from.
# The tree is an ordinary instance under the default global directory, so this runs the
# commands in this process, not through the benchmark service.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
source "$here/common.sh"
unset LORE_GLOBAL_PATH
export LORE_USE_SERVICE=0

tree=$1 since=${2:-}
marker=$M2_OUT/real-marker
cd "$tree" || exit 1
if [ -e "$marker" ]; then
    newer=(-newer "$marker")
elif [ -n "$since" ]; then
    newer=(-newermt "$since")
else
    echo "no marker at $marker and no start time given" >&2
    exit 1
fi

# One walk, children before parents: files go, then directories in that order once empty.
lists=$(mktemp -d)
find . -depth ! -path './.lore' ! -path './.lore/*' "${newer[@]}" \
    \( -type d -fprint0 "$lists/dirs" -o -fprint0 "$lists/files" \)
xargs -0 -r rm -f < "$lists/files"
xargs -0 -r rmdir --ignore-fail-on-non-empty < "$lists/dirs"
rm -rf "$lists"
"$LORE" status --scan > /dev/null 2>&1 || exit 1
"$LORE" reset --purge . > /dev/null 2>&1 || exit 1
after=$("$LORE" status --scan 2>&1) || exit 1
if ! grep -q '^No tracked changes' <<< "$after" || grep -q '^Untracked files:' <<< "$after"; then
    echo "$after"
    exit 1
fi
touch "$marker"
