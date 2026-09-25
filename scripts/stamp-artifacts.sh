#!/usr/bin/env bash
#
# scripts/stamp-artifacts.sh — Stamp the current Lore revision into built artifacts.
#
# Reads the number of the revision the working tree is at from `lore revision info` and has
# lore-stamp write it as the build name into each artifact, which then reports
# `<package version>+<revision>`. With no arguments, stamps every artifact of a full release build
# (`cargo build --release`) in target/release, or in $CARGO_TARGET_DIR/release when that is set.
#
# lore-stamp is built in a target directory of its own: built in the artifacts' target directory
# with the features of `-p lore-base` alone, it would displace dependencies the full build compiled
# with the workspace's features, and the next full build would compile them again.
#
# Usage: scripts/stamp-artifacts.sh [artifact...]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly REPO_ROOT
TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
readonly TARGET_DIR

revision="$(lore revision info --repository "${REPO_ROOT}" |
    awk '$1 == "Revision" && $2 == ":" { print $3; exit }')"
if [[ -z "${revision}" ]]; then
    printf 'stamp-artifacts: lore revision info reported no revision for %s\n' "${REPO_ROOT}" >&2
    exit 1
fi

if (($# == 0)); then
    case "$(uname -s)" in
        Darwin) artifacts=(lore loreserver lore_chaos_client liblore.dylib liblore.a) ;;
        MINGW* | MSYS* | CYGWIN*) artifacts=(lore.exe loreserver.exe lore_chaos_client.exe lore.dll lore.lib) ;;
        *) artifacts=(lore loreserver lore_chaos_client liblore.so liblore.a) ;;
    esac
    set -- "${artifacts[@]/#/${TARGET_DIR}/release/}"
fi

CARGO_TARGET_DIR="${TARGET_DIR}/lore-stamp" exec cargo run --quiet --release \
    --manifest-path "${REPO_ROOT}/Cargo.toml" -p lore-base --bin lore-stamp -- \
    --build "${revision}" "$@"
