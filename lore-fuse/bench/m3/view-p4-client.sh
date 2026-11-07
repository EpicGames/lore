#!/usr/bin/env bash
# Prints, for `p4 client -i`, the spec of a p4 client that maps <stream> the way the Lore view
# filter <view> filters the repository. The client is not a stream client: a stream client's view
# is the stream's own and cannot carry the view's exclusions.
# Usage: view-p4-client.sh <view> <stream> <client> <root>
#
# Each view line excludes a directory. `/a/b/` becomes `-<stream>/a/b/...` and `**/a/` becomes
# `-<stream>/.../a/...`; `/a/**/b/` is also written as `/a/b/`, which Lore matches and p4's
# `a/.../b` does not. A top-level directory the view excludes whole is not mapped, and the stream's
# own exclusions under the mapped ones are kept. Both sides match case-insensitively: Lore
# lowercases its globs, and the Fortnite server is case-insensitive. A view with re-inclusions
# (`!`), a line that is not a directory or one with an empty component (`//`, as two lines joined
# by a missing line end give) is refused.
set -eu

view=$1 stream=${2%/} client=$3 root=$4

# The stream's exclusions, from the view p4 generates for a client of the stream that does not
# exist; -o only prints it.
probe=view-p4-client-$$
STREAM_EXCLUDES=$(p4 client -o -S "$stream" "$probe" | grep -E '^[[:space:]]-//' | sed "s#//$probe/#//$client/#") || true
TOP_DIRS=$(p4 dirs "$stream/*" | sed "s#^$stream/##")
export STREAM_EXCLUDES TOP_DIRS

cat << EOF
Client:    $client
Owner:    $(p4 -ztag info | sed -n 's/^\.\.\. userName //p')
Host:    $(hostname)
Description:
    $stream mapped like the Lore view $(basename "$view").
Root:    $root
Options:    noallwrite noclobber nocompress unlocked nomodtime normdir noaltsync
SubmitOptions:    submitunchanged
LineEnd:    local
View:
EOF

awk -v stream="$stream" -v client="$client" '
    function map(path) { printf "\t%s/%s //%s/%s\n", stream, path, client, path }
    function exclude(path) {
        gsub(/\*\*/, "...", path)
        printf "\t-%s/%s... //%s/%s...\n", stream, path, client, path
    }
    function top(path) { path = tolower(path); sub(/\/.*/, "", path); return path }
    {
        sub(/\r$/, "")
        gsub(/^[ \t]+|[ \t]+$/, "")
        if ($0 == "" || $0 ~ /^#/) next
        if ($0 ~ /^!/ || $0 !~ /\/$/ || $0 ~ /\/\// || ($0 !~ /^\// && $0 !~ /^\*\*\//)) {
            print "view-p4-client.sh: unsupported view line: " $0 > "/dev/stderr"
            failed = 1
            exit 1
        }
        globs[++n] = $0
        # A whole top-level directory, such as /Samples/.
        if ($0 ~ /^\/[^\/*]+\/$/) dropped[top(substr($0, 2))] = 1
    }
    END {
        if (failed) exit 1
        map("*")
        dirs = split(ENVIRON["TOP_DIRS"], dir, "\n")
        for (i = 1; i <= dirs; i++) {
            if (dir[i] == "" || top(dir[i]) in dropped) continue
            kept[top(dir[i])] = 1
            map(dir[i] "/...")
        }
        excludes = split(ENVIRON["STREAM_EXCLUDES"], line, "\n")
        for (i = 1; i <= excludes; i++) {
            path = line[i]
            sub(/^[ \t]*-/, "", path)
            sub(/ .*/, "", path)
            if (top(substr(path, length(stream) + 2)) in kept) print line[i]
        }
        for (i = 1; i <= n; i++) {
            glob = globs[i]
            if (glob ~ /^\*\*\//) {
                exclude("..." substr(glob, 3))
                continue
            }
            glob = substr(glob, 2)
            if (!(top(glob) in kept)) continue
            exclude(glob)
            if (index(glob, "/**/")) {
                sub(/\/\*\*\//, "/", glob)
                exclude(glob)
            }
        }
    }' "$view"
