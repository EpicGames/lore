#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Generate the gitignore ground-truth table used by
`lore-revision/tests/filter_gitignore.rs`.

Ground truth comes from git itself, never from a reading of the spec. Each case
gets a throwaway repository: the patterns are written to `.gitignore`, a fixed
tree is materialized, and `git status --porcelain -uall` is asked which paths
survive. Whatever git reports as untracked is included; everything else git
ignores. That single answer folds in pattern matching, anchoring, last-match
wins and the pruning of excluded directories.

Coverage is combinatorial rather than hand-picked: `*` and `**` and partial
wildcards appear in leading, interior and trailing component positions, at every
depth the tree supports, alone and paired with re-inclusions in both orders, and
in alternating exclude/re-include chains.

Usage:
    python3 scripts/test/gitignore_matrix.py > \\
        lore-revision/tests/data/gitignore_ground_truth.rs
"""
import itertools
import os
import shutil
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor

# ---------------------------------------------------------------- probe tree
# Names are chosen so prefix (`*a`), infix (`*a*`), suffix (`a*`) and extension
# (`*.tmp`) wildcards all bite, and so that `d` has two subdirectories, `e`
# exists at two different depths, and the tree reaches four levels.
FILES = [
    "a", "ab", "xa", "a.tmp", "b.log",
    "d/a", "d/ab", "d/xa", "d/a.tmp", "d/b.log",
    "d/e/a", "d/e/ab", "d/e/a.tmp",
    "d/e/f/a", "d/e/f/a.tmp",
    "d/g/a", "d/g/a.tmp",
    "dd/a", "dd/e/a",
    "e/a", "e/a.tmp",
]
DIRS = ["d", "d/e", "d/e/f", "d/g", "dd", "dd/e", "e"]

# ------------------------------------------------------------ pattern atoms
# One path component each: literals, whole-component wildcards, and partial
# wildcards in leading / trailing / surrounding position.
COMPONENTS = ["d", "e", "a", "*", "**", "*a", "a*", "*a*", "?a", "[ad]"]

# Components used when building multi-level patterns, kept smaller so the
# product stays bounded.
DEEP_COMPONENTS = ["d", "e", "a", "*", "**", "*a"]


def variants(body):
    """A pattern body as authored four ways: anchored or not, directory or not."""
    out = {body, "/" + body, body + "/", "/" + body + "/"}
    return sorted(out)


def single_patterns():
    """Every component alone, and every 2- and 3-component combination, in each
    anchoring/directory spelling."""
    seen = []
    for comp in COMPONENTS:
        seen.extend(variants(comp))
    for a, b in itertools.product(DEEP_COMPONENTS, repeat=2):
        seen.extend(variants(f"{a}/{b}"))
    for a, b, c in itertools.product(DEEP_COMPONENTS, repeat=3):
        # Three-component patterns only in the plain and anchored spellings;
        # the directory spellings are already covered at depth one and two.
        seen.append(f"{a}/{b}/{c}")
        seen.append(f"/{a}/{b}/{c}")
    # Drop degenerate patterns git itself treats as malformed or trivial.
    return sorted({p for p in seen if p.strip("/") and "///" not in p})


# Representative set for the exclusion x inclusion product: every wildcard kind
# in every position, at each depth.
MATRIX = [
    "**", "*", "/*",
    "d", "/d", "d/", "/d/",
    "*a", "a*", "*a*", "*.tmp", "?a", "[ad]",
    "d/*", "d/**", "d/*/", "*/d", "**/d", "*/*",
    "d/e", "/d/e", "d/e/", "d/*/a", "d/**/a", "**/a", "**/e/a", "*/e/a",
    "d/e/*", "d/e/**", "d/e/f", "*/*/a", "**/*.tmp", "d/**/*.tmp",
]

# Smaller set for three-rule alternation, so the product stays bounded.
TRIPLE_EXCLUDE = ["**", "/*", "d", "d/*", "d/**", "**/a", "*.tmp"]
TRIPLE_INCLUDE = ["d", "d/e", "d/**", "d/*", "d/e/*", "d/e/f", "*/e"]
TRIPLE_TAIL = ["d/e/*", "d/e/f", "**/a", "*.tmp", "d/e/f/*", "d/g"]

# Hand-written chains that mirror shapes from the gitignore documentation and
# from real view filters, deeper than the generated triples reach.
CHAINS = [
    ["/*", "!/d", "/d/*", "!/d/e", "/d/e/*", "!/d/e/f"],
    ["/*", "!/d", "/d/*", "!/d/e", "/d/e/*", "!/d/e/f", "/d/e/f/*", "!/d/e/f/a"],
    ["/*", "!/d/", "/d/*", "!/d/e/", "/d/e/*", "!/d/e/f/"],
    ["/*", "!/d", "/d/*", "!/d/e", "*.tmp"],
    ["/*", "!/d", "/d/*", "!/d/e", "**/a"],
    ["/*", "!/d", "/d/*", "!/d/e", "/d/e", "!/d/e/f"],
    ["**", "!d", "!d/e", "!d/e/f"],
    ["**", "!*/", "!a"],
    ["**", "!*/", "!*.tmp"],
    ["**", "!*/", "!d/e/a"],
    ["**", "!d", "!a"],
    ["**", "!keep", "!a"],
    ["*", "!*/", "!a", "!*.tmp"],
    ["d/**", "!d/e/**", "d/e/f/**"],
    ["d/**", "!d/**/a"],
    ["*.tmp", "!d/**", "d/e/*.tmp"],
    ["d/e/f", "!d/e/f/a", "d/e/f/a"],
    ["a", "!a", "a", "!a"],
    ["!a", "a", "!a", "a"],
    ["d/*", "!d/e", "d/e/*", "!d/e/f", "d/e/f/*", "!d/e/f/a.tmp"],
    ["**", "!*/", "!a", "d/e"],
    ["**/*", "!d/**", "d/g/**"],
]


def build_cases():
    cases = []
    for pat in single_patterns():
        cases.append(("single", [pat]))
        cases.append(("single_neg", ["**", "!" + pat]))
    for exc, inc in itertools.product(MATRIX, repeat=2):
        cases.append(("pair", [exc, "!" + inc]))
        cases.append(("pair_rev", ["!" + inc, exc]))
    for exc, inc, tail in itertools.product(
        TRIPLE_EXCLUDE, TRIPLE_INCLUDE, TRIPLE_TAIL
    ):
        cases.append(("triple", [exc, "!" + inc, tail]))
    for chain in CHAINS:
        cases.append(("chain", chain))
    # Deduplicate while preserving order.
    seen = set()
    out = []
    for kind, pats in cases:
        key = tuple(pats)
        if key in seen:
            continue
        seen.add(key)
        out.append((kind, pats))
    return out


def run(case):
    kind, patterns = case
    root = tempfile.mkdtemp(prefix="gimatrix-")
    try:
        subprocess.run(["git", "init", "-q"], cwd=root, check=True,
                       capture_output=True)
        with open(os.path.join(root, ".gitignore"), "w") as f:
            f.write("\n".join(patterns) + "\n")
        for rel in FILES:
            full = os.path.join(root, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            open(full, "a").close()
        st = subprocess.run(["git", "status", "--porcelain", "-uall"],
                            cwd=root, capture_output=True, text=True, check=True)
        untracked = set()
        for line in st.stdout.splitlines():
            if line.startswith("?? "):
                p = line[3:].strip().strip('"')
                if p != ".gitignore":
                    untracked.add(p)
        mask = 0
        for i, rel in enumerate(FILES):
            if rel in untracked:
                mask |= 1 << i
        return (kind, patterns, mask)
    finally:
        shutil.rmtree(root, ignore_errors=True)


def rs_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def rs_list(xs):
    return "&[" + ", ".join(rs_str(x) for x in xs) + "]"


def emit(results):
    w = sys.stdout.write
    w("// SPDX-FileCopyrightText: 2026 Epic Games, Inc.\n")
    w("// SPDX-License-Identifier: MIT\n")
    w("//\n")
    w("// @generated by scripts/test/gitignore_matrix.py -- do not edit by hand.\n")
    w("// Regenerate with:\n")
    w("//   python3 scripts/test/gitignore_matrix.py > \\\n")
    w("//       lore-revision/tests/data/gitignore_ground_truth.rs\n")
    w("//\n")
    w("// GROUND TRUTH FROM GIT. Every expectation below was produced by running\n")
    w("// git itself -- a throwaway repository per case, the patterns written to\n")
    w("// .gitignore, the tree materialized, and `git status --porcelain -uall`\n")
    w("// asked which paths survive. Whatever git reports as untracked is\n")
    w("// included; everything else git ignores. That answer folds in pattern\n")
    w("// matching, anchoring, last-match-wins and the pruning of excluded\n")
    w("// directories, so it is the whole of gitignore's observable behaviour and\n")
    w("// not one person's reading of the spec.\n")
    w("//\n")
    w("// gitignore is the standard lore follows, so these are the expectations\n")
    w("// lore is held to. Where lore deliberately departs, the departure is\n")
    w("// asserted as a bounded property in the test, not waived per case.\n")
    w("\n")
    w("/// One pattern set and, as a bitmask over [`FILES`], the paths git leaves\n")
    w("/// included under it.\n")
    w("pub struct Case {\n")
    w("    pub kind: &'static str,\n")
    w("    pub patterns: &'static [&'static str],\n")
    w("    pub included: u32,\n")
    w("}\n\n")
    w("/// Every file in the probe tree, in bitmask order. Names are chosen so\n")
    w("/// prefix, infix, suffix and extension wildcards all bite, at depths one\n")
    w("/// through four, with two sibling subdirectories under `d` and an `e` at\n")
    w("/// two different depths.\n")
    w(f"pub const FILES: &[&str] = {rs_list(FILES)};\n\n")
    w("/// Every directory in the probe tree.\n")
    w(f"pub const DIRS: &[&str] = {rs_list(DIRS)};\n\n")
    w("pub const CASES: &[Case] = &[\n")
    for kind, patterns, mask in results:
        w(f"    Case {{ kind: {rs_str(kind)}, patterns: {rs_list(patterns)}, "
          f"included: {mask:#x} }},\n")
    w("];\n")


def main():
    cases = build_cases()
    print(f"{len(cases)} cases", file=sys.stderr)
    with ThreadPoolExecutor(max_workers=32) as pool:
        results = list(pool.map(run, cases))
    emit(results)


if __name__ == "__main__":
    main()
