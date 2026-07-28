# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests for parallel multi-path staging.

Builds a large two-level directory tree (thousands of files and directories)
and stages large, overlapping explicit path sets — with and without ``--scan``,
across add / modify / delete — to exercise the parallel-staging path: input
normalization (antichain collapse of overlapping paths), sequential pre-creation
of shared ancestor directories, and the parallel per-target fan-out.

Every operation is verified against the exact expected staged set via ``--json``
status: precisely the files and directories expected, each with the expected
node type and action, with no duplicates, extras, or omissions.
"""

import logging
import os

import pytest
from lore_parsers import parse_status_json
from test_utils import to_posix

from lore import Lore

logger = logging.getLogger(__name__)

# Tree shape: two levels deep, sized so both directory and file counts run into
# the hundreds to exercise the parallel fan-out and shared-ancestor pre-creation
# while keeping the full explicit-path arg lists (staged relative to the repo
# root) well under the Windows CreateProcess command-line length limit.
TOP_DIRS = 12
SUB_DIRS = 10
FILES_PER_SUB = 2


def _build_tree(repo: Lore):
    """Write the full tree to disk. Returns (files, dirs, tops, subs) with files
    a dict[path -> content] and the rest lists of posix paths (dirs excludes the
    repo root)."""
    files: dict[str, str] = {}
    tops: list[str] = []
    subs: list[str] = []
    for t in range(TOP_DIRS):
        top = f"d{t:03d}"
        tops.append(top)
        for s in range(SUB_DIRS):
            sub = f"{top}/s{s:03d}"
            subs.append(sub)
            for f in range(FILES_PER_SUB):
                files[f"{sub}/f{f:03d}.txt"] = f"content {t}-{s}-{f}\n"
    repo.write_files(files)
    return files, tops, subs


def _staged_map(repo: Lore) -> dict[str, tuple[str | None, str | None]]:
    """Return {posix_path -> (type, action)} for every staged status entry,
    asserting no path is reported more than once."""
    entries = parse_status_json(repo.status(json=True, offline=True))
    result: dict[str, tuple] = {}
    dups: list[str] = []
    for e in entries:
        if not e.get("flagStaged"):
            continue
        path = to_posix(e.get("path", ""))
        if not path:
            continue
        if path in result:
            dups.append(path)
        result[path] = (e.get("type"), e.get("action"))
    assert not dups, f"duplicate staged paths: {sorted(set(dups))}"
    return result


def _assert_staged_exactly(repo: Lore, expected: dict[str, tuple[str, str]]):
    """Assert the full set of staged entries equals `expected` ({path -> (type,
    action)}) — no missing, no extra, no mismatched type/action."""
    got = _staged_map(repo)
    missing = {k: expected[k] for k in expected if k not in got}
    extra = {k: got[k] for k in got if k not in expected}
    wrong = {k: (got[k], expected[k]) for k in expected if k in got and got[k] != expected[k]}
    assert not (missing or extra or wrong), (
        f"staged set mismatch (got {len(got)}, want {len(expected)}):\n"
        f"  missing={sorted(missing.items())[:12]}\n"
        f"  extra={sorted(extra.items())[:12]}\n"
        f"  wrong={sorted(wrong.items())[:12]}"
    )


def _covered(path: str, prefixes: list[str]) -> bool:
    return any(path == p or path.startswith(p + "/") for p in prefixes)


def _full_tree_adds(files, tops, subs) -> dict[str, tuple[str, str]]:
    """Expected staged map when the whole freshly-created tree is added."""
    expected = {to_posix(p): ("file", "add") for p in files}
    for d in tops + subs:
        expected[to_posix(d)] = ("directory", "add")
    return expected


@pytest.mark.smoke
def test_stage_scan_add_overlapping(new_lore_repo):
    """`--scan` the whole new tree through an overlapping arg set (every top dir
    plus a sample of already-covered subdirs and files). The overlap must
    collapse and the entire tree must be staged exactly once, all as adds."""
    repo: Lore = new_lore_repo()
    files, tops, subs = _build_tree(repo)

    args = tops + subs[::37] + sorted(files)[::101]
    repo.stage(args, scan=True, offline=True, relative_paths=True)

    _assert_staged_exactly(repo, _full_tree_adds(files, tops, subs))


@pytest.mark.smoke
def test_stage_noscan_add_explicit_files(new_lore_repo):
    """Stage every leaf file explicitly, without `--scan`. The targets form a
    file antichain sharing `d*/s*` prefixes, so all shared ancestor directories
    are pre-created sequentially before the parallel per-file walks. The whole
    tree (files and their directories) must end up staged exactly once as adds."""
    repo: Lore = new_lore_repo()
    files, tops, subs = _build_tree(repo)

    repo.stage(sorted(files), offline=True, relative_paths=True)

    _assert_staged_exactly(repo, _full_tree_adds(files, tops, subs))


def _apply_mixed_changes(repo: Lore, files):
    """Apply a deterministic modify/add/delete mix to a committed tree and return
    the expected staged entries (keyed by posix path -> (type, action)) for the
    whole tree."""
    files_sorted = sorted(files)
    modified = set(files_sorted[::5])
    deleted = set(files_sorted[3::13]) - modified

    new_subdirs = [f"d{t:03d}/nsub" for t in range(0, TOP_DIRS, 4)]
    new_files = {f"{d}/nf{i:02d}.txt": "new file\n" for d in new_subdirs for i in range(3)}

    repo.write_files({p: "modified content\n" for p in modified})
    for p in deleted:
        os.remove(os.path.join(repo.path, *p.split("/")))
    repo.write_files(new_files)

    changed_paths = sorted(modified) + sorted(deleted) + sorted(new_files)

    expected: dict[str, tuple[str, str]] = {}
    for p in modified:
        expected[to_posix(p)] = ("file", "keep")
    for p in deleted:
        expected[to_posix(p)] = ("file", "delete")
    for p in new_files:
        expected[to_posix(p)] = ("file", "add")
    for d in new_subdirs:
        expected[to_posix(d)] = ("directory", "add")
    return changed_paths, expected


@pytest.mark.smoke
def test_stage_scan_mixed_whole_tree(new_lore_repo):
    """Commit the tree, apply a modify/add/delete mix across it, then `--scan`
    the whole tree. Exactly the changed files (modify=keep, add, delete) and the
    newly added directories must be staged, once each."""
    repo: Lore = new_lore_repo()
    files, tops, subs = _build_tree(repo)
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    _changed, expected = _apply_mixed_changes(repo, files)

    repo.stage(scan=True, offline=True)

    _assert_staged_exactly(repo, expected)


@pytest.mark.smoke
def test_stage_noscan_mixed_dirty_overlapping(new_lore_repo):
    """Commit the tree, apply the same modify/add/delete mix, dirty-mark exactly
    the changed paths, and stage an overlapping arg set covering the first half
    of the top dirs without `--scan`. Exactly the changes under the covered paths
    must be staged, once each, with the same result the `--scan` variant produces
    for that region."""
    repo: Lore = new_lore_repo()
    files, tops, subs = _build_tree(repo)
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    changed, whole_expected = _apply_mixed_changes(repo, files)
    repo.dirty(changed, offline=True, relative_paths=True)

    covered_tops = tops[: len(tops) // 2]
    overlap_subs = [s for s in subs if _covered(s, covered_tops)][::13]
    overlap_files = [
        p for p in changed if _covered(to_posix(p), covered_tops)
    ][::17]
    repo.stage(
        covered_tops + overlap_subs + overlap_files,
        offline=True,
        relative_paths=True,
    )

    expected = {
        p: v for p, v in whole_expected.items() if _covered(p, covered_tops)
    }
    _assert_staged_exactly(repo, expected)
