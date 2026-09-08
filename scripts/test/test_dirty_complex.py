# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Comprehensive smoke tests for dirty-tracking interactions.

Covers file and directory dirty tracking, status and status --scan, stage and
stage --scan, and their interactions with reset, commit, branch switch, sync,
branch merge, cherry-pick and revert across added / modified / deleted files
and multi-level directories.

All tests use --json structured output parsed by parse_status_json and validate
the full set of reported file/directory states (path, action, flagDirty,
flagStaged) after and between operations.

Assertions encode the intended behavior and validate the full reported
file/directory state after each operation.
"""

import json
import logging
import os

import pytest
from lore_parsers import parse_status_json
from test_utils import to_posix

from lore import Lore

logger = logging.getLogger(__name__)


# ===========================================================================
# Shared helpers
# ===========================================================================


def get_status_files(repo: Lore, **kwargs) -> list[dict]:
    """Parsed repositoryStatusFile entries from `lore status --json` (offline)."""
    return parse_status_json(repo.status(json=True, offline=True, **kwargs))


def get_status_files_twice(repo: Lore, **kwargs) -> list[dict]:
    """Run status twice with identical args; assert the (path, dirty, staged)
    fingerprint is identical across runs (idempotency guard) and return the
    second run's entries."""
    first = get_status_files(repo, **kwargs)
    second = get_status_files(repo, **kwargs)

    def fp(entries: list[dict]) -> list[tuple]:
        return sorted(
            (to_posix(e.get("path", "")), e.get("flagDirty"), e.get("flagStaged"))
            for e in entries
        )

    assert fp(first) == fp(second), (
        f"status must be idempotent across repeated invocations with {kwargs}.\n"
        f"first:  {fp(first)}\nsecond: {fp(second)}"
    )
    return second


def find_status_entry(entries: list[dict], path: str) -> dict | None:
    """Return the entry whose path matches (posix-normalized), or None."""
    target = to_posix(path)
    for entry in entries:
        if to_posix(entry.get("path", "")) == target:
            return entry
    return None


def has_staged_anchor(repo: Lore) -> bool:
    """True iff the repo currently has a non-zero staged revision anchor."""
    zero = "0" * 64
    for line in repo.status(json=True, offline=True).splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        staged = event.get("data", {}).get("revisionStaged", "")
        if staged and staged != zero:
            return True
    return False


def summarize(entries: list[dict]) -> list[tuple]:
    """Compact (path, type, action, dirty, staged) view for assertion messages."""
    return [
        (
            to_posix(e.get("path", "")),
            e.get("type"),
            e.get("action"),
            e.get("flagDirty"),
            e.get("flagStaged"),
        )
        for e in entries
    ]


def assert_entry(
    entries: list[dict],
    path: str,
    *,
    action: str | None = None,
    dirty: bool | None = None,
    staged: bool | None = None,
    node_type: str | None = None,
    from_path: str | None = None,
    msg: str = "",
) -> dict:
    """Assert a status entry exists at path with the given fields. Only non-None
    expectations are checked. Returns the entry."""
    entry = find_status_entry(entries, path)
    assert entry is not None, (
        f"{path} should appear in status. {msg}\nentries={summarize(entries)}"
    )
    if action is not None:
        assert entry.get("action") == action, (
            f"{path} action should be {action!r}, got {entry.get('action')!r}. {msg}"
        )
    if dirty is not None:
        assert entry.get("flagDirty") is dirty, (
            f"{path} flagDirty should be {dirty}, got {entry.get('flagDirty')}. {msg}"
        )
    if staged is not None:
        assert entry.get("flagStaged") is staged, (
            f"{path} flagStaged should be {staged}, got {entry.get('flagStaged')}. {msg}"
        )
    if node_type is not None:
        assert entry.get("type") == node_type, (
            f"{path} type should be {node_type!r}, got {entry.get('type')!r}. {msg}"
        )
    if from_path is not None:
        assert to_posix(entry.get("fromPath", "")) == to_posix(from_path), (
            f"{path} fromPath should be {from_path!r}, got {entry.get('fromPath')!r}. {msg}"
        )
    return entry


def assert_absent(entries: list[dict], path: str, msg: str = "") -> None:
    """Assert no status entry exists at path."""
    entry = find_status_entry(entries, path)
    assert entry is None, f"{path} should not appear in status. {msg} got={entry}"


def file_paths(entries: list[dict]) -> set[str]:
    """Set of posix paths of file-type entries (directory nodes excluded)."""
    return {to_posix(e["path"]) for e in entries if e.get("type") == "file"}


def assert_file_set(entries: list[dict], expected, msg: str = "") -> None:
    """Assert the set of file-type paths exactly equals expected. Directory
    nodes are ignored so their ordering/inclusion does not make the set
    comparison brittle."""
    got = file_paths(entries)
    want = {to_posix(p) for p in expected}
    assert got == want, (
        f"file set mismatch. {msg}\nexpected={sorted(want)}\ngot={sorted(got)}\n"
        f"full={summarize(entries)}"
    )


def commit_base(repo: Lore, files: dict[str, str], message: str = "base") -> None:
    """Write files (path -> content), stage --scan, and commit a base revision,
    leaving a clean working tree."""
    repo.write_files(files)
    repo.stage(scan=True, offline=True)
    repo.commit(message, offline=True)


# ===========================================================================
# status vs --scan across file change types (persistence + idempotency)
# ===========================================================================


@pytest.mark.smoke
def test_filescan_modify_detected_and_persists(new_lore_repo):
    """A content modification of a committed file, detected only by
    `status --scan` (no dirty mark), is reported as action=keep/flagDirty
    and PERSISTS into a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n", "other.txt": "untouched\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned, ["file.txt"], msg="scan should detect only the modified file"
    )
    assert_entry(
        scanned, "file.txt", action="keep", dirty=True, staged=False, node_type="file"
    )

    persisted = get_status_files(repo)
    assert_file_set(
        persisted,
        ["file.txt"],
        msg="scanned modification must persist to no-scan status",
    )
    assert_entry(persisted, "file.txt", action="keep", dirty=True, staged=False)


@pytest.mark.smoke
def test_filescan_add_detected_and_persists(new_lore_repo):
    """A new untracked file detected by `status --scan` is reported as
    action=add/flagDirty and persists into a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("new.txt", "w+") as f:
        f.write("brand new content\n")

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["new.txt"], msg="scan should detect the new file")
    assert_entry(
        scanned, "new.txt", action="add", dirty=True, staged=False, node_type="file"
    )

    persisted = get_status_files(repo)
    assert_file_set(
        persisted, ["new.txt"], msg="scanned add must persist to no-scan status"
    )
    assert_entry(persisted, "new.txt", action="add", dirty=True, staged=False)


@pytest.mark.smoke
def test_filescan_empty_new_file_detected(new_lore_repo):
    """A zero-byte new file (hashes to the zero address) detected by
    `status --scan` is reported as action=add/flagDirty idempotently across
    repeated scans and persists into a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("empty.txt", "w+"):
        pass

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["empty.txt"], msg="scan should detect the empty new file")
    assert_entry(
        scanned, "empty.txt", action="add", dirty=True, staged=False, node_type="file"
    )

    persisted = get_status_files(repo)
    assert_file_set(
        persisted, ["empty.txt"], msg="empty new file must persist to no-scan status"
    )
    assert_entry(persisted, "empty.txt", action="add", dirty=True, staged=False)


@pytest.mark.smoke
def test_filescan_delete_detected_dirty_and_persists(new_lore_repo):
    """A delete discovered by `status --scan` (the committed file removed from
    disk, not dirty-marked) is reported as action=delete with flagDirty=True,
    like a scanned modify or add, and persists into a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "will be deleted\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["victim.txt"], msg="scan should detect the deletion")
    assert_entry(
        scanned,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="a scan-detected delete should be flagged dirty like modify/add",
    )

    persisted = get_status_files(repo)
    assert_file_set(
        persisted,
        ["victim.txt"],
        msg="a scan-detected delete must persist to no-scan status",
    )
    assert_entry(
        persisted,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        msg="a scan-detected delete must persist as a dirty delete",
    )


@pytest.mark.smoke
def test_filescan_no_scan_ignores_unmarked_edits(new_lore_repo):
    """`status` WITHOUT --scan performs no filesystem walk: an on-disk
    modification that is neither dirty-marked nor staged is invisible."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")

    entries = get_status_files(repo)
    assert_file_set(
        entries, [], msg="no-scan status must not report an un-dirtied on-disk edit"
    )


@pytest.mark.smoke
def test_filescan_nested_modify_no_ancestors(new_lore_repo):
    """A scan-detected modification of a deeply nested leaf is reported as
    action=keep/flagDirty for the leaf only; unchanged ANCESTOR directories
    are NOT reported."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/deep.txt": "deep original\n", "top.txt": "top\n"})

    with repo.open_file("a/b/c/deep.txt", "w+") as f:
        f.write("deep modified longer\n")

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned, ["a/b/c/deep.txt"], msg="only the modified leaf is a changed file"
    )
    assert_entry(
        scanned,
        "a/b/c/deep.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_absent(scanned, "a", msg="unchanged ancestor dir must not be reported")
    assert_absent(scanned, "a/b", msg="unchanged ancestor dir must not be reported")
    assert_absent(scanned, "a/b/c", msg="unchanged ancestor dir must not be reported")

    persisted = get_status_files(repo)
    assert_file_set(persisted, ["a/b/c/deep.txt"])
    assert_absent(persisted, "a")
    assert_absent(persisted, "a/b")
    assert_absent(persisted, "a/b/c")


@pytest.mark.smoke
def test_filescan_scan_clears_stale_dirty_on_revert(new_lore_repo):
    """`status --scan` clears a stale dirty flag: a file marked dirty then
    rewritten on disk to its exact committed content disappears from status
    after a scan, and stays absent in a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(before, "file.txt", dirty=True, msg="should be dirty before revert")

    with repo.open_file("file.txt", "w+") as f:
        f.write("original content\n")

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned, [], msg="reverted file must clear its stale dirty flag on scan"
    )

    persisted = get_status_files(repo)
    assert_file_set(
        persisted, [], msg="cleared dirty must stay cleared in no-scan status"
    )


@pytest.mark.smoke
def test_filescan_scan_clears_one_keeps_other(new_lore_repo):
    """`status --scan` clears the stale dirty flag of a reverted file while
    keeping the dirty flag of a still-modified file."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"reverted.txt": "reverted original\n", "still.txt": "still original\n"}
    )

    with repo.open_file("reverted.txt", "w+") as f:
        f.write("reverted modified longer\n")
    with repo.open_file("still.txt", "w+") as f:
        f.write("still modified longer\n")
    repo.dirty(["reverted.txt", "still.txt"], offline=True)

    with repo.open_file("reverted.txt", "w+") as f:
        f.write("reverted original\n")

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned, ["still.txt"], msg="only the still-modified file should remain"
    )
    assert_absent(scanned, "reverted.txt", msg="reverted file's dirty must clear")
    assert_entry(scanned, "still.txt", action="keep", dirty=True, staged=False)

    persisted = get_status_files(repo)
    assert_file_set(persisted, ["still.txt"])
    assert_entry(persisted, "still.txt", action="keep", dirty=True, staged=False)


# ===========================================================================
# Multi-level directories under status / --scan / dirty
# ===========================================================================


@pytest.mark.smoke
def test_dirs_add_tree_scan(new_lore_repo):
    """--scan of a brand-new 3-level directory subtree reports the leaf as
    action=add/flagDirty/file and emits each new ancestor directory node as
    action=add/type=directory (idempotent across re-scan)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("dir1/dir2/dir3")
    with repo.open_file("dir1/dir2/dir3/leaf.txt", "w+") as f:
        f.write("leaf content\n")

    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(
        scanned,
        "dir1/dir2/dir3/leaf.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_file_set(scanned, ["dir1/dir2/dir3/leaf.txt"])
    assert_entry(scanned, "dir1", action="add", node_type="directory")
    assert_entry(scanned, "dir1/dir2", action="add", node_type="directory")
    assert_entry(scanned, "dir1/dir2/dir3", action="add", node_type="directory")


@pytest.mark.smoke
def test_dirs_delete_tree_scan(new_lore_repo):
    """Removing a committed multi-level directory from disk and scanning
    reports the directory nodes (type=directory, action=delete) and their
    child files (action=delete); an unrelated committed file is untouched.

    This checks the reported node structure; flagDirty of a scan-detected
    delete is covered by the filescan section.
    """
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "d/x.txt": "x content\n",
            "d/sub/y.txt": "y content\n",
            "keep.txt": "keep\n",
        },
    )

    repo.rmtree("d")

    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(scanned, "d", action="delete", node_type="directory")
    assert_entry(scanned, "d/sub", action="delete", node_type="directory")
    assert_entry(scanned, "d/x.txt", action="delete", node_type="file")
    assert_entry(scanned, "d/sub/y.txt", action="delete", node_type="file")
    assert_file_set(scanned, ["d/x.txt", "d/sub/y.txt"])
    assert_absent(scanned, "keep.txt", msg="unrelated committed file is untouched")


@pytest.mark.smoke
def test_dirs_dirty_parent_collects_mixed_children(new_lore_repo):
    """`file dirty <dir>` collects every changed descendant: a modified file
    (action=keep/dirty), a deleted file (action=delete/dirty), and a newly
    added file (action=add/dirty).

    The explicit `dirty` command force-marks every tracked file still on disk
    as DirtyModify without comparing content, so the unchanged sibling
    src/sub/c.txt is also reported as keep/dirty.
    """
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "src/a.txt": "a original\n",
            "src/sub/b.txt": "b original\n",
            "src/sub/c.txt": "c original\n",
        },
    )

    with repo.open_file("src/a.txt", "w+") as f:
        f.write("a modified longer\n")
    repo.remove_file("src/sub/b.txt")
    with repo.open_file("src/sub/d.txt", "w+") as f:
        f.write("d new content\n")

    repo.dirty("src", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["src/a.txt", "src/sub/b.txt", "src/sub/c.txt", "src/sub/d.txt"]
    )
    assert_entry(
        entries, "src/a.txt", action="keep", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        entries,
        "src/sub/b.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(
        entries,
        "src/sub/d.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(
        entries,
        "src/sub/c.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty <dir> force-marks unchanged tracked children too",
    )


@pytest.mark.smoke
def test_dirs_dirty_changes_two_levels_below_marked_parent(new_lore_repo):
    """Marking the TOP directory dirty surfaces changes that live two and
    three levels below it: a deep modify (keep/dirty) and a deep add
    (add/dirty) are both reported against the single top-level dirty mark.

    `dirty <dir>` recurses without content comparison, so the shallow
    unchanged sibling top/keep.txt is also force-marked keep/dirty.
    """
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "top/keep.txt": "top keep\n",
            "top/mid/deep.txt": "deep original\n",
        },
    )

    with repo.open_file("top/mid/deep.txt", "w+") as f:
        f.write("deep modified longer\n")
    repo.make_dirs("top/mid/inner")
    with repo.open_file("top/mid/inner/added.txt", "w+") as f:
        f.write("added three levels deep\n")

    repo.dirty("top", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["top/keep.txt", "top/mid/deep.txt", "top/mid/inner/added.txt"]
    )
    assert_entry(
        entries,
        "top/mid/deep.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(
        entries,
        "top/mid/inner/added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(
        entries,
        "top/keep.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty <dir> force-marks the unchanged shallow sibling too",
    )


@pytest.mark.smoke
def test_dirs_delete_one_keep_sibling(new_lore_repo):
    """Dirty-deleting one file of a two-file directory reports only the
    deleted child (action=delete/dirty); the surviving sibling stays clean.
    A no-scan dirty of a single leaf path does not emit the parent directory
    node, so only the deleted file is reported."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "pkg/gone.txt": "gone content\n",
            "pkg/stay.txt": "stay content\n",
        },
    )

    repo.remove_file("pkg/gone.txt")
    repo.dirty("pkg/gone.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["pkg/gone.txt"])
    assert_entry(
        entries,
        "pkg/gone.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_absent(entries, "pkg/stay.txt", msg="surviving sibling stays clean")


@pytest.mark.smoke
def test_dirs_emptied_dir_retained_after_last_file_delete_commit(new_lore_repo):
    """Committing the dirty-delete of the only file in a nested directory
    removes the file from the committed tree and leaves a clean status. The
    now-empty directory node is RETAINED in the committed tree (as an empty
    node, child 0), consistent with the scan-driven/staged delete path.
    """
    repo: Lore = new_lore_repo()
    commit_base(repo, {"n/only.txt": "only content\n", "root.txt": "root\n"})

    repo.remove_file("n/only.txt")
    repo.dirty("n/only.txt", offline=True)
    repo.stage(offline=True)
    repo.commit("drop only.txt", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status should be clean after commit, got {summarize(entries)}"
    )
    scanned = get_status_files_twice(repo, scan=True)
    assert scanned == [], f"--scan status should be clean, got {summarize(scanned)}"

    dump = repo.repository_dump()
    assert "only.txt" not in dump, (
        f"deleted file should be absent from committed tree:\n{dump}"
    )
    assert "n/" in dump, (
        f"emptied directory 'n' should be retained in committed tree:\n{dump}"
    )


@pytest.mark.smoke
def test_dirs_add_tree_stage_commit_dump(new_lore_repo):
    """Adding a new directory tree, staging it via --scan, and committing
    lands the directory and its leaf in the committed tree and leaves a clean
    status (both no-scan and --scan)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/leaf.txt", "w+") as f:
        f.write("committed leaf\n")

    repo.stage(scan=True, offline=True)
    repo.commit("add a/b/c/leaf.txt", offline=True)

    dump = repo.repository_dump()
    assert "a/b/c/leaf.txt" in dump, f"leaf should appear in committed tree:\n{dump}"
    assert "a/b/c/" in dump, (
        f"added directory tree should appear in committed tree:\n{dump}"
    )

    entries = get_status_files(repo)
    assert entries == [], (
        f"status should be clean after commit, got {summarize(entries)}"
    )
    scanned = get_status_files_twice(repo, scan=True)
    assert scanned == [], f"--scan status should be clean, got {summarize(scanned)}"


@pytest.mark.smoke
def test_dirs_modify_under_existing_dir_keeps_dir_unreported(new_lore_repo):
    """Modifying a file inside an existing committed directory reports the
    file (action=keep/dirty) but NOT its unchanged ancestor directory node —
    a modification does not change the directory's identity for status."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "lib/mod.txt": "mod original\n",
            "lib/other.txt": "other stays\n",
        },
    )

    with repo.open_file("lib/mod.txt", "w+") as f:
        f.write("mod modified longer\n")

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["lib/mod.txt"])
    assert_entry(
        scanned,
        "lib/mod.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_absent(
        scanned, "lib", msg="unchanged ancestor directory must not be reported"
    )
    assert_absent(scanned, "lib/other.txt", msg="untouched sibling stays clean")

    persisted = get_status_files(repo)
    assert_entry(
        persisted,
        "lib/mod.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="scanned modification persists to no-scan status",
    )
    assert_absent(
        persisted, "lib", msg="ancestor dir still unreported in no-scan status"
    )


# ===========================================================================
# Section: file dirty across change types incl. move/copy and nesting
# ===========================================================================


@pytest.mark.smoke
def test_dirtyapi_modify_marks_dirty(new_lore_repo):
    """An explicit dirty mark on a modified committed file reports it as
    action=keep with flagDirty=True and flagStaged=False (no pure 'modify'
    action exists; a content edit keeps the node)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["file.txt"])
    assert_entry(
        entries, "file.txt", action="keep", dirty=True, staged=False, node_type="file"
    )


@pytest.mark.smoke
def test_dirtyapi_add_marks_dirty(new_lore_repo):
    """An explicit dirty mark on a brand-new untracked file reports it as
    action=add with flagDirty=True and flagStaged=False."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("new.txt", "w+") as f:
        f.write("new content\n")
    repo.dirty("new.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["new.txt"])
    assert_entry(
        entries, "new.txt", action="add", dirty=True, staged=False, node_type="file"
    )


@pytest.mark.smoke
def test_dirtyapi_delete_marks_dirty(new_lore_repo):
    """An explicitly dirty-marked delete of a committed file reports
    action=delete with flagDirty=True and flagStaged=False."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "will be deleted\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["victim.txt"])
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_absent(entries, "keep.txt", msg="untouched committed file stays clean")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_dirtyapi_copy_reports_copy_action(new_lore_repo):
    """A `file dirty copy` surfaces the destination as action=copy with
    fromPath pointing at the source; the source itself stays clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"orig.txt": "source content\n"})

    with repo.open_file("copy.txt", "w+") as f:
        f.write("source content\n")
    repo.dirty_copy("orig.txt", "copy.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["copy.txt"])
    assert_entry(
        entries,
        "copy.txt",
        action="copy",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="orig.txt",
        msg="a dirty copy must report action=copy with fromPath=source",
    )
    assert_absent(entries, "orig.txt", msg="copy source is unchanged")


@pytest.mark.smoke
def test_dirtyapi_move_action_no_scan(new_lore_repo):
    """A `file dirty move` (rename on disk + dirty_move) reports the
    destination as action=move with fromPath=source in no-scan status; the
    source path is absent."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"old.txt": "movable content\n"})

    os.rename(repo._fix_path("old.txt"), repo._fix_path("new.txt"))
    repo.dirty_move("old.txt", "new.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["new.txt"])
    assert_entry(
        entries,
        "new.txt",
        action="move",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="old.txt",
        msg="no-scan should report the dirty move with provenance",
    )
    assert_absent(entries, "old.txt", msg="move source must not appear")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_dirtyapi_move_survives_scan(new_lore_repo):
    """A `file dirty move` remains action=move/fromPath=source after a
    --scan, since the rename is still present on disk."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"old.txt": "movable content\n"})

    os.rename(repo._fix_path("old.txt"), repo._fix_path("new.txt"))
    repo.dirty_move("old.txt", "new.txt", offline=True)

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["new.txt"])
    assert_entry(
        scanned,
        "new.txt",
        action="move",
        dirty=True,
        node_type="file",
        from_path="old.txt",
        msg="--scan must preserve the dirty-move provenance",
    )
    assert_absent(scanned, "old.txt", msg="move source must not reappear after scan")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_dirtyapi_move_into_new_directory(new_lore_repo):
    """Renaming a committed file into a brand-new directory and dirty-moving it
    reports the destination as action=move/fromPath=source and surfaces the new
    destination directory node (type=directory). A dirty move creates any
    missing parent directories — exactly like a plain dirty-add — so the
    destination parent need NOT already exist as a tracked node."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"src.txt": "movable content\n"})

    repo.make_dirs("dest2")
    os.rename(repo._fix_path("src.txt"), repo._fix_path("dest2/src.txt"))
    repo.dirty_move("src.txt", "dest2/src.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["dest2/src.txt"])
    assert_entry(
        entries,
        "dest2/src.txt",
        action="move",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="src.txt",
        msg="move into a new dir keeps move provenance",
    )
    assert_entry(
        entries,
        "dest2",
        action="add",
        node_type="directory",
        msg="new dest dir node present",
    )
    assert_absent(entries, "src.txt", msg="move source must not appear")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_dirtyapi_copy_into_nested_new_directory(new_lore_repo):
    """A dirty copy of a committed file into a nested brand-new directory
    reports the destination as action=copy/fromPath=source and surfaces every
    new directory node (type=directory). A dirty copy creates any missing parent
    directories, so the destination parent need NOT already be a tracked node."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"orig.txt": "source content\n"})

    repo.make_dirs("newdir/sub")
    with repo.open_file("newdir/sub/copy.txt", "w+") as f:
        f.write("source content\n")
    repo.dirty_copy("orig.txt", "newdir/sub/copy.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["newdir/sub/copy.txt"])
    assert_entry(
        entries,
        "newdir/sub/copy.txt",
        action="copy",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="orig.txt",
        msg="a nested dirty copy must report action=copy with fromPath=source",
    )
    assert_entry(
        entries,
        "newdir",
        action="add",
        node_type="directory",
        msg="new parent dir node present",
    )
    assert_entry(
        entries,
        "newdir/sub",
        action="add",
        node_type="directory",
        msg="new sub dir node present",
    )
    assert_absent(entries, "orig.txt", msg="copy source is unchanged")


@pytest.mark.smoke
def test_dirtyapi_dirty_directory_recurses(new_lore_repo):
    """Marking a committed directory dirty recurses to every child: each
    modified leaf is reported as action=keep with flagDirty=True."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "src/a.txt": "aaa original\n",
            "src/b.txt": "bbb original\n",
            "src/c.txt": "ccc original\n",
        },
    )

    repo.write_files(
        {
            "src/a.txt": "aaa modified longer\n",
            "src/b.txt": "bbb modified longer\n",
            "src/c.txt": "ccc modified longer\n",
        }
    )
    repo.dirty("src", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["src/a.txt", "src/b.txt", "src/c.txt"])
    for child in ("src/a.txt", "src/b.txt", "src/c.txt"):
        assert_entry(
            entries, child, action="keep", dirty=True, staged=False, node_type="file"
        )


@pytest.mark.smoke
def test_dirtyapi_nonexistent_path_ignored(new_lore_repo):
    """A dirty mark on a path that exists neither on disk nor in the revision
    has no effect; status stays empty."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.dirty("ghost/none.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, [])
    assert_absent(entries, "ghost/none.txt", msg="nonexistent path must be ignored")


@pytest.mark.smoke
def test_dirtyapi_ignored_path_skipped(new_lore_repo):
    """A dirty mark on a file under an ignored directory is skipped; status
    reports nothing for it."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("ig/\n")

    repo.make_dirs("ig")
    with repo.open_file("ig/f.txt", "w+") as f:
        f.write("should be ignored\n")
    repo.dirty("ig/f.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, [])
    assert_absent(entries, "ig/f.txt", msg="file under ignored path must be skipped")


# ---------------------------------------------------------------------------
# stage default vs --scan vs explicit path across change types & dirs
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_stage_default_only_stages_dirty_marked(new_lore_repo):
    """Default `stage` (no --scan) does NO filesystem walk: it stages only
    nodes already marked dirty, leaving an unmarked on-disk edit untouched."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"dirty.txt": "original\n", "unmarked.txt": "original\n"})

    with repo.open_file("dirty.txt", "w+") as f:
        f.write("modified dirty\n")
    with repo.open_file("unmarked.txt", "w+") as f:
        f.write("modified unmarked\n")

    repo.dirty("dirty.txt", offline=True)
    repo.stage(scan=False, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["dirty.txt"], msg="only the dirty-marked file is tracked")
    assert_entry(
        entries,
        "dirty.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="default stage of a dirty modify keeps dirty and sets staged",
    )
    assert_absent(entries, "unmarked.txt", msg="unmarked edit is invisible to no-scan")


@pytest.mark.smoke
def test_stage_scan_stages_unmarked_modify(new_lore_repo):
    """`stage --scan` walks the filesystem and stages an unmarked content
    modification as action=keep, flagStaged=true, flagDirty=TRUE — the scan
    detects the change and marks it dirty as well as staging it."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")

    repo.stage(scan=True, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["file.txt"])
    assert_entry(
        entries,
        "file.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="stage --scan marks the detected modification dirty and stages it",
    )


@pytest.mark.smoke
def test_stage_scan_stages_unmarked_add(new_lore_repo):
    """`stage --scan` discovers and stages a brand-new untracked file as
    action=add, flagStaged=true, flagDirty=TRUE — the scan detects the new file
    and marks it dirty as well as staging it."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("new.txt", "w+") as f:
        f.write("untracked content\n")

    repo.stage(scan=True, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["new.txt"])
    assert_entry(
        entries,
        "new.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="stage --scan marks the detected add dirty and stages it",
    )


@pytest.mark.smoke
def test_stage_scan_stages_unmarked_delete(new_lore_repo):
    """`stage --scan` discovers a committed file removed from disk and stages
    the deletion as action=delete, flagStaged=true, flagDirty=true (the scan
    detects the removal and marks it dirty as well as staging it). The other
    committed file is untouched."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "will be deleted\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")

    repo.stage(scan=True, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["victim.txt"], msg="only the deleted file is tracked")
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="stage --scan marks the detected delete dirty and stages it",
    )
    assert_absent(entries, "keep.txt", msg="undeleted file is not tracked")


@pytest.mark.smoke
def test_stage_explicit_file_without_dirty(new_lore_repo):
    """An explicit `stage <file>` (no --scan, no dirty mark) checks the
    filesystem for that path and stages the modification."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")

    repo.stage("file.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["file.txt"])
    assert_entry(
        entries,
        "file.txt",
        action="keep",
        staged=True,
        node_type="file",
        msg="explicit path stage works without a dirty mark",
    )


@pytest.mark.smoke
def test_stage_explicit_dir_default_only_dirty_leaves(new_lore_repo):
    """An explicit directory `stage <dir>` WITHOUT --scan stages only the
    dirty-marked leaves under that dir, not every modified leaf."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"src/a.txt": "a original\n", "src/b.txt": "b original\n"})

    with repo.open_file("src/a.txt", "w+") as f:
        f.write("a modified\n")
    with repo.open_file("src/b.txt", "w+") as f:
        f.write("b modified\n")
    repo.dirty("src/a.txt", offline=True)

    repo.stage("src", scan=False, offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["src/a.txt"], msg="only the dirty leaf under src is staged"
    )
    assert_entry(
        entries, "src/a.txt", action="keep", dirty=True, staged=True, node_type="file"
    )
    assert_absent(entries, "src/b.txt", msg="unmarked leaf under src is not staged")


@pytest.mark.smoke
def test_stage_dir_scan_full_walk(new_lore_repo):
    """`stage <dir> --scan` walks the whole subtree and stages every modified
    leaf (none dirty-marked beforehand), each flagStaged=true/flagDirty=true —
    the scan detects each modification and marks it dirty as well as staging."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"src/a.txt": "a original\n", "src/b.txt": "b original\n"})

    with repo.open_file("src/a.txt", "w+") as f:
        f.write("a modified\n")
    with repo.open_file("src/b.txt", "w+") as f:
        f.write("b modified\n")

    repo.stage("src", scan=True, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["src/a.txt", "src/b.txt"])
    assert_entry(entries, "src/a.txt", action="keep", dirty=True, staged=True)
    assert_entry(entries, "src/b.txt", action="keep", dirty=True, staged=True)


@pytest.mark.smoke
def test_stage_path_scoped_scan(new_lore_repo):
    """`stage --scan` scoped to a path arg stages only files under that path;
    modifications outside the scope are neither staged nor (since no dirty mark
    persisted) reported by a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"subA/a.txt": "a original\n", "subB/b.txt": "b original\n"})

    with repo.open_file("subA/a.txt", "w+") as f:
        f.write("a modified\n")
    with repo.open_file("subB/b.txt", "w+") as f:
        f.write("b modified\n")

    repo.stage(paths=["subA"], scan=True, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["subA/a.txt"], msg="only subA was scanned/staged")
    assert_entry(entries, "subA/a.txt", action="keep", dirty=True, staged=True)
    assert_absent(entries, "subB/b.txt", msg="subB outside scan scope is untracked")


@pytest.mark.smoke
def test_stage_add_and_delete_default_from_dirty(new_lore_repo):
    """Default `stage` (no --scan) stages dirty-marked add and delete nodes,
    reporting action=add and action=delete respectively, each dirty+staged."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "will be deleted\n", "keep.txt": "stays\n"})

    with repo.open_file("added.txt", "w+") as f:
        f.write("brand new\n")
    repo.remove_file("victim.txt")
    repo.dirty(["added.txt", "victim.txt"], offline=True)

    repo.stage(scan=False, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["added.txt", "victim.txt"])
    assert_entry(
        entries, "added.txt", action="add", dirty=True, staged=True, node_type="file"
    )
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
    )
    assert_absent(entries, "keep.txt", msg="untouched file is not tracked")


# ===========================================================================
# reset across change types & directories
# ===========================================================================


@pytest.mark.smoke
def test_reset_modify_clears_and_restores(new_lore_repo):
    """Resetting a dirty modification restores committed content and clears the
    dirty flag; status (no-scan and --scan) is clean afterwards."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(before, "file.txt", action="keep", dirty=True, staged=False)

    repo.reset("file.txt", offline=True)

    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "original content\n", "content must be restored to committed"

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, [], msg="no-scan status must be clean after reset")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, [], msg="--scan status must be clean after reset")
    assert not has_staged_anchor(repo), "anchor must be gone once the tree is clean"


@pytest.mark.smoke
def test_reset_add_untracks(new_lore_repo):
    """Plain `reset` of a dirty-add keeps the file on disk (only --purge removes
    untracked files) and clears its dirty tracking, leaving it as a plain
    untracked file; a later --scan rediscovers it as an add. This is symmetric
    with how reset clears a modify or a delete: it restores the working tree to
    the committed revision's view of the path, and the committed revision has no
    node for an added file, so the file becomes untracked rather than
    tracked-and-dirty."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("new.txt", "w+") as f:
        f.write("brand new\n")
    repo.dirty("new.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(before, "new.txt", action="add", dirty=True, staged=False)

    repo.reset("new.txt", offline=True)

    # Plain reset must KEEP the untracked added file on disk; only `reset --purge`
    # removes untracked files (see reset --help and docs/guides/links.md).
    assert os.path.exists(repo._fix_path("new.txt")), (
        "plain reset must keep the untracked added file on disk; only --purge removes it"
    )
    with repo.open_file("new.txt", "r") as f:
        assert f.read() == "brand new\n", "untracked content must be intact after reset"

    # Reset clears the dirty flag, so the now-untracked file is invisible to
    # no-scan status (symmetric with the modify/delete reset cases).
    no_scan = get_status_files(repo)
    assert_absent(
        no_scan,
        "new.txt",
        msg="reset must clear the dirty-add; no-scan status must be clean",
    )
    assert_file_set(no_scan, [], msg="no files should remain tracked after reset")

    # The file still exists untracked on disk, so --scan rediscovers it as an
    # add (this is --scan's job; it is not resurrecting a tracked node).
    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(
        scanned,
        "new.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="--scan rediscovers the surviving untracked file as an add",
    )


@pytest.mark.smoke
def test_reset_delete_restores(new_lore_repo):
    """Resetting a dirty-delete restores the file on disk from the committed
    revision and clears it from status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "precious content\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(before, "victim.txt", action="delete", dirty=True, staged=False)

    repo.reset("victim.txt", offline=True)

    assert os.path.exists(repo._fix_path("victim.txt")), "deleted file must be restored"
    with repo.open_file("victim.txt", "r") as f:
        assert f.read() == "precious content\n", "restored content must match committed"

    no_scan = get_status_files(repo)
    assert_absent(no_scan, "victim.txt", msg="restored delete must be clean")
    assert_file_set(no_scan, [], msg="no files should remain tracked after reset")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, [], msg="--scan must agree the tree is clean")


@pytest.mark.smoke
def test_reset_directory_mixed_children(new_lore_repo):
    """Resetting a directory restores its tracked dirtied children (a modify and
    a delete) to their committed content/existence and clears the directory's
    dirty tracking, leaving no-scan status clean. An added-but-uncommitted child
    is untracked relative to the committed state, so a plain (non --purge) reset
    leaves it on disk; only `reset --purge` deletes untracked files (see
    test_reset_purge_removes_untracked). A subsequent --scan therefore correctly
    rediscovers the surviving add child."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "src/mod.txt": "modify original\n",
            "src/del.txt": "delete original\n",
            "outside.txt": "untouched\n",
        },
    )

    with repo.open_file("src/mod.txt", "w+") as f:
        f.write("modify changed longer\n")
    repo.remove_file("src/del.txt")
    with repo.open_file("src/add.txt", "w+") as f:
        f.write("freshly added\n")
    repo.dirty(["src/mod.txt", "src/del.txt", "src/add.txt"], offline=True)

    before = get_status_files(repo)
    assert_file_set(
        before,
        ["src/mod.txt", "src/del.txt", "src/add.txt"],
        msg="all three children dirtied before reset",
    )
    assert_entry(before, "src/mod.txt", action="keep", dirty=True)
    assert_entry(before, "src/del.txt", action="delete", dirty=True)
    assert_entry(before, "src/add.txt", action="add", dirty=True)

    repo.reset("src", offline=True)

    # Tracked children are restored to their committed state.
    with repo.open_file("src/mod.txt", "r") as f:
        assert f.read() == "modify original\n", "modified child must be restored"
    assert os.path.exists(repo._fix_path("src/del.txt")), (
        "deleted child must be restored"
    )
    with repo.open_file("src/del.txt", "r") as f:
        assert f.read() == "delete original\n", "restored delete content must match"

    # A plain reset is not a purge: the added-but-uncommitted child is untracked
    # relative to the committed state and is left on disk (only --purge removes
    # untracked files).
    assert os.path.exists(repo._fix_path("src/add.txt")), (
        "plain (non --purge) reset must keep the untracked added child on disk"
    )

    # The directory's dirty tracking is cleared: no-scan status is clean and the
    # staged anchor is released now that nothing dirty/staged remains.
    no_scan = get_status_files(repo)
    assert_file_set(no_scan, [], msg="no-scan status must be clean after dir reset")
    assert not has_staged_anchor(repo), "anchor must be released once tracking is clear"

    # The surviving add child is a genuine untracked file, so --scan rediscovers
    # it as an unstaged add (idempotently).
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned,
        ["src/add.txt"],
        msg="--scan must rediscover the surviving untracked add",
    )
    assert_entry(scanned, "src/add.txt", action="add", dirty=True, staged=False)


@pytest.mark.smoke
def test_reset_nested_path(new_lore_repo):
    """Resetting a file several directories deep restores it and clears the
    dirty flag from every intermediate parent (status clean, no stray dirty
    ancestor entries)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/d/deep.txt": "deep original\n", "a/sibling.txt": "sib\n"})

    with repo.open_file("a/b/c/d/deep.txt", "w+") as f:
        f.write("deep modified content longer\n")
    repo.dirty("a/b/c/d/deep.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(before, "a/b/c/d/deep.txt", action="keep", dirty=True, staged=False)

    repo.reset("a/b/c/d/deep.txt", offline=True)

    with repo.open_file("a/b/c/d/deep.txt", "r") as f:
        assert f.read() == "deep original\n", "nested file must be restored"

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, [], msg="reset must clear the leaf and all dirty parents")
    for ancestor in ("a", "a/b", "a/b/c", "a/b/c/d"):
        assert_absent(no_scan, ancestor, msg="intermediate parent must not stay dirty")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, [], msg="--scan agrees the subtree is clean")
    assert not has_staged_anchor(repo), (
        "anchor released after the only dirty leaf reset"
    )


@pytest.mark.smoke
def test_reset_purge_removes_untracked(new_lore_repo):
    """reset --purge removes an untracked file (never even dirtied) from disk."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"tracked.txt": "tracked\n"})

    with repo.open_file("junk.txt", "w+") as f:
        f.write("untracked junk\n")

    # The file was never dirtied, so it is invisible to no-scan status.
    pre = get_status_files(repo)
    assert_absent(pre, "junk.txt", msg="untracked file is invisible to no-scan status")

    repo.reset(".", purge=True, offline=True)

    assert not os.path.exists(repo._fix_path("junk.txt")), (
        "purge must remove the untracked file from disk"
    )
    assert os.path.exists(repo._fix_path("tracked.txt")), (
        "tracked file must survive purge"
    )
    with repo.open_file("tracked.txt", "r") as f:
        assert f.read() == "tracked\n", "tracked content must be intact after purge"

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, [], msg="status clean after purge")


@pytest.mark.smoke
def test_reset_to_revision(new_lore_repo):
    """reset --revision restores on-disk content to a chosen older revision."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1 content\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 content longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v2 content longer\n", "working tree starts at v2"

    repo.reset("file.txt", revision=rev_v1, offline=True)

    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v1 content\n", "reset --revision must restore v1 content"


@pytest.mark.smoke
def test_reset_refuses_on_staged(new_lore_repo):
    """Resetting a STAGED node refuses with an error rather than silently
    clearing the stage; the staged state survives the failed reset."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.stage("file.txt", offline=True)

    staged = get_status_files(repo)
    assert_entry(staged, "file.txt", staged=True)

    with pytest.raises(LoreException) as excinfo:
        repo.reset("file.txt", offline=True)
    assert "Failed to reset staged node" in str(excinfo.value), (
        f"refusal should name the staged-node guard, got: {excinfo.value}"
    )

    # The stage must survive the refused reset.
    after = get_status_files(repo)
    assert_entry(
        after,
        "file.txt",
        staged=True,
        msg="staged node must remain staged after a refused reset",
    )
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "modified content longer\n", (
            "refused reset must not touch on-disk content"
        )


# ===========================================================================
# commit with mixed staged/dirty + committed-tree (dump) + anchor lifecycle
# ===========================================================================


def _dump_node_addr(dump: str, name: str) -> str | None:
    """Return the addr field of the dump line for an exact node name, or None.
    Dump lines look like: '<name> id N parent .. addr <hash>-<id>'. Directory
    nodes carry no addr."""
    for raw in dump.splitlines():
        line = raw.strip()
        parts = line.split()
        if not parts or parts[0] != name:
            continue
        if "addr" in parts:
            return parts[parts.index("addr") + 1]
        return None
    return None


@pytest.mark.smoke
def test_commit_dirty_only_modify_survives(new_lore_repo):
    """Committing a staged modify clears it; a dirty-only modify on another
    file survives as action=keep/flagDirty, and the staged anchor persists."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"staged.txt": "staged original\n", "dirty.txt": "dirty original\n"}
    )

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged modified longer\n")
    with repo.open_file("dirty.txt", "w+") as f:
        f.write("dirty modified longer\n")
    repo.dirty(["staged.txt", "dirty.txt"], offline=True)
    repo.stage("staged.txt", offline=True)

    repo.commit("commit staged", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["dirty.txt"], msg="only the dirty-only file remains pending"
    )
    assert_absent(entries, "staged.txt", msg="staged modify is committed and clean")
    assert_entry(
        entries,
        "dirty.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-only modify survives commit as a kept dirty file",
    )
    assert has_staged_anchor(repo), (
        "anchor must persist while a dirty-only node remains"
    )


@pytest.mark.smoke
def test_commit_dirty_only_add_excluded_from_tree(new_lore_repo):
    """A staged add in a new dir lands in the sealed tree; a dirty-only add in
    a different new dir stays pending (add/dirty) and is absent from the dump."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("staged_dir")
    with repo.open_file("staged_dir/staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged_dir/staged.txt", offline=True)

    repo.make_dirs("dirty_dir")
    with repo.open_file("dirty_dir/dirty.txt", "w+") as f:
        f.write("dirty content\n")
    repo.dirty("dirty_dir/dirty.txt", offline=True)

    repo.commit("commit staged add", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["dirty_dir/dirty.txt"], msg="only the dirty-only add remains"
    )
    assert_absent(
        entries, "staged_dir/staged.txt", msg="staged add is committed and clean"
    )
    assert_entry(
        entries,
        "dirty_dir/dirty.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-only add stays pending after commit",
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "staged.txt" in dump, f"staged add should be in the sealed tree:\n{dump}"
    assert "dirty.txt" not in dump, (
        f"dirty-only add must not be in the sealed tree:\n{dump}"
    )
    assert "dirty_dir" not in dump, (
        f"dirty-only added dir must not be in the sealed tree:\n{dump}"
    )


@pytest.mark.smoke
def test_commit_dirty_only_delete_reverted_in_tree(new_lore_repo):
    """A dirty-only delete is reverted when the merkle tree is sealed (file
    stays in the dump) while remaining pending in status (delete/dirty); an
    unrelated staged change is what actually gets committed."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"victim.txt": "will be deleted\n", "other.txt": "other original\n"}
    )

    os.remove(repo._fix_path("victim.txt"))
    repo.dirty("victim.txt", offline=True)

    with repo.open_file("other.txt", "w+") as f:
        f.write("other modified longer\n")
    repo.stage("other.txt", offline=True)

    repo.commit("commit unrelated", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["victim.txt"], msg="only the dirty-only delete remains pending"
    )
    assert_absent(entries, "other.txt", msg="staged modify is committed and clean")
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-only delete stays pending after commit",
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "victim.txt" in dump, (
        f"dirty-only delete must be reverted in the sealed tree:\n{dump}"
    )


@pytest.mark.smoke
def test_commit_anchor_deleted_when_clean(new_lore_repo):
    """Modify + dirty + stage + commit leaves nothing pending, so the staged
    anchor is removed and a no-scan status is empty."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)
    repo.stage("file.txt", offline=True)
    repo.commit("commit all", offline=True)

    assert not has_staged_anchor(repo), (
        "anchor must be deleted when nothing remains pending"
    )
    entries = get_status_files(repo)
    assert entries == [], (
        f"no-scan status must be empty when clean, got {summarize(entries)}"
    )


@pytest.mark.smoke
def test_commit_anchor_preserved_when_dirty_remains(new_lore_repo):
    """When a dirty-only node survives a commit, the staged anchor is kept and
    the survivor is the only pending entry."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"staged.txt": "staged original\n", "dirty.txt": "dirty original\n"}
    )

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged modified longer\n")
    with repo.open_file("dirty.txt", "w+") as f:
        f.write("dirty modified longer\n")
    repo.dirty(["staged.txt", "dirty.txt"], offline=True)
    repo.stage("staged.txt", offline=True)
    repo.commit("commit staged", offline=True)

    assert has_staged_anchor(repo), (
        "anchor must persist while a dirty-only node remains"
    )
    entries = get_status_files(repo)
    assert_file_set(entries, ["dirty.txt"], msg="only the dirty-only survivor remains")
    assert_entry(entries, "dirty.txt", action="keep", dirty=True, staged=False)


@pytest.mark.smoke
def test_commit_emptied_dir_retained_in_tree(new_lore_repo):
    """Committing a staged delete of a directory's only file removes the file
    from the sealed tree. The now-empty directory node is RETAINED in the tree
    (as an empty node, child 0), not collapsed away."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"keep.txt": "keep\n", "sub/only.txt": "only file\n"})

    repo.status(reset=True, offline=True)
    before = repo.repository_dump()
    assert "sub/only.txt" in before and "sub/" in before, (
        f"base tree should contain the dir and its leaf:\n{before}"
    )

    os.remove(repo._fix_path("sub/only.txt"))
    repo.stage("sub/only.txt", scan=True, offline=True)

    staged = get_status_files(repo)
    assert_entry(
        staged,
        "sub/only.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="the file is staged for deletion before commit",
    )

    repo.commit("delete only file", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status must be clean after committing the delete, got {summarize(entries)}"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "sub/only.txt" not in dump, (
        f"deleted file must be gone from the sealed tree:\n{dump}"
    )
    assert "sub/" in dump, (
        f"emptied directory node should be retained in the sealed tree:\n{dump}"
    )
    assert "keep.txt" in dump, f"unrelated file must remain in the sealed tree:\n{dump}"


@pytest.mark.smoke
def test_commit_staged_add_dir_in_tree(new_lore_repo):
    """Staging a brand-new nested directory tree and committing puts every
    directory node and the leaf into the sealed tree and leaves status clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/leaf.txt", "w+") as f:
        f.write("leaf content\n")
    repo.stage("a/b/c/leaf.txt", scan=True, offline=True)

    staged = get_status_files(repo)
    assert_entry(
        staged,
        "a/b/c/leaf.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
    )
    assert_entry(
        staged, "a/b/c", action="add", dirty=True, staged=True, node_type="directory"
    )

    repo.commit("add nested tree", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status must be clean after committing the add, got {summarize(entries)}"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    for token in ("a/", "a/b/", "a/b/c/", "a/b/c/leaf.txt"):
        assert token in dump, f"committed nested tree should contain {token!r}:\n{dump}"


@pytest.mark.smoke
def test_commit_mixed_each_class_commit(new_lore_repo):
    """A staged modify, a staged add and a dirty-only modify committed together:
    the staged ones land clean, the dirty-only survives, and the sealed tree
    reflects only the staged changes (staged-modify content hash changes, the
    dirty-only file's hash is unchanged from base)."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "mod.txt": "mod original\n",
            "dirty.txt": "dirty original\n",
            "anchor.txt": "anchor\n",
        },
    )
    repo.status(reset=True, offline=True)
    base_dump = repo.repository_dump()
    base_mod_addr = _dump_node_addr(base_dump, "mod.txt")
    base_dirty_addr = _dump_node_addr(base_dump, "dirty.txt")
    assert base_mod_addr and base_dirty_addr, f"base addrs should parse:\n{base_dump}"

    with repo.open_file("mod.txt", "w+") as f:
        f.write("mod modified longer\n")
    with repo.open_file("added.txt", "w+") as f:
        f.write("freshly added\n")
    repo.stage(["mod.txt", "added.txt"], scan=True, offline=True)

    with repo.open_file("dirty.txt", "w+") as f:
        f.write("dirty modified longer\n")
    repo.dirty("dirty.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "mod.txt", action="keep", dirty=True, staged=True)
    assert_entry(pre, "added.txt", action="add", dirty=True, staged=True)
    assert_entry(pre, "dirty.txt", action="keep", dirty=True, staged=False)

    repo.commit("commit mixed", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["dirty.txt"], msg="only the dirty-only modify survives")
    assert_absent(entries, "mod.txt", msg="staged modify is committed and clean")
    assert_absent(entries, "added.txt", msg="staged add is committed and clean")
    assert_entry(entries, "dirty.txt", action="keep", dirty=True, staged=False)
    assert has_staged_anchor(repo), (
        "anchor persists while the dirty-only modify remains"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "added.txt" in dump, f"staged add should be in the sealed tree:\n{dump}"
    assert _dump_node_addr(dump, "mod.txt") != base_mod_addr, (
        f"staged modify's content hash should change in the sealed tree:\n{dump}"
    )
    assert _dump_node_addr(dump, "dirty.txt") == base_dirty_addr, (
        f"dirty-only modify must not reach the sealed tree (hash unchanged):\n{dump}"
    )


# ===========================================================================
# branch switch: dirty carry matrix + anchor rebase
# ===========================================================================


@pytest.mark.smoke
def test_switch_carries_dirty_modify_add_delete(new_lore_repo):
    """Switching between two same-revision branches carries the full dirty
    set: a modify (action=keep), an add (action=add) and a delete
    (action=delete) all remain flagDirty after the switch; a committed file
    untouched on disk stays clean and absent."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "mod.txt": "mod original\n",
            "del.txt": "del original\n",
            "stay.txt": "stay\n",
        },
    )

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("mod.txt", "w+") as f:
        f.write("mod locally edited\n")
    with repo.open_file("added.txt", "w+") as f:
        f.write("added content\n")
    repo.remove_file("del.txt")
    repo.dirty(["mod.txt", "added.txt", "del.txt"], offline=True)

    pre = get_status_files(repo)
    assert_file_set(
        pre, ["mod.txt", "added.txt", "del.txt"], msg="dirty set before switch"
    )

    repo.branch_switch("other", offline=True)

    entries = get_status_files(repo)
    assert_entry(
        entries, "mod.txt", action="keep", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        entries, "added.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        entries, "del.txt", action="delete", dirty=True, staged=False, node_type="file"
    )
    assert_absent(entries, "stay.txt", msg="untouched committed file stays clean")
    assert_file_set(
        entries,
        ["mod.txt", "added.txt", "del.txt"],
        msg="dirty set carried across same-revision switch",
    )

    with repo.open_file("mod.txt", "r") as f:
        assert f.read() == "mod locally edited\n"
    with repo.open_file("added.txt", "r") as f:
        assert f.read() == "added content\n"
    assert not os.path.exists(repo._fix_path("del.txt")), (
        "dirty-deleted file stays deleted after switch"
    )


@pytest.mark.smoke
def test_switch_only_dirty_paths_after_feature_commit(new_lore_repo):
    """Switching back to main rebases the staged anchor: the feature commit's
    modify/add/delete are reverted on disk, and only the separately-dirtied
    paths remain dirty with their local content / deletion preserved."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "app/main.py": "main entrypoint\n",
            "app/utils/helper.py": "helper original\n",
            "docs/readme.md": "readme original\n",
            "data/sample.txt": "sample data\n",
            "data/config.json": "{}\n",
        },
    )

    repo.branch_create("feature", offline=True)
    repo.write_files(
        {"app/main.py": "modified on feature\n", "app/new.py": "new feature code\n"}
    )
    repo.remove_file("docs/readme.md")
    repo.stage(scan=True, offline=True)
    repo.commit("feature commit", offline=True)

    repo.write_files(
        {
            "data/sample.txt": "dirty modified sample\n",
            "data/extra.txt": "dirty new file\n",
        }
    )
    repo.remove_file("app/utils/helper.py")
    repo.dirty(
        ["data/sample.txt", "data/extra.txt", "app/utils/helper.py"], offline=True
    )

    pre = get_status_files(repo)
    assert_file_set(
        pre,
        ["data/sample.txt", "data/extra.txt", "app/utils/helper.py"],
        msg="dirty set before switch",
    )

    repo.branch_switch("main", offline=True)

    entries = get_status_files(repo)
    assert_entry(entries, "data/sample.txt", action="keep", dirty=True, staged=False)
    assert_entry(entries, "data/extra.txt", action="add", dirty=True, staged=False)
    assert_entry(
        entries, "app/utils/helper.py", action="delete", dirty=True, staged=False
    )
    assert_file_set(
        entries,
        ["data/sample.txt", "data/extra.txt", "app/utils/helper.py"],
        msg="only the three dirtied paths remain dirty after switch",
    )

    with repo.open_file("app/main.py", "r") as f:
        assert f.read() == "main entrypoint\n", "feature modify reverted on disk"
    assert not os.path.exists(repo._fix_path("app/new.py")), (
        "feature add absent on main"
    )
    with repo.open_file("docs/readme.md", "r") as f:
        assert f.read() == "readme original\n", "feature delete restored on main"
    with repo.open_file("data/config.json", "r") as f:
        assert f.read() == "{}\n"

    with repo.open_file("data/sample.txt", "r") as f:
        assert f.read() == "dirty modified sample\n", "dirty modify keeps local content"
    with repo.open_file("data/extra.txt", "r") as f:
        assert f.read() == "dirty new file\n", "dirty add keeps local content"
    assert not os.path.exists(repo._fix_path("app/utils/helper.py")), (
        "dirty delete stays deleted"
    )


@pytest.mark.smoke
def test_switch_clears_anchor_when_no_dirty(new_lore_repo):
    """Switching back to main with no dirty work clears the staged anchor,
    leaves the file absent from status, and restores main's content."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "alice content here\n"})

    repo.branch_create("other", offline=True)
    with repo.open_file("file.txt", "w+") as f:
        f.write("bob content here\n")
    repo.stage(scan=True, offline=True)
    repo.commit("bob commit", offline=True)

    repo.branch_switch("main", offline=True)

    assert not has_staged_anchor(repo), (
        "anchor should be cleared after a no-dirty switch"
    )
    entries = get_status_files_twice(repo, scan=True)
    assert_absent(entries, "file.txt", msg="file.txt clean after switch")
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "alice content here\n"


@pytest.mark.smoke
def test_switch_anchor_rebase_parity_add(new_lore_repo):
    """A branch that committed an ADD, switched back to main, leaves a clean
    status (no false add) and main content; the added file is gone from disk."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.branch_create("other", offline=True)
    with repo.open_file("added.txt", "w+") as f:
        f.write("added on other\n")
    repo.stage(scan=True, offline=True)
    repo.commit("add on other", offline=True)

    repo.branch_switch("main", offline=True)

    assert not has_staged_anchor(repo), "no dirty work => anchor cleared"
    entries = get_status_files_twice(repo, scan=True)
    assert_absent(entries, "added.txt", msg="no false add after switching back")
    assert_file_set(entries, [], msg="status clean after parity add switch")
    assert not os.path.exists(repo._fix_path("added.txt")), (
        "added file removed by switch to main"
    )
    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base\n"


@pytest.mark.smoke
def test_switch_anchor_rebase_parity_delete(new_lore_repo):
    """A branch that committed a DELETE, switched back to main, leaves a clean
    status and restores the deleted file with main's content."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n", "victim.txt": "victim content\n"})

    repo.branch_create("other", offline=True)
    repo.remove_file("victim.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("delete on other", offline=True)

    repo.branch_switch("main", offline=True)

    assert not has_staged_anchor(repo), "no dirty work => anchor cleared"
    entries = get_status_files_twice(repo, scan=True)
    assert_absent(entries, "victim.txt", msg="no false delete after switching back")
    assert_file_set(entries, [], msg="status clean after parity delete switch")
    with repo.open_file("victim.txt", "r") as f:
        assert f.read() == "victim content\n", "deleted file restored on main"


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_switch_carries_dirty_move(new_lore_repo):
    """A dirty move on main is still reported (action=move, fromPath=source)
    after switching to a same-revision branch, and the on-disk rename is
    intact (source gone, destination present)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"old.txt": "movable content\n", "anchor.txt": "anchor\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    os.rename(repo._fix_path("old.txt"), repo._fix_path("new.txt"))
    repo.dirty_move("old.txt", "new.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(
        pre, "new.txt", action="move", dirty=True, staged=False, from_path="old.txt"
    )

    repo.branch_switch("other", offline=True)

    # The on-disk rename survives the switch.
    assert not os.path.exists(repo._fix_path("old.txt")), "move source gone on disk"
    with repo.open_file("new.txt", "r") as f:
        assert f.read() == "movable content\n", "move destination intact on disk"

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "new.txt",
        action="move",
        dirty=True,
        staged=False,
        from_path="old.txt",
        msg="dirty move provenance (action=move, fromPath) is carried across a same-revision switch",
    )


@pytest.mark.smoke
def test_switch_with_staged_node_present(new_lore_repo):
    """A same-revision branch switch with an actually-staged node must not
    silently lose the staged commit-intent.

    Sibling state-altering operations all refuse on a staged state (branch
    merge / cherry-pick / revert error "Cannot merge with staged state";
    branch reset errors "Unable to reset branch when there is a staged
    state"). branch switch must likewise not discard a genuine staged change:
    it must EITHER refuse with a LoreException OR carry the stage so the node
    still appears (flagStaged=True) afterward.
    """
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(
        pre,
        "staged.txt",
        action="add",
        dirty=True,
        staged=True,
        msg="staged before switch",
    )

    # The switch must not silently discard the staged commit-intent. Accept
    # either intended remedy: refuse (LoreException) or carry the stage.
    refused = False
    try:
        repo.branch_switch("other", offline=True)
    except LoreException:
        refused = True

    if refused:
        # Refusal path: the stage must survive on the original branch, exactly
        # like a refused reset / merge with a staged state.
        assert "On branch main" in repo.status(offline=True), (
            "a refused switch must leave the repo on the original branch"
        )
        after = get_status_files(repo)
        assert_entry(
            after,
            "staged.txt",
            action="add",
            dirty=True,
            staged=True,
            msg="staged node must survive a refused switch",
        )
    else:
        # Carry path: the switch succeeded, so the staged node is carried onto
        # the target branch.
        assert "On branch other" in repo.status(offline=True)
        assert os.path.exists(repo._fix_path("staged.txt")), (
            "staged file remains on disk"
        )
        after = get_status_files(repo)
        assert_entry(
            after,
            "staged.txt",
            action="add",
            dirty=True,
            staged=True,
            msg="staged node is carried across a same-revision switch",
        )


@pytest.mark.smoke
def test_switch_stale_dirty_does_not_block(new_lore_repo):
    """A stale dirty flag (file dirtied, then on-disk content rewritten to the
    committed content) does not block a branch switch."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)
    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")

    repo.branch_switch("other", offline=True)
    assert "On branch other" in repo.status(offline=True)


# ===========================================================================
# sync: change-type & dirty-carry matrix + anchor rebase
# ===========================================================================


@pytest.mark.smoke
def test_sync_back_then_forward_modify(new_lore_repo):
    """Syncing to an earlier revision of a modified file realizes that
    revision's content with a clean status; syncing forward again restores
    the newest content."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1 content\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 content longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    repo.sync(rev_v1, offline=True)
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v1 content\n", "sync(v1) should realize v1 content"
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean at v1 after sync back")

    repo.sync(offline=True)
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v2 content longer\n", "sync() forward should restore v2"
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean at v2 after sync forward")


@pytest.mark.smoke
def test_sync_anchor_rebase_no_false_mods(new_lore_repo):
    """After syncing back to v1, repeated `status --scan` must not invent a
    false modification: the synced file matches v1 exactly so it stays absent
    from status and on-disk content is v1."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1 content\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 content longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    repo.sync(rev_v1, offline=True)

    entries = get_status_files_twice(repo, scan=True)
    assert_absent(
        entries, "file.txt", msg="synced file must not be a false modification"
    )
    assert_file_set(entries, [], msg="no false mods after sync back + rescan")
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v1 content\n"


@pytest.mark.smoke
def test_sync_across_add_revision(new_lore_repo):
    """Syncing across a revision that adds a file: at the base revision the
    added file is absent on disk with a clean status; syncing forward
    materializes it, again clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})
    rev_base = repo.revision_history(offline=True)[0].signature

    with repo.open_file("new.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("add new.txt", offline=True)

    repo.sync(rev_base, offline=True)
    assert not os.path.exists(repo._fix_path("new.txt")), (
        "new.txt must be absent on disk at the base revision"
    )
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean at base across add")
    assert_absent(entries, "new.txt", msg="new.txt must not appear at base")

    repo.sync(offline=True)
    assert os.path.exists(repo._fix_path("new.txt")), (
        "new.txt must be present after syncing forward"
    )
    with repo.open_file("new.txt", "r") as f:
        assert f.read() == "added in v2\n"
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean after sync forward across add")


@pytest.mark.smoke
def test_sync_across_delete_revision(new_lore_repo):
    """Syncing across a revision that deletes a file: at the base revision the
    file is present on disk; syncing forward removes it. Status is clean at
    both endpoints."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"gone.txt": "will be deleted\n", "keep.txt": "stays\n"})
    rev_base = repo.revision_history(offline=True)[0].signature

    repo.remove_file("gone.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("delete gone.txt", offline=True)

    repo.sync(rev_base, offline=True)
    assert os.path.exists(repo._fix_path("gone.txt")), (
        "gone.txt must be restored on disk at the base revision"
    )
    with repo.open_file("gone.txt", "r") as f:
        assert f.read() == "will be deleted\n"
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean at base with file present")

    repo.sync(offline=True)
    assert not os.path.exists(repo._fix_path("gone.txt")), (
        "gone.txt must be absent after syncing forward across the delete"
    )
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean after sync forward across delete")


@pytest.mark.smoke
def test_sync_across_directory_restructure(new_lore_repo):
    """Syncing across a revision that restructures directories (moves files
    into a new nested dir and adds another) realizes each revision's tree
    exactly, with a clean status at both endpoints."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "src/old/a.txt": "alpha\n",
            "src/old/b.txt": "bravo\n",
            "top.txt": "top\n",
        },
    )
    rev_base = repo.revision_history(offline=True)[0].signature

    repo.make_dirs("src/new/deep")
    os.rename(repo._fix_path("src/old/a.txt"), repo._fix_path("src/new/deep/a.txt"))
    os.rename(repo._fix_path("src/old/b.txt"), repo._fix_path("src/new/deep/b.txt"))
    with repo.open_file("src/new/extra.txt", "w+") as f:
        f.write("extra\n")
    repo.stage(scan=True, offline=True)
    repo.commit("restructure", offline=True)

    repo.sync(rev_base, offline=True)
    assert os.path.exists(repo._fix_path("src/old/a.txt"))
    assert os.path.exists(repo._fix_path("src/old/b.txt"))
    assert not os.path.exists(repo._fix_path("src/new/deep/a.txt"))
    assert not os.path.exists(repo._fix_path("src/new/extra.txt"))
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean at base tree shape")

    repo.sync(offline=True)
    assert os.path.exists(repo._fix_path("src/new/deep/a.txt"))
    assert os.path.exists(repo._fix_path("src/new/deep/b.txt"))
    assert os.path.exists(repo._fix_path("src/new/extra.txt"))
    assert not os.path.exists(repo._fix_path("src/old/a.txt"))
    with repo.open_file("src/new/deep/a.txt", "r") as f:
        assert f.read() == "alpha\n"
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean at restructured tree shape")


@pytest.mark.smoke
def test_sync_stale_dirty_does_not_block(new_lore_repo):
    """A stale dirty flag (content reverted to match the current revision)
    does not block sync: syncing forward succeeds and realizes the target
    revision's content."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 longer content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    repo.sync(rev_v1, offline=True)
    # Stale dirty: mark dirty against an edit, then revert to match v1.
    with repo.open_file("file.txt", "w+") as f:
        f.write("scratch edit longer\n")
    repo.dirty("file.txt", offline=True)
    with repo.open_file("file.txt", "w+") as f:
        f.write("v1\n")

    repo.sync(offline=True)
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v2 longer content\n", (
            "stale dirty must not block sync from realizing v2"
        )
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="status clean after sync forward over stale dirty")


@pytest.mark.smoke
def test_sync_pending_genuine_dirty_carries_modification(new_lore_repo):
    """Sync with a genuine pending dirty modification rebases the dirty leaf
    onto the target revision (same anchor-rebase model as branch switch). The
    dirty modification is carried — its content stays on disk over the synced
    base and the path is still reported dirty."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1\n", "other.txt": "other v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("other.txt", "w+") as f:
        f.write("other v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    # On v2, genuinely dirty file.txt (untouched by v2) and sync back to v1.
    with repo.open_file("file.txt", "w+") as f:
        f.write("genuine local edit longer\n")
    repo.dirty("file.txt", offline=True)

    repo.sync(rev_v1, offline=True)

    # The carried dirty modification survives onto the v1 base.
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "genuine local edit longer\n", (
            "genuine pending dirty modification must be carried across sync, not lost"
        )
    with repo.open_file("other.txt", "r") as f:
        assert f.read() == "other v1\n", "non-dirty file follows the synced revision"

    entries = get_status_files_twice(repo)
    assert_entry(
        entries,
        "file.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="carried dirty modify is still reported after sync",
    )
    assert_file_set(entries, ["file.txt"], msg="only the carried dirty path is pending")


@pytest.mark.smoke
def test_sync_idempotent_scan_after_changes(new_lore_repo):
    """After syncing across an add+delete revision, `status --scan` is
    idempotent and reports nothing — the working tree matches the synced
    revision exactly with no residual dirty state."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"gone.txt": "to delete\n", "stable.txt": "stable\n"})
    rev_base = repo.revision_history(offline=True)[0].signature

    repo.remove_file("gone.txt")
    with repo.open_file("added.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("delete gone.txt, add added.txt", offline=True)

    # Sync back to base: gone.txt returns, added.txt disappears.
    repo.sync(rev_base, offline=True)
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="scan idempotent + empty at base after sync")
    assert os.path.exists(repo._fix_path("gone.txt"))
    assert not os.path.exists(repo._fix_path("added.txt"))

    # Sync forward to v2: gone.txt disappears, added.txt returns.
    repo.sync(offline=True)
    entries = get_status_files_twice(repo, scan=True)
    assert_file_set(entries, [], msg="scan idempotent + empty at v2 after sync forward")
    assert not os.path.exists(repo._fix_path("gone.txt"))
    assert os.path.exists(repo._fix_path("added.txt"))


# ===========================================================================
# branch merge -- CLEAN (non-overlapping) change-type matrix, each with a
# dirty-only carry on an unrelated path
# ===========================================================================


@pytest.mark.smoke
def test_mergeclean_featadd_mainadd_carry_modify(new_lore_repo):
    """Clean merge where feature adds fa.txt and main adds ma.txt; an
    unrelated dirty-only modify of base.txt is carried through the merge's
    auto-commit. After merge both added files are committed (clean) and the
    carry remains flagDirty=True/flagStaged=False."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("fa.txt", "w+") as f:
        f.write("feature add\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature add fa.txt", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("ma.txt", "w+") as f:
        f.write("main add\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main add ma.txt", offline=True)

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited longer\n")
    repo.dirty("base.txt", offline=True)

    repo.branch_merge("feature", offline=True)

    with repo.open_file("fa.txt", "r") as f:
        assert f.read() == "feature add\n", "fa.txt must reflect feature's add on disk"
    with repo.open_file("ma.txt", "r") as f:
        assert f.read() == "main add\n", "ma.txt must reflect main's add on disk"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["base.txt"], msg="only the dirty carry should remain pending"
    )
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty modify carry survives the clean merge",
    )
    assert_absent(entries, "fa.txt", msg="feature add is committed and clean")
    assert_absent(entries, "ma.txt", msg="main add is committed and clean")


@pytest.mark.smoke
def test_mergeclean_featmodify_mainmodify_carry_add(new_lore_repo):
    """Clean merge where feature modifies f.txt and main modifies m.txt; an
    unrelated dirty-only add of a file in a brand-new directory is carried
    through, exercising carry recreation of intermediate directory nodes.
    After merge both modifications are committed (clean) and the dirty-add
    carry remains pending."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"f.txt": "f original\n", "m.txt": "m original\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("f.txt", "w+") as f:
        f.write("f modified on feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature modifies f.txt", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("m.txt", "w+") as f:
        f.write("m modified on main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main modifies m.txt", offline=True)

    repo.make_dirs("carrydir")
    with repo.open_file("carrydir/newcarry.txt", "w+") as f:
        f.write("dirty carry add\n")
    repo.dirty("carrydir/newcarry.txt", offline=True)

    repo.branch_merge("feature", offline=True)

    with repo.open_file("f.txt", "r") as f:
        assert f.read() == "f modified on feature\n", (
            "f.txt must reflect feature change"
        )
    with repo.open_file("m.txt", "r") as f:
        assert f.read() == "m modified on main\n", "m.txt must reflect main change"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["carrydir/newcarry.txt"], msg="only the dirty-add carry remains"
    )
    assert_entry(
        entries,
        "carrydir/newcarry.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-add carry survives the clean merge",
    )
    assert_entry(
        entries,
        "carrydir",
        action="add",
        dirty=True,
        staged=False,
        node_type="directory",
        msg="the carried new directory node is recreated by the carry replay",
    )
    assert_absent(entries, "f.txt", msg="feature modify is committed and clean")
    assert_absent(entries, "m.txt", msg="main modify is committed and clean")


@pytest.mark.smoke
def test_mergeclean_featdelete_maindelete_carry_delete(new_lore_repo):
    """Clean merge where feature deletes fd.txt and main deletes md.txt; an
    unrelated dirty-only delete of carrydel.txt is carried through. After
    merge both target files are gone from disk and committed-clean, and the
    explicitly dirty-marked delete carry remains pending (action=delete)."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "fd.txt": "feature deletes me\n",
            "md.txt": "main deletes me\n",
            "carrydel.txt": "carry deletes me\n",
        },
    )

    repo.branch_create("feature", offline=True)
    repo.remove_file("fd.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("feature deletes fd.txt", offline=True)
    repo.branch_switch("main", offline=True)

    repo.remove_file("md.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("main deletes md.txt", offline=True)

    repo.remove_file("carrydel.txt")
    repo.dirty("carrydel.txt", offline=True)

    repo.branch_merge("feature", offline=True)

    assert not os.path.exists(repo._fix_path("fd.txt")), "fd.txt deleted by feature"
    assert not os.path.exists(repo._fix_path("md.txt")), "md.txt deleted by main"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["carrydel.txt"], msg="only the dirty-delete carry remains"
    )
    assert_entry(
        entries,
        "carrydel.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="explicitly dirty-marked delete carry survives merge",
    )
    assert_absent(entries, "fd.txt", msg="feature delete committed and clean")
    assert_absent(entries, "md.txt", msg="main delete committed and clean")


@pytest.mark.smoke
def test_mergeclean_featadd_maindelete_carry_modify(new_lore_repo):
    """Clean merge where feature adds fa.txt and main deletes a different
    committed file md.txt; an unrelated dirty-only modify of base.txt is
    carried. After merge fa.txt is present, md.txt is gone, both clean, and
    the carry remains pending."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n", "md.txt": "main deletes me\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("fa.txt", "w+") as f:
        f.write("feature add\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature adds fa.txt", offline=True)
    repo.branch_switch("main", offline=True)

    repo.remove_file("md.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("main deletes md.txt", offline=True)

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited longer\n")
    repo.dirty("base.txt", offline=True)

    repo.branch_merge("feature", offline=True)

    with repo.open_file("fa.txt", "r") as f:
        assert f.read() == "feature add\n", "fa.txt must reflect feature's add"
    assert not os.path.exists(repo._fix_path("md.txt")), "md.txt deleted by main"

    entries = get_status_files(repo)
    assert_file_set(entries, ["base.txt"], msg="only the dirty carry remains")
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty modify carry survives the clean add/delete merge",
    )
    assert_absent(entries, "fa.txt", msg="feature add committed and clean")
    assert_absent(entries, "md.txt", msg="main delete committed and clean")


@pytest.mark.smoke
def test_mergeclean_featmodify_mainadd_carry_modify(new_lore_repo):
    """Clean merge where feature modifies f.txt and main adds ma.txt; an
    unrelated dirty-only modify of base.txt is carried. After merge the
    modification and the add are committed-clean and the carry remains
    pending."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"f.txt": "f original\n", "base.txt": "base original\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("f.txt", "w+") as f:
        f.write("f modified on feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature modifies f.txt", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("ma.txt", "w+") as f:
        f.write("main add\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main adds ma.txt", offline=True)

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited longer\n")
    repo.dirty("base.txt", offline=True)

    repo.branch_merge("feature", offline=True)

    with repo.open_file("f.txt", "r") as f:
        assert f.read() == "f modified on feature\n", (
            "f.txt must reflect feature change"
        )
    with repo.open_file("ma.txt", "r") as f:
        assert f.read() == "main add\n", "ma.txt must reflect main's add"

    entries = get_status_files(repo)
    assert_file_set(entries, ["base.txt"], msg="only the dirty carry remains")
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty modify carry survives the clean modify/add merge",
    )
    assert_absent(entries, "f.txt", msg="feature modify committed and clean")
    assert_absent(entries, "ma.txt", msg="main add committed and clean")


@pytest.mark.smoke
def test_mergeclean_featdelete_mainmodify_carry_add(new_lore_repo):
    """Clean merge where feature deletes fd.txt and main modifies m.txt; an
    unrelated dirty-only add in a brand-new directory is carried (exercising
    carry dir-node recreation). After merge fd.txt is gone, m.txt reflects
    main's change, both clean, and the dirty-add carry remains pending."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"fd.txt": "feature deletes me\n", "m.txt": "m original\n"})

    repo.branch_create("feature", offline=True)
    repo.remove_file("fd.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("feature deletes fd.txt", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("m.txt", "w+") as f:
        f.write("m modified on main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main modifies m.txt", offline=True)

    repo.make_dirs("carrydir")
    with repo.open_file("carrydir/newcarry.txt", "w+") as f:
        f.write("dirty carry add\n")
    repo.dirty("carrydir/newcarry.txt", offline=True)

    repo.branch_merge("feature", offline=True)

    assert not os.path.exists(repo._fix_path("fd.txt")), "fd.txt deleted by feature"
    with repo.open_file("m.txt", "r") as f:
        assert f.read() == "m modified on main\n", "m.txt must reflect main change"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["carrydir/newcarry.txt"], msg="only the dirty-add carry remains"
    )
    assert_entry(
        entries,
        "carrydir/newcarry.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-add carry survives the clean delete/modify merge",
    )
    assert_entry(
        entries,
        "carrydir",
        action="add",
        dirty=True,
        staged=False,
        node_type="directory",
        msg="the carried new directory node is recreated by the carry replay",
    )
    assert_absent(entries, "fd.txt", msg="feature delete committed and clean")
    assert_absent(entries, "m.txt", msg="main modify committed and clean")


@pytest.mark.smoke
def test_mergeclean_clean_merge_no_carry_baseline(new_lore_repo):
    """Baseline: a clean merge (feature adds fa.txt, main adds ma.txt) with NO
    dirty carry leaves a fully clean working tree -- status is empty and the
    staged anchor is cleared after the merge auto-commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("fa.txt", "w+") as f:
        f.write("feature add\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature adds fa.txt", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("ma.txt", "w+") as f:
        f.write("main add\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main adds ma.txt", offline=True)

    repo.branch_merge("feature", offline=True)

    with repo.open_file("fa.txt", "r") as f:
        assert f.read() == "feature add\n", "fa.txt must reflect feature's add"
    with repo.open_file("ma.txt", "r") as f:
        assert f.read() == "main add\n", "ma.txt must reflect main's add"

    entries = get_status_files(repo)
    assert entries == [], (
        f"status should be empty after a clean no-carry merge, got {summarize(entries)}"
    )
    assert not has_staged_anchor(repo), (
        "anchor should be cleared after a clean no-carry merge"
    )


@pytest.mark.smoke
def test_mergeclean_refuses_on_staged(new_lore_repo):
    """`branch merge` refuses to start when an actually-staged node exists,
    raising a LoreException with "Cannot merge with staged state"; the staged
    node remains staged after the refusal."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("fa.txt", "w+") as f:
        f.write("feature add\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature adds fa.txt", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.branch_merge("feature", offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"merge must refuse on actually-staged nodes, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "staged.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged.txt must remain staged after the rejected merge",
    )
    assert_absent(
        entries, "fa.txt", msg="feature add must not land after a refused merge"
    )


# ===========================================================================
# branch merge — CONFLICT (same-path) matrix + resolve mine/theirs + dir-level
# + abort. The merge pattern is always: base on main; feature edits P;
# back on main edit the SAME P (conflicting); carry an unrelated dirty-only
# change; merge feature INTO main. Because we are ON main, "mine" == main's
# content and "theirs" == feature's content.
# ===========================================================================


def _mergeconflict_unresolved(repo: Lore) -> set[str]:
    """Posix paths currently flagged as unresolved merge conflicts."""
    return {
        to_posix(e["path"])
        for e in get_status_files(repo)
        if e.get("flagConflictUnresolved") is True
    }


@pytest.mark.smoke
def test_mergeconflict_modify_modify_resolve_mine(new_lore_repo):
    """modify/modify conflict resolved with 'mine' keeps main's content and the
    unrelated dirty-only carry survives the merge commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "carry.txt": "carry base\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edits conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    with repo.open_file("carry.txt", "w+") as f:
        f.write("carry locally edited\n")
    repo.dirty("carry.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected merge to surface a conflict, got:\n{merge_output}"
    )
    assert "conflict.txt" in _mergeconflict_unresolved(repo), (
        "conflict.txt should be an unresolved conflict before resolve"
    )

    repo.branch_merge_resolve_mine("conflict.txt", offline=True)
    repo.commit("merge resolved mine", offline=True)

    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "main\n", "resolve mine must keep main's content"

    entries = get_status_files(repo)
    assert_file_set(entries, ["carry.txt"], msg="only the dirty carry should remain")
    assert_entry(
        entries,
        "carry.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="dirty carry must survive the conflicted merge",
    )
    assert_absent(entries, "conflict.txt", msg="resolved conflict is clean post-commit")


@pytest.mark.smoke
def test_mergeconflict_modify_modify_resolve_theirs(new_lore_repo):
    """modify/modify conflict resolved with 'theirs' takes feature's content and
    the unrelated dirty-only carry survives the merge commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "carry.txt": "carry base\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edits conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    with repo.open_file("carry.txt", "w+") as f:
        f.write("carry locally edited\n")
    repo.dirty("carry.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected merge to surface a conflict, got:\n{merge_output}"
    )
    assert "conflict.txt" in _mergeconflict_unresolved(repo)

    repo.branch_merge_resolve_theirs("conflict.txt", offline=True)
    repo.commit("merge resolved theirs", offline=True)

    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "feature\n", "resolve theirs must take feature's content"

    entries = get_status_files(repo)
    assert_file_set(entries, ["carry.txt"], msg="only the dirty carry should remain")
    assert_entry(
        entries,
        "carry.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="dirty carry must survive the conflicted merge",
    )
    assert_absent(entries, "conflict.txt", msg="resolved conflict is clean post-commit")


@pytest.mark.smoke
def test_mergeconflict_delete_modify_resolve_mine(new_lore_repo):
    """delete/modify conflict (feature deletes P, main modifies P) resolved with
    'mine' keeps main's modified file on disk; the dirty carry survives."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "carry.txt": "carry base\n"})

    repo.branch_create("feature", offline=True)
    repo.remove_file("conflict.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("feature deletes conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main modified\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main modifies conflict.txt", offline=True)

    with repo.open_file("carry.txt", "w+") as f:
        f.write("carry locally edited\n")
    repo.dirty("carry.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected delete/modify merge to surface a conflict, got:\n{merge_output}"
    )
    assert "conflict.txt" in _mergeconflict_unresolved(repo), (
        "conflict.txt should be an unresolved delete/modify conflict"
    )

    # 'mine' on main = keep main's modification.
    repo.branch_merge_resolve_mine("conflict.txt", offline=True)
    repo.commit("merge resolved mine", offline=True)

    assert os.path.exists(repo._fix_path("conflict.txt")), (
        "resolve mine keeps main's modified file"
    )
    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "main modified\n"

    entries = get_status_files(repo)
    assert_file_set(entries, ["carry.txt"], msg="only the dirty carry should remain")
    assert_entry(entries, "carry.txt", action="keep", dirty=True, staged=False)
    assert_absent(entries, "conflict.txt", msg="resolved conflict is clean post-commit")


@pytest.mark.smoke
def test_mergeconflict_delete_modify_resolve_theirs(new_lore_repo):
    """delete/modify conflict (feature deletes P, main modifies P) resolved with
    'theirs' takes feature's delete: the file is removed from disk and absent
    after the merge commit; the dirty carry survives."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "carry.txt": "carry base\n"})

    repo.branch_create("feature", offline=True)
    repo.remove_file("conflict.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("feature deletes conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main modified\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main modifies conflict.txt", offline=True)

    with repo.open_file("carry.txt", "w+") as f:
        f.write("carry locally edited\n")
    repo.dirty("carry.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected delete/modify merge to surface a conflict, got:\n{merge_output}"
    )
    assert "conflict.txt" in _mergeconflict_unresolved(repo), (
        "conflict.txt should be an unresolved delete/modify conflict"
    )

    # 'theirs' = feature's delete — the file goes away.
    repo.branch_merge_resolve_theirs("conflict.txt", offline=True)
    resolved = assert_entry(
        get_status_files(repo),
        "conflict.txt",
        action="delete",
        msg="resolving to theirs (delete) marks the path as a resolved delete",
    )
    assert resolved.get("flagConflict") is True
    assert resolved.get("flagConflictUnresolved") is False
    assert not os.path.exists(repo._fix_path("conflict.txt")), (
        "resolve theirs (delete) removes the file from disk"
    )

    repo.commit("merge resolved theirs", offline=True)

    assert not os.path.exists(repo._fix_path("conflict.txt")), (
        "deleted file stays gone after the merge commit"
    )
    entries = get_status_files(repo)
    assert_file_set(entries, ["carry.txt"], msg="only the dirty carry should remain")
    assert_entry(entries, "carry.txt", action="keep", dirty=True, staged=False)
    assert_absent(entries, "conflict.txt", msg="resolved delete is clean post-commit")


@pytest.mark.smoke
def test_mergeconflict_modify_delete_resolve_theirs(new_lore_repo):
    """modify/delete conflict (feature modifies P, main deletes P) resolved with
    'theirs' takes feature's modification back onto disk; carry survives."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "carry.txt": "carry base\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature modified\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature modifies conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    repo.remove_file("conflict.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("main deletes conflict.txt", offline=True)

    with repo.open_file("carry.txt", "w+") as f:
        f.write("carry locally edited\n")
    repo.dirty("carry.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected modify/delete merge to surface a conflict, got:\n{merge_output}"
    )
    assert "conflict.txt" in _mergeconflict_unresolved(repo), (
        "conflict.txt should be an unresolved modify/delete conflict"
    )

    # 'theirs' = feature's modification — the file comes back.
    repo.branch_merge_resolve_theirs("conflict.txt", offline=True)
    repo.commit("merge resolved theirs", offline=True)

    assert os.path.exists(repo._fix_path("conflict.txt")), (
        "resolve theirs restores feature's modified file"
    )
    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "feature modified\n"

    entries = get_status_files(repo)
    assert_file_set(entries, ["carry.txt"], msg="only the dirty carry should remain")
    assert_entry(entries, "carry.txt", action="keep", dirty=True, staged=False)
    assert_absent(entries, "conflict.txt", msg="resolved conflict is clean post-commit")


@pytest.mark.smoke
def test_mergeconflict_add_add_resolve_mine(new_lore_repo):
    """add/add conflict (both branches add the SAME new path with different
    content) resolved with 'mine' keeps main's added content; carry survives."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n", "carry.txt": "carry base\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("added.txt", "w+") as f:
        f.write("feature added\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature adds added.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("added.txt", "w+") as f:
        f.write("main added\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main adds added.txt", offline=True)

    with repo.open_file("carry.txt", "w+") as f:
        f.write("carry locally edited\n")
    repo.dirty("carry.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected add/add merge to surface a conflict, got:\n{merge_output}"
    )
    assert "added.txt" in _mergeconflict_unresolved(repo), (
        "added.txt should be an unresolved add/add conflict"
    )

    repo.branch_merge_resolve_mine("added.txt", offline=True)
    repo.commit("merge resolved mine", offline=True)

    with repo.open_file("added.txt", "r") as f:
        assert f.read() == "main added\n", "resolve mine keeps main's added content"

    entries = get_status_files(repo)
    assert_file_set(entries, ["carry.txt"], msg="only the dirty carry should remain")
    assert_entry(entries, "carry.txt", action="keep", dirty=True, staged=False)
    assert_absent(entries, "added.txt", msg="resolved add/add is clean post-commit")


@pytest.mark.smoke
def test_merge_dir_delete_vs_file_add_clean(new_lore_repo):
    """Feature deletes every committed file under directory d/; main adds a NEW
    file d/new.txt into that same directory. The two changes touch disjoint leaf
    paths, so the merge applies cleanly with NO conflict and auto-commits: the
    feature deletes land, main's d/new.txt survives, and the unrelated dirty
    carry survives the auto-commit. A delete of a directory's existing leaves
    does not conflict with a sibling add."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "d/keep.txt": "keep base\n",
            "d/other.txt": "other base\n",
            "carry.txt": "carry base\n",
        },
    )

    repo.branch_create("feature", offline=True)
    repo.remove_file("d/keep.txt")
    repo.remove_file("d/other.txt")
    repo.stage(scan=True, offline=True)
    repo.commit("feature deletes directory d/", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("d/new.txt", "w+") as f:
        f.write("main new file\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main adds d/new.txt", offline=True)

    with repo.open_file("carry.txt", "w+") as f:
        f.write("carry locally edited\n")
    repo.dirty("carry.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert " 0 conflicted" in merge_output, (
        f"disjoint dir-delete vs file-add should merge cleanly, got:\n{merge_output}"
    )
    assert not _mergeconflict_unresolved(repo), (
        "no conflict expected — merge auto-commits"
    )

    # The clean merge auto-committed: feature's deletes landed, main's new file
    # under the otherwise-emptied dir survives.
    assert os.path.exists(repo._fix_path("d/new.txt")), "main's d/new.txt must survive"
    with repo.open_file("d/new.txt", "r") as f:
        assert f.read() == "main new file\n"
    assert not os.path.exists(repo._fix_path("d/keep.txt")), (
        "feature's delete must land"
    )
    assert not os.path.exists(repo._fix_path("d/other.txt")), (
        "feature's delete must land"
    )

    entries = get_status_files(repo)
    assert_file_set(entries, ["carry.txt"], msg="only the dirty carry should remain")
    assert_entry(
        entries,
        "carry.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="dirty carry must survive the dir-delete vs file-add merge",
    )
    assert_absent(entries, "d/new.txt", msg="d/new.txt is committed and clean")


@pytest.mark.smoke
def test_mergeconflict_carry_add_new_dir_through_conflict(new_lore_repo):
    """A dirty ADD in a brand-new nested directory survives a conflicted merge
    all the way through resolve + commit — the carry replay must recreate the
    intermediate directory nodes in the fresh staged state."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edits conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    # Dirty add in a directory hierarchy that exists in no committed revision —
    # the carry must recreate dir nodes when replayed onto the merge commit.
    repo.make_dirs("new_dir/sub")
    with repo.open_file("new_dir/sub/added.txt", "w+") as f:
        f.write("brand new nested\n")
    repo.dirty("new_dir/sub/added.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected merge to surface a conflict, got:\n{merge_output}"
    )
    assert "conflict.txt" in _mergeconflict_unresolved(repo)

    repo.branch_merge_resolve_mine("conflict.txt", offline=True)
    repo.commit("merge resolved mine", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries,
        ["new_dir/sub/added.txt"],
        msg="only the dirty-add carry should remain after the merge",
    )
    assert_entry(
        entries,
        "new_dir/sub/added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-add carry must survive + keep action=add",
    )
    # The recreated directory nodes for the carried add must be present.
    assert_entry(entries, "new_dir", node_type="directory", action="add")
    assert_entry(entries, "new_dir/sub", node_type="directory", action="add")
    assert_absent(entries, "conflict.txt", msg="resolved conflict is clean post-commit")


@pytest.mark.smoke
def test_mergeconflict_abort_keeps_carry(new_lore_repo):
    """`merge abort` cancels the merge but preserves the pre-existing dirty-only
    carry: carry.txt remains a dirty modify (action=keep/flagDirty) after the
    abort with its on-disk edit intact, and survives an unrelated later commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "carry.txt": "carry base\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edits conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    with repo.open_file("carry.txt", "w+") as f:
        f.write("carry locally edited\n")
    repo.dirty("carry.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected merge to surface a conflict, got:\n{merge_output}"
    )
    assert "conflict.txt" in _mergeconflict_unresolved(repo)

    repo.branch_merge_abort(offline=True)

    # Abort cancels the merge but keeps the unrelated dirty-only carry.
    after_abort = get_status_files(repo)
    assert_entry(
        after_abort,
        "carry.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="merge abort preserves the pre-existing dirty-only carry",
    )
    assert_file_set(
        after_abort, ["carry.txt"], msg="only the carry remains after abort"
    )
    with repo.open_file("carry.txt", "r") as f:
        assert f.read() == "carry locally edited\n", (
            "carry on-disk edit is intact after abort"
        )

    # An unrelated staged commit afterwards commits only post.txt; the dirty-only
    # carry survives that commit.
    with repo.open_file("post.txt", "w+") as f:
        f.write("post-abort\n")
    repo.stage("post.txt", offline=True)
    repo.commit("post-abort commit", offline=True)

    entries = get_status_files(repo)
    assert_absent(entries, "post.txt", msg="staged commit should be clean after commit")
    assert_entry(
        entries,
        "carry.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="the dirty-only carry survives an unrelated commit",
    )
    assert_file_set(entries, ["carry.txt"], msg="only the surviving carry remains")


# ===========================================================================
# revision revert -- dirty carry across change types (clean + conflict)
# ===========================================================================


@pytest.mark.smoke
def test_revert_clean_carry_modify(new_lore_repo):
    """Clean revert of an add commit removes the added file while a dirty
    MODIFY carry on an unrelated base file survives as action=keep/dirty."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n"})

    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 add revertable", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited longer\n")
    repo.dirty("base.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "base.txt", action="keep", dirty=True, staged=False)
    assert_file_set(pre, ["base.txt"], msg="only the dirty modify pending pre-revert")

    repo.revision_revert(rev_v2, offline=True)

    assert not os.path.exists(repo._fix_path("revertable.txt")), (
        "reverted add should be removed from disk"
    )
    entries = get_status_files(repo)
    assert_absent(entries, "revertable.txt", msg="reverted add is gone from status")
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="dirty modify carry must survive a clean revert",
    )
    assert_file_set(entries, ["base.txt"], msg="only the carried modify remains")
    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base locally edited longer\n", (
            "carried dirty content must remain on disk"
        )


@pytest.mark.smoke
def test_revert_clean_carry_add_new_dir(new_lore_repo):
    """Clean revert with a dirty ADD in a brand-new nested directory: the
    carry survives and its intermediate directory nodes are recreated in the
    fresh staged state (action=add for the leaf and the new dir nodes)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n"})

    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 add revertable", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    repo.make_dirs("dirty_dir/nested")
    with repo.open_file("dirty_dir/nested/dirty_file.txt", "w+") as f:
        f.write("new dirty content\n")
    repo.dirty("dirty_dir/nested/dirty_file.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(
        pre, "dirty_dir/nested/dirty_file.txt", action="add", dirty=True, staged=False
    )

    repo.revision_revert(rev_v2, offline=True)

    assert not os.path.exists(repo._fix_path("revertable.txt"))
    entries = get_status_files(repo)
    assert_absent(entries, "revertable.txt", msg="reverted add gone from status")
    assert_entry(
        entries,
        "dirty_dir/nested/dirty_file.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty add carry must survive a clean revert",
    )
    assert_entry(
        entries,
        "dirty_dir",
        action="add",
        node_type="directory",
        msg="carry replay must recreate the new directory node",
    )
    assert_entry(
        entries,
        "dirty_dir/nested",
        action="add",
        node_type="directory",
        msg="carry replay must recreate the nested directory node",
    )
    assert_file_set(
        entries,
        ["dirty_dir/nested/dirty_file.txt"],
        msg="only the carried add leaf remains as a file entry",
    )


@pytest.mark.smoke
def test_revert_clean_carry_delete(new_lore_repo):
    """Clean revert with a dirty DELETE carry: the explicit dirty-delete of an
    unrelated committed file survives as action=delete/flagDirty=True."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n", "victim.txt": "delete me\n"})

    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 add revertable", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "victim.txt", action="delete", dirty=True, staged=False)
    assert_file_set(pre, ["victim.txt"], msg="only the dirty delete pending pre-revert")

    repo.revision_revert(rev_v2, offline=True)

    assert not os.path.exists(repo._fix_path("revertable.txt"))
    entries = get_status_files(repo)
    assert_absent(entries, "revertable.txt", msg="reverted add gone from status")
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        msg="dirty delete carry must survive a clean revert",
    )
    assert_file_set(entries, ["victim.txt"], msg="only the carried delete remains")
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "carried dirty delete must keep the file absent on disk"
    )


@pytest.mark.smoke
def test_revert_conflict_resolve_mine(new_lore_repo):
    """A conflicted revert resolved with `resolve mine` keeps the current-branch
    (HEAD) content for the conflicting file; the unrelated dirty carry survives
    the eventual commit. "Mine" is the current-branch HEAD side of the revert."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"target.txt": "v1\n", "untouched.txt": "untouched\n"})

    with repo.open_file("target.txt", "w+") as f:
        f.write("v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 to be reverted", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("target.txt", "w+") as f:
        f.write("v3\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v3", offline=True)

    with repo.open_file("untouched.txt", "w+") as f:
        f.write("locally edited longer\n")
    repo.dirty("untouched.txt", offline=True)

    revert_output = repo.revision_revert(rev_v2, offline=True)
    assert "conflicted" in revert_output, (
        f"reverting v2 should conflict with v3, got:\n{revert_output}"
    )

    repo.revision_revert_resolve_mine("target.txt", offline=True)
    repo.commit("revert resolved mine", offline=True)

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "untouched.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="dirty carry must survive a conflicted revert resolved mine",
    )
    assert_absent(entries, "target.txt", msg="target.txt clean after resolve + commit")
    assert_file_set(entries, ["untouched.txt"], msg="only the carry remains pending")
    with repo.open_file("target.txt", "r") as f:
        assert f.read() == "v3\n", "resolve mine keeps the current HEAD (v3) content"


@pytest.mark.smoke
def test_revert_conflict_resolve_theirs(new_lore_repo):
    """A conflicted revert resolved with `resolve theirs` takes the revert
    result for the conflicting file -- the content of the parent of the
    reverted revision (v1). The unrelated dirty carry survives the commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"target.txt": "v1\n", "untouched.txt": "untouched\n"})

    with repo.open_file("target.txt", "w+") as f:
        f.write("v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 to be reverted", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("target.txt", "w+") as f:
        f.write("v3\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v3", offline=True)

    with repo.open_file("untouched.txt", "w+") as f:
        f.write("locally edited longer\n")
    repo.dirty("untouched.txt", offline=True)

    revert_output = repo.revision_revert(rev_v2, offline=True)
    assert "conflicted" in revert_output, (
        f"reverting v2 should conflict with v3, got:\n{revert_output}"
    )

    repo.revision_revert_resolve_theirs("target.txt", offline=True)
    repo.commit("revert resolved theirs", offline=True)

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "untouched.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="dirty carry must survive a conflicted revert resolved theirs",
    )
    assert_absent(entries, "target.txt", msg="target.txt clean after resolve + commit")
    assert_file_set(entries, ["untouched.txt"], msg="only the carry remains pending")
    with repo.open_file("target.txt", "r") as f:
        assert f.read() == "v1\n", (
            "resolve theirs takes the revert result: the parent of the "
            "reverted revision (v1)"
        )


@pytest.mark.smoke
def test_revert_refuses_on_staged(new_lore_repo):
    """`revision revert` refuses to start when an actually-STAGED node exists;
    the staged node is left untouched by the rejected operation."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base v1\n"})

    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 add revertable", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged.txt", offline=True)

    from error_types import LoreException

    with pytest.raises(LoreException) as excinfo:
        repo.revision_revert(rev_v2, offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"revert must refuse on actually-staged nodes, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "staged.txt",
        action="add",
        dirty=True,
        staged=True,
        msg="the staged node must survive the rejected revert",
    )
    assert os.path.exists(repo._fix_path("revertable.txt")), (
        "the revert was rejected so revertable.txt is still present"
    )


@pytest.mark.smoke
def test_revert_abort_keeps_carry(new_lore_repo):
    """`revert abort` cancels the revert but preserves the pre-existing dirty-only
    carry: base.txt remains a dirty modify (action=keep/flagDirty) after the abort
    with its on-disk edit intact, and survives an unrelated later commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n", "target.txt": "v1\n"})

    with repo.open_file("target.txt", "w+") as f:
        f.write("v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("target.txt", "w+") as f:
        f.write("v3\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v3", offline=True)

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited longer\n")
    repo.dirty("base.txt", offline=True)

    revert_output = repo.revision_revert(rev_v2, offline=True)
    assert "conflicted" in revert_output, (
        f"reverting v2 should conflict with v3, got:\n{revert_output}"
    )
    repo.revision_revert_abort(offline=True)

    # Abort cancels the revert but keeps the unrelated dirty-only carry.
    after_abort = get_status_files(repo)
    assert_entry(
        after_abort,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="revert abort preserves the pre-existing dirty-only carry",
    )
    assert_file_set(after_abort, ["base.txt"], msg="only the carry remains after abort")
    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base locally edited longer\n", (
            "carry on-disk edit is intact after abort"
        )

    with repo.open_file("staged_post.txt", "w+") as f:
        f.write("post-abort\n")
    repo.stage("staged_post.txt", offline=True)
    repo.commit("post-abort commit", offline=True)

    entries = get_status_files(repo)
    assert_absent(entries, "staged_post.txt", msg="committed staged file is clean")
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="the dirty-only carry survives an unrelated commit",
    )
    assert_file_set(entries, ["base.txt"], msg="only the surviving carry remains")


# ===========================================================================
# revision cherry-pick — dirty carry across change types (clean + conflict)
# ===========================================================================


def _commit_source_rev(repo: Lore, files: dict[str, str], message: str) -> str:
    """On a fresh `source` branch, commit `files` and return the signature of
    the resulting (head) revision; then switch back to main."""
    repo.branch_create("source", offline=True)
    repo.write_files(files)
    repo.stage(scan=True, offline=True)
    repo.commit(message, offline=True)
    rev = repo.revision_history(1, offline=True)[0].signature
    assert len(rev) == 64, f"expected a 64-char revision signature, got {rev!r}"
    repo.branch_switch("main", offline=True)
    return rev


@pytest.mark.smoke
def test_cherrypick_clean_carry_modify(new_lore_repo):
    """A dirty-only content modification (action=keep) on main survives a clean
    cherry-pick: the picked file lands clean in the committed tree while the
    carry is replayed as a pending dirty modify."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n"})

    source_rev = _commit_source_rev(
        repo, {"from_source.txt": "source content\n"}, "source adds file"
    )

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited\n")
    repo.dirty("base.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(
        pre, "base.txt", action="keep", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(pre, ["base.txt"], msg="only the dirty carry is pending pre-pick")

    repo.revision_cherry_pick(source_rev, offline=True)

    assert os.path.exists(repo._fix_path("from_source.txt"))
    with repo.open_file("from_source.txt", "r") as f:
        assert f.read() == "source content\n"

    entries = get_status_files(repo)
    assert_absent(entries, "from_source.txt", msg="picked file is committed and clean")
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty modify carry must survive the clean pick",
    )
    assert_file_set(
        entries, ["base.txt"], msg="only the carry remains pending after pick"
    )
    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base locally edited\n"


@pytest.mark.smoke
def test_cherrypick_clean_carry_add_new_dir(new_lore_repo):
    """A dirty ADD in a brand-new nested directory survives a clean cherry-pick;
    the carry replay recreates the intermediate directory nodes in the fresh
    staged state."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    source_rev = _commit_source_rev(
        repo, {"from_source.txt": "source content\n"}, "source adds file"
    )

    repo.make_dirs("carry_dir/nested")
    with repo.open_file("carry_dir/nested/added.txt", "w+") as f:
        f.write("dirty new content\n")
    repo.dirty("carry_dir/nested/added.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(
        pre,
        "carry_dir/nested/added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_file_set(
        pre, ["carry_dir/nested/added.txt"], msg="only the dirty add is pending"
    )

    repo.revision_cherry_pick(source_rev, offline=True)

    assert os.path.exists(repo._fix_path("from_source.txt"))
    entries = get_status_files(repo)
    assert_absent(entries, "from_source.txt", msg="picked file is committed and clean")
    assert_entry(
        entries,
        "carry_dir/nested/added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty add carry must survive the clean pick",
    )
    assert_file_set(
        entries, ["carry_dir/nested/added.txt"], msg="only the carry remains pending"
    )
    # The intermediate directory nodes must be recreated in the fresh staged state.
    assert_entry(entries, "carry_dir", node_type="directory", action="add")
    assert_entry(entries, "carry_dir/nested", node_type="directory", action="add")
    with repo.open_file("carry_dir/nested/added.txt", "r") as f:
        assert f.read() == "dirty new content\n"


@pytest.mark.smoke
def test_cherrypick_clean_carry_delete(new_lore_repo):
    """A dirty DELETE (explicitly marked) on main survives a clean cherry-pick:
    the deleted committed file is still reported as a pending dirty delete after
    the pick lands its commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n", "victim.txt": "to be deleted\n"})

    source_rev = _commit_source_rev(
        repo, {"from_source.txt": "source content\n"}, "source adds file"
    )

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(
        pre, "victim.txt", action="delete", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(
        pre, ["victim.txt"], msg="only the dirty delete is pending pre-pick"
    )

    repo.revision_cherry_pick(source_rev, offline=True)

    assert os.path.exists(repo._fix_path("from_source.txt"))
    entries = get_status_files(repo)
    assert_absent(entries, "from_source.txt", msg="picked file is committed and clean")
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty delete carry must survive the clean pick",
    )
    assert_file_set(
        entries, ["victim.txt"], msg="only the carry remains pending after pick"
    )
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "the dirty-deleted file must stay absent from disk after the pick"
    )


@pytest.mark.smoke
def test_cherrypick_conflict_resolve_mine(new_lore_repo):
    """A conflicting cherry-pick resolved with `resolve mine` keeps the main
    (current-branch) side of conflict.txt; an unrelated dirty carry survives the
    resolve + commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "untouched.txt": "untouched\n"})

    source_rev = _commit_source_rev(
        repo, {"conflict.txt": "source side\n"}, "source edits conflict.txt"
    )

    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main side\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    with repo.open_file("untouched.txt", "w+") as f:
        f.write("locally edited\n")
    repo.dirty("untouched.txt", offline=True)

    pick_output = repo.revision_cherry_pick(source_rev, offline=True)
    assert "conflicted" in pick_output, (
        f"expected cherry-pick to surface a conflict, got:\n{pick_output}"
    )

    repo.revision_cherry_pick_resolve_mine("conflict.txt", offline=True)
    repo.commit("cherry-pick resolved mine", offline=True)

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "untouched.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="unrelated dirty carry must survive the conflicted pick",
    )
    assert_absent(
        entries, "conflict.txt", msg="conflict.txt clean after resolve + commit"
    )
    assert_file_set(entries, ["untouched.txt"], msg="only the carry remains pending")
    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "main side\n", "resolve mine keeps the current-branch side"


@pytest.mark.smoke
def test_cherrypick_conflict_resolve_theirs(new_lore_repo):
    """A conflicting cherry-pick resolved with `resolve theirs` takes the source
    (picked-revision) side of conflict.txt; an unrelated dirty carry survives the
    resolve + commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "untouched.txt": "untouched\n"})

    source_rev = _commit_source_rev(
        repo, {"conflict.txt": "source side\n"}, "source edits conflict.txt"
    )

    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main side\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    with repo.open_file("untouched.txt", "w+") as f:
        f.write("locally edited\n")
    repo.dirty("untouched.txt", offline=True)

    pick_output = repo.revision_cherry_pick(source_rev, offline=True)
    assert "conflicted" in pick_output, (
        f"expected cherry-pick to surface a conflict, got:\n{pick_output}"
    )

    repo.revision_cherry_pick_resolve_theirs("conflict.txt", offline=True)
    repo.commit("cherry-pick resolved theirs", offline=True)

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "untouched.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="unrelated dirty carry must survive the conflicted pick",
    )
    assert_absent(
        entries, "conflict.txt", msg="conflict.txt clean after resolve + commit"
    )
    assert_file_set(entries, ["untouched.txt"], msg="only the carry remains pending")
    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "source side\n", (
            "resolve theirs takes the picked-revision side"
        )


@pytest.mark.smoke
def test_cherrypick_pick_rev_that_adds_directory_with_carry(new_lore_repo):
    """Cherry-picking a revision that adds a nested directory tree commits the
    whole tree while an unrelated dirty carry survives the pick."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    source_rev = _commit_source_rev(
        repo,
        {
            "feature/src/main.txt": "main module\n",
            "feature/src/util/helper.txt": "helper module\n",
        },
        "source adds nested dir tree",
    )

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited\n")
    repo.dirty("base.txt", offline=True)

    repo.revision_cherry_pick(source_rev, offline=True)

    assert os.path.exists(repo._fix_path("feature/src/main.txt"))
    assert os.path.exists(repo._fix_path("feature/src/util/helper.txt"))
    with repo.open_file("feature/src/util/helper.txt", "r") as f:
        assert f.read() == "helper module\n"

    entries = get_status_files(repo)
    assert_absent(
        entries, "feature/src/main.txt", msg="picked tree is committed and clean"
    )
    assert_absent(
        entries, "feature/src/util/helper.txt", msg="picked tree is committed and clean"
    )
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty carry must survive the directory-adding pick",
    )
    assert_file_set(
        entries, ["base.txt"], msg="only the carry remains pending after pick"
    )


@pytest.mark.smoke
def test_cherrypick_refuses_on_staged(new_lore_repo):
    """revision cherry-pick refuses to start when an actually-staged node exists,
    raising a LoreException; the staged node survives the refusal."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    source_rev = _commit_source_rev(
        repo, {"from_source.txt": "source content\n"}, "source adds file"
    )

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged.txt", offline=True)

    from error_types import LoreException

    with pytest.raises(LoreException) as excinfo:
        repo.revision_cherry_pick(source_rev, offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"cherry-pick should refuse on actually-staged nodes, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "staged.txt",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged.txt should remain staged after the rejected pick",
    )
    assert_absent(entries, "from_source.txt", msg="nothing from the source was applied")
    assert_file_set(entries, ["staged.txt"], msg="only the pre-existing stage remains")


@pytest.mark.smoke
def test_cherrypick_abort_keeps_carry(new_lore_repo):
    """`cherry-pick abort` cancels the pick but preserves the pre-existing
    dirty-only carry: base.txt remains a dirty modify (action=keep/flagDirty)
    after the abort with its on-disk edit intact, and survives a later commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n", "conflict.txt": "base\n"})

    source_rev = _commit_source_rev(
        repo, {"conflict.txt": "source side\n"}, "source edits conflict.txt"
    )

    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main side\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited\n")
    repo.dirty("base.txt", offline=True)

    pick_output = repo.revision_cherry_pick(source_rev, offline=True)
    assert "conflicted" in pick_output, (
        f"expected cherry-pick to surface a conflict, got:\n{pick_output}"
    )

    repo.revision_cherry_pick_abort(offline=True)

    # Abort cancels the pick but keeps the unrelated dirty-only carry.
    after_abort = get_status_files(repo)
    assert_entry(
        after_abort,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="cherry-pick abort preserves the pre-existing dirty-only carry",
    )
    assert_file_set(after_abort, ["base.txt"], msg="only the carry remains after abort")
    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base locally edited\n", (
            "carry on-disk edit is intact after abort"
        )

    with repo.open_file("staged_post.txt", "w+") as f:
        f.write("post-abort\n")
    repo.stage("staged_post.txt", offline=True)
    repo.commit("post-abort commit", offline=True)

    entries = get_status_files(repo)
    assert_absent(entries, "staged_post.txt", msg="staged_post.txt clean after commit")
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="the dirty-only carry survives an unrelated commit",
    )
    assert_file_set(entries, ["base.txt"], msg="only the surviving carry remains")


# ===========================================================================
# Super-scenarios: all dirty classes together, --reset across change types,
# path-scoped scan.
# ===========================================================================


def _stage_one(repo: Lore, path: str) -> None:
    """Stage exactly one path (no scan, no dirty walk) leaving everything
    else untouched."""
    repo.stage(path, offline=True)


@pytest.mark.smoke
def test_stress_all_classes_then_commit(new_lore_repo):
    """One repo state holding all change classes at once: a staged add, a
    staged delete, a dirty-only modify, a dirty-only add (new dir), and a
    dirty-only delete. Status reports each with the correct action/flags;
    commit lands only the two staged classes while the three dirty-only
    classes survive as flagDirty=true with their actions intact."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "staged_del.txt": "delete me staged\n",
            "dirty_mod.txt": "modify me\n",
            "dirty_del.txt": "delete me dirty\n",
        },
    )

    # Staged add: brand-new file, explicitly staged.
    with repo.open_file("staged_add.txt", "w+") as f:
        f.write("staged add content\n")
    _stage_one(repo, "staged_add.txt")

    # Staged delete: remove a committed file, mark dirty, then stage it.
    repo.remove_file("staged_del.txt")
    repo.dirty("staged_del.txt", offline=True)
    _stage_one(repo, "staged_del.txt")

    # Dirty-only modify of a committed file.
    with repo.open_file("dirty_mod.txt", "w+") as f:
        f.write("dirty modified content longer\n")
    repo.dirty("dirty_mod.txt", offline=True)

    # Dirty-only add in a brand-new directory.
    repo.make_dirs("newdir")
    with repo.open_file("newdir/dirty_add.txt", "w+") as f:
        f.write("dirty add in new dir\n")
    repo.dirty("newdir/dirty_add.txt", offline=True)

    # Dirty-only delete of a committed file.
    repo.remove_file("dirty_del.txt")
    repo.dirty("dirty_del.txt", offline=True)

    pre = get_status_files(repo)
    assert_file_set(
        pre,
        [
            "staged_add.txt",
            "staged_del.txt",
            "dirty_mod.txt",
            "newdir/dirty_add.txt",
            "dirty_del.txt",
        ],
        msg="all five change classes should be reported pre-commit",
    )
    assert_entry(
        pre, "staged_add.txt", action="add", dirty=True, staged=True, node_type="file"
    )
    assert_entry(
        pre,
        "staged_del.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
    )
    assert_entry(
        pre, "dirty_mod.txt", action="keep", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        pre,
        "newdir/dirty_add.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(
        pre,
        "dirty_del.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
    )
    # The new dirty-add directory is surfaced as an add directory node.
    assert_entry(pre, "newdir", action="add", node_type="directory")

    repo.commit(offline=True)

    # After commit, only the dirty-only classes remain pending.
    post = get_status_files(repo)
    assert_file_set(
        post,
        ["dirty_mod.txt", "newdir/dirty_add.txt", "dirty_del.txt"],
        msg="staged classes committed, dirty-only classes survive",
    )
    assert_entry(post, "dirty_mod.txt", action="keep", dirty=True, staged=False)
    assert_entry(post, "newdir/dirty_add.txt", action="add", dirty=True, staged=False)
    assert_entry(post, "dirty_del.txt", action="delete", dirty=True, staged=False)
    assert_absent(
        post, "staged_add.txt", msg="staged add should be committed and clean"
    )
    assert_absent(
        post, "staged_del.txt", msg="staged delete should be committed and clean"
    )

    # Confirm the committed tree by dropping the tracked state and dumping.
    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "staged_add.txt" in dump, f"staged add must be in committed tree:\n{dump}"
    assert "staged_del.txt" not in dump, (
        f"staged delete must be gone from tree:\n{dump}"
    )
    # Dirty-only changes never touch the committed tree.
    assert "dirty_del.txt" in dump, (
        f"dirty-only delete must NOT alter committed tree:\n{dump}"
    )
    assert "dirty_add.txt" not in dump, (
        f"dirty-only add must NOT enter committed tree:\n{dump}"
    )
    # The dirty-only modify keeps its original committed content.
    assert "dirty modified content longer" not in dump, (
        f"dirty-only modify must NOT alter committed content:\n{dump}"
    )


@pytest.mark.smoke
def test_stress_all_classes_then_reset_all(new_lore_repo):
    """The same all-classes state, dropped wholesale by status(reset=True):
    the report is empty and the staged anchor is gone."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "staged_del.txt": "delete me staged\n",
            "dirty_mod.txt": "modify me\n",
            "dirty_del.txt": "delete me dirty\n",
        },
    )

    with repo.open_file("staged_add.txt", "w+") as f:
        f.write("staged add content\n")
    _stage_one(repo, "staged_add.txt")

    repo.remove_file("staged_del.txt")
    repo.dirty("staged_del.txt", offline=True)
    _stage_one(repo, "staged_del.txt")

    with repo.open_file("dirty_mod.txt", "w+") as f:
        f.write("dirty modified content longer\n")
    repo.dirty("dirty_mod.txt", offline=True)

    repo.make_dirs("newdir")
    with repo.open_file("newdir/dirty_add.txt", "w+") as f:
        f.write("dirty add in new dir\n")
    repo.dirty("newdir/dirty_add.txt", offline=True)

    repo.remove_file("dirty_del.txt")
    repo.dirty("dirty_del.txt", offline=True)

    assert has_staged_anchor(repo), (
        "anchor should exist before reset (staged nodes present)"
    )

    entries = get_status_files(repo, reset=True)
    assert entries == [], (
        f"status(reset=True) should yield empty status, got {summarize(entries)}"
    )
    assert not has_staged_anchor(repo), (
        "anchor should be cleared after status(reset=True)"
    )


@pytest.mark.smoke
def test_stress_reset_scan_redetects_delete(new_lore_repo):
    """Stage a delete, then status(reset=True, scan=True) drops the stage and
    re-detects the deletion from the filesystem. Like a scanned modify or add,
    the re-detected delete is action=delete with flagDirty=true and persists to
    a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "will be deleted\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)
    _stage_one(repo, "victim.txt")
    assert_entry(
        get_status_files(repo),
        "victim.txt",
        action="delete",
        dirty=True,
        staged=True,
        msg="staged delete before reset",
    )

    entries = get_status_files_twice(repo, reset=True, scan=True)
    assert_file_set(
        entries, ["victim.txt"], msg="reset+scan should re-detect only the deletion"
    )
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        staged=False,
        msg="reset+scan must re-detect the deletion as an unstaged delete",
    )
    assert_entry(
        entries,
        "victim.txt",
        dirty=True,
        msg="scan-detected delete is flagDirty=true (symmetric with modify/add)",
    )

    persisted = get_status_files(repo)
    assert_entry(
        persisted,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        msg="scan-detected delete persists to no-scan status like modify/add",
    )


@pytest.mark.smoke
def test_stress_reset_scan_redetects_dir_mixed(new_lore_repo):
    """A directory of mixed changes (modify + add + delete) staged, then
    status(reset=True, scan=True): the stage is dropped and the unstaged
    filesystem state is re-detected. The scan-detected modify, add and delete
    all come back as unstaged dirty changes with their respective actions."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"src/a.txt": "aaa\n", "src/b.txt": "bbb\n"})

    with repo.open_file("src/a.txt", "w+") as f:
        f.write("aaa modified longer\n")
    repo.remove_file("src/b.txt")
    with repo.open_file("src/c.txt", "w+") as f:
        f.write("ccc new\n")
    repo.stage(scan=True, offline=True)

    staged = get_status_files(repo)
    assert_file_set(
        staged,
        ["src/a.txt", "src/b.txt", "src/c.txt"],
        msg="all three staged before reset",
    )
    assert_entry(staged, "src/a.txt", staged=True)
    assert_entry(staged, "src/c.txt", staged=True)

    entries = get_status_files_twice(repo, reset=True, scan=True)
    assert_file_set(
        entries,
        ["src/a.txt", "src/b.txt", "src/c.txt"],
        msg="reset+scan re-detects the unstaged filesystem state of the directory",
    )
    assert_entry(
        entries,
        "src/a.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="scan re-detects the modify as an unstaged dirty modify",
    )
    assert_entry(
        entries,
        "src/c.txt",
        action="add",
        dirty=True,
        staged=False,
        msg="scan re-detects the add as an unstaged dirty add",
    )
    assert_entry(
        entries,
        "src/b.txt",
        action="delete",
        staged=False,
        msg="scan re-detects the deletion as an unstaged delete",
    )
    assert_entry(
        entries,
        "src/b.txt",
        dirty=True,
        msg="scan-detected delete is flagDirty=true like the sibling modify/add",
    )


@pytest.mark.smoke
def test_stress_pathscoped_status_mixed(new_lore_repo):
    """Changes spread across three subdirectories (subA modify, subB add,
    subC delete dirty-marked); a path-scoped scan over overlapping path args
    [subA, subB, subC, .] reports exactly those changes and is idempotent —
    no drops or duplicates across the overlapping path arguments."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "subA/mod.txt": "subA original\n",
            "subC/del.txt": "subC to delete\n",
            "other/keep.txt": "untouched\n",
        },
    )

    with repo.open_file("subA/mod.txt", "w+") as f:
        f.write("subA modified content longer\n")
    repo.dirty("subA/mod.txt", offline=True)

    repo.make_dirs("subB")
    with repo.open_file("subB/add.txt", "w+") as f:
        f.write("subB added\n")
    repo.dirty("subB/add.txt", offline=True)

    repo.remove_file("subC/del.txt")
    repo.dirty("subC/del.txt", offline=True)

    entries = get_status_files_twice(
        repo, path=["subA", "subB", "subC", "."], scan=True
    )
    assert_file_set(
        entries,
        ["subA/mod.txt", "subB/add.txt", "subC/del.txt"],
        msg="path-scoped scan reports exactly the three changed files, no dups across overlap",
    )
    assert_entry(entries, "subA/mod.txt", action="keep", dirty=True, staged=False)
    assert_entry(entries, "subB/add.txt", action="add", dirty=True, staged=False)
    assert_entry(entries, "subC/del.txt", action="delete", dirty=True, staged=False)
    assert_absent(entries, "other/keep.txt", msg="untouched file must not appear")


@pytest.mark.smoke
def test_stress_long_chain(new_lore_repo):
    """A long lifecycle chain validating the full reported set at each step:
    dirty -> stage -> unstage -> commit -> modify -> scan -> reset."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v0 original\n", "side.txt": "side original\n"})

    # 1. dirty: a modify is tracked as a dirty-only modify.
    with repo.open_file("file.txt", "w+") as f:
        f.write("v1 modified longer\n")
    repo.dirty("file.txt", offline=True)
    s = get_status_files(repo)
    assert_file_set(s, ["file.txt"], msg="after dirty: only file.txt pending")
    assert_entry(s, "file.txt", action="keep", dirty=True, staged=False)

    # 2. stage: dirty preserved, now also staged.
    _stage_one(repo, "file.txt")
    s = get_status_files(repo)
    assert_file_set(s, ["file.txt"], msg="after stage: still only file.txt")
    assert_entry(s, "file.txt", action="keep", dirty=True, staged=True)

    # 3. unstage: staged cleared, dirty survives (file still differs).
    repo.unstage(offline=True)
    s = get_status_files(repo)
    assert_file_set(s, ["file.txt"], msg="after unstage: still only file.txt")
    assert_entry(s, "file.txt", action="keep", dirty=True, staged=False)

    # 4. commit: re-stage then commit; file.txt becomes clean, anchor gone.
    _stage_one(repo, "file.txt")
    repo.commit(offline=True)
    s = get_status_files(repo)
    assert_file_set(s, [], msg="after commit: working tree clean")
    assert not has_staged_anchor(repo), "anchor should be gone after clean commit"
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v1 modified longer\n"

    # 5. modify: edit a different file on disk without dirtying it; a no-scan
    # status is blind to it.
    with repo.open_file("side.txt", "w+") as f:
        f.write("side modified by scan detection\n")
    s = get_status_files(repo)
    assert_file_set(s, [], msg="no-scan status must not see an un-dirtied on-disk edit")

    # 6. scan: detects and persists side.txt as a dirty modify.
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["side.txt"], msg="scan detects side.txt")
    assert_entry(scanned, "side.txt", action="keep", dirty=True, staged=False)
    persisted = get_status_files(repo)
    assert_entry(
        persisted,
        "side.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="a scanned modification persists to no-scan status",
    )

    # 7. reset: status(reset=True) clears all tracking -> empty + no anchor.
    entries = get_status_files(repo, reset=True)
    assert entries == [], f"after reset: empty status, got {summarize(entries)}"
    assert not has_staged_anchor(repo), "anchor cleared after reset"


# ===========================================================================
# Deep cross-operation CHAIN tests: thread a tracked change through MANY
# operations, validating the FULL reported status (assert_file_set +
# per-path assert_entry) AND on-disk content/existence at EACH step. These
# are genuinely multi-stage (>=4 operations), not pairwise.
# ===========================================================================


def _chain_read(repo: Lore, path: str) -> str:
    """Read the full on-disk content of a tracked path (offline helper)."""
    with repo.open_file(path, "r") as f:
        return f.read()


def _chain_exists(repo: Lore, path: str) -> bool:
    """True iff path exists on disk under the repo working tree."""
    return os.path.exists(repo._fix_path(path))


@pytest.mark.smoke
def test_chain_modify_scan_switch_reset_status(new_lore_repo):
    """CENTERPIECE chain: commit base -> branch_create(other) -> switch(main)
    -> modify file.txt on disk -> status --scan (keep/dirty, persists) ->
    switch(other) [dirty modify CARRIES] -> reset(file.txt) -> clean.

    Invariant: a scan-detected dirty modify survives a same-revision branch
    switch with identical status + on-disk content, and a file reset then
    restores committed content and leaves both no-scan and --scan status clean.
    Full status set + on-disk content/existence validated at every step."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v0 content", "anchor.txt": "anchor"})

    # other branches from main at the SAME commit; switch back to main.
    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    # Step 1: modify file.txt on disk (no dirty mark yet).
    with repo.open_file("file.txt", "w+") as f:
        f.write("v0 content modified longer")
    blind = get_status_files(repo)
    assert_file_set(blind, [], msg="no-scan status is blind to the un-dirtied edit")

    # Step 2: status --scan detects file.txt as keep/dirty (idempotent).
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["file.txt"], msg="scan detects only the modified file")
    assert_entry(
        scanned, "file.txt", action="keep", dirty=True, staged=False, node_type="file"
    )
    assert_absent(scanned, "anchor.txt", msg="untouched committed file stays clean")

    # Step 3: the scanned modify PERSISTS into a later no-scan status.
    persisted = get_status_files(repo)
    assert_file_set(
        persisted, ["file.txt"], msg="scanned modify persists to no-scan status"
    )
    assert_entry(persisted, "file.txt", action="keep", dirty=True, staged=False)
    assert _chain_read(repo, "file.txt") == "v0 content modified longer"

    # Step 4: switch to other (same revision) — the dirty modify CARRIES.
    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    assert_file_set(
        carried, ["file.txt"], msg="dirty modify carries across same-revision switch"
    )
    assert_entry(
        carried,
        "file.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="carried modify keeps keep/dirty after the switch",
    )
    assert_absent(carried, "anchor.txt", msg="untouched file stays clean on other")
    assert _chain_read(repo, "file.txt") == "v0 content modified longer", (
        "on-disk content carries the modification across the switch"
    )

    # Step 5: reset(file.txt) on other restores content + clears tracking.
    repo.reset("file.txt", offline=True)
    assert _chain_read(repo, "file.txt") == "v0 content", (
        "file reset restores the committed content"
    )
    no_scan = get_status_files(repo)
    assert_file_set(no_scan, [], msg="no-scan status clean after reset")
    rescanned = get_status_files_twice(repo, scan=True)
    assert_file_set(rescanned, [], msg="--scan status clean after reset")
    assert not has_staged_anchor(repo), "anchor released once the tree is clean"


@pytest.mark.smoke
def test_chain_modify_scan_switch_roundtrip_reset(new_lore_repo):
    """Chain: commit base -> branch_create(other) -> switch(main) -> modify ->
    scan (keep/dirty) -> switch(other) -> switch back(main): the dirty set is
    IDENTICAL across the round trip -> reset -> clean.

    Invariant: a carried dirty modify is byte-for-byte stable across a full
    other->main round trip of same-revision switches (full set compared at each
    hop), and a final reset restores committed content + clears all tracking."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "round v0", "keep.txt": "keep"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("round modified longer")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["file.txt"], msg="scan detects the modify on main")
    assert_entry(scanned, "file.txt", action="keep", dirty=True, staged=False)
    main_set = summarize(get_status_files(repo))

    # Hop to other: same dirty set, same content.
    repo.branch_switch("other", offline=True)
    other_entries = get_status_files(repo)
    assert_file_set(other_entries, ["file.txt"], msg="dirty modify carries onto other")
    assert_entry(other_entries, "file.txt", action="keep", dirty=True, staged=False)
    assert _chain_read(repo, "file.txt") == "round modified longer"
    assert summarize(other_entries) == main_set, (
        f"status set must match across the switch.\nmain={main_set}\nother={summarize(other_entries)}"
    )

    # Hop back to main: still identical.
    repo.branch_switch("main", offline=True)
    back_entries = get_status_files(repo)
    assert_file_set(
        back_entries, ["file.txt"], msg="dirty modify carries back to main unchanged"
    )
    assert_entry(back_entries, "file.txt", action="keep", dirty=True, staged=False)
    assert _chain_read(repo, "file.txt") == "round modified longer", (
        "on-disk content is stable across the full round trip"
    )
    assert summarize(back_entries) == main_set, (
        f"round-trip status must match the original.\nbefore={main_set}\nafter={summarize(back_entries)}"
    )

    # Reset closes the chain to a clean tree.
    repo.reset("file.txt", offline=True)
    assert _chain_read(repo, "file.txt") == "round v0", (
        "reset restores committed content"
    )
    assert_file_set(get_status_files(repo), [], msg="no-scan clean after reset")
    assert_file_set(
        get_status_files_twice(repo, scan=True), [], msg="--scan clean after reset"
    )
    assert not has_staged_anchor(repo), "anchor released once clean"


@pytest.mark.smoke
def test_chain_add_scan_switch_stage_commit_dump(new_lore_repo):
    """Chain: commit base -> branch_create(other) -> switch(main) -> add new
    file on disk -> scan (add/dirty, persists) -> switch(other) [add carries]
    -> stage + commit on other -> status clean + repository_dump contains the
    added file.

    Invariant: a scan-detected dirty ADD carries across a same-revision switch
    and can be committed on the target branch, after which status is fully clean
    and the committed tree (dump) contains the new file."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    # Add a brand-new file and detect it via scan.
    with repo.open_file("fresh.txt", "w+") as f:
        f.write("fresh content")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["fresh.txt"], msg="scan detects the new file")
    assert_entry(
        scanned, "fresh.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    persisted = get_status_files(repo)
    assert_entry(
        persisted,
        "fresh.txt",
        action="add",
        dirty=True,
        staged=False,
        msg="scanned add persists to no-scan status",
    )

    # Carry the add across a same-revision switch to other.
    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    assert_file_set(carried, ["fresh.txt"], msg="dirty add carries onto other")
    assert_entry(carried, "fresh.txt", action="add", dirty=True, staged=False)
    assert _chain_read(repo, "fresh.txt") == "fresh content", (
        "added content carries on disk"
    )

    # Stage + commit on other.
    repo.stage("fresh.txt", offline=True)
    staged = get_status_files(repo)
    assert_entry(
        staged,
        "fresh.txt",
        action="add",
        dirty=True,
        staged=True,
        msg="add is staged on other before commit",
    )
    repo.commit("commit fresh on other", offline=True)

    # Status fully clean, anchor gone, and the dump carries the committed file.
    after = get_status_files(repo)
    assert_file_set(after, [], msg="no-scan status clean after committing the add")
    assert_file_set(
        get_status_files_twice(repo, scan=True), [], msg="--scan clean after commit"
    )
    assert not has_staged_anchor(repo), "anchor cleared after a clean commit"

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "fresh.txt" in dump, f"committed add must appear in the sealed tree:\n{dump}"


@pytest.mark.smoke
def test_chain_full_lifecycle_modify_commit_modify_switch_reset(new_lore_repo):
    """Full lifecycle chain: modify file.txt -> scan -> stage -> commit
    (clean, anchor cleared) -> modify file.txt AGAIN -> scan ->
    branch_create(other) -> switch(other) [second modify carries] -> reset ->
    clean.

    Invariant: after a scan+stage+commit clears the first edit, a SECOND
    scan-detected edit carries across a freshly-created same-revision branch and
    a reset restores the committed (v1) content, leaving status clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v0 lifecycle", "anchor.txt": "anchor"})

    # First edit: scan + stage + commit -> clean, anchor gone.
    with repo.open_file("file.txt", "w+") as f:
        f.write("v1 lifecycle longer")
    s1 = get_status_files_twice(repo, scan=True)
    assert_entry(s1, "file.txt", action="keep", dirty=True, staged=False)
    repo.stage("file.txt", offline=True)
    assert_entry(
        get_status_files(repo),
        "file.txt",
        action="keep",
        dirty=True,
        staged=True,
        msg="staged before the first commit",
    )
    repo.commit("commit v1", offline=True)
    assert_file_set(get_status_files(repo), [], msg="clean after committing v1")
    assert not has_staged_anchor(repo), "anchor gone after the clean commit"
    assert _chain_read(repo, "file.txt") == "v1 lifecycle longer", (
        "v1 committed on disk"
    )

    # Second edit detected by scan and persisted.
    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 lifecycle even longer")
    s2 = get_status_files_twice(repo, scan=True)
    assert_file_set(s2, ["file.txt"], msg="scan detects the second edit")
    assert_entry(s2, "file.txt", action="keep", dirty=True, staged=False)

    # Create a same-revision branch from the v1 head and switch onto it.
    repo.branch_create("other", offline=True)
    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    assert_file_set(carried, ["file.txt"], msg="second edit carries onto other")
    assert_entry(carried, "file.txt", action="keep", dirty=True, staged=False)
    assert _chain_read(repo, "file.txt") == "v2 lifecycle even longer", (
        "v2 carries on disk"
    )

    # Reset restores the committed (v1) content and clears tracking.
    repo.reset("file.txt", offline=True)
    assert _chain_read(repo, "file.txt") == "v1 lifecycle longer", (
        "reset restores committed v1, not the original base v0"
    )
    assert_file_set(get_status_files(repo), [], msg="no-scan clean after reset")
    assert_file_set(
        get_status_files_twice(repo, scan=True), [], msg="--scan clean after reset"
    )
    assert not has_staged_anchor(repo), "anchor released once clean"


@pytest.mark.smoke
def test_chain_two_modifies_scan_switch_reset_one_then_other(new_lore_repo):
    """Chain: modify TWO files -> scan (both keep/dirty) -> switch(other)
    [both carry] -> reset ONE -> the OTHER is still dirty (full set checked) ->
    reset the OTHER -> clean.

    Invariant: a per-path reset clears exactly one tracked dirty modify (content
    restored) while every other carried dirty path keeps its status + on-disk
    content; resetting the last one leaves the tree clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a.txt": "a v0", "b.txt": "b v0", "untouched.txt": "stay"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("a.txt", "w+") as f:
        f.write("a modified longer")
    with repo.open_file("b.txt", "w+") as f:
        f.write("b modified longer")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["a.txt", "b.txt"], msg="scan detects both modifies")
    assert_entry(scanned, "a.txt", action="keep", dirty=True, staged=False)
    assert_entry(scanned, "b.txt", action="keep", dirty=True, staged=False)

    # Carry both across the switch.
    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    assert_file_set(
        carried, ["a.txt", "b.txt"], msg="both dirty modifies carry onto other"
    )
    assert_entry(carried, "a.txt", action="keep", dirty=True, staged=False)
    assert_entry(carried, "b.txt", action="keep", dirty=True, staged=False)

    # Reset a.txt only: b.txt must remain dirty with its content intact.
    repo.reset("a.txt", offline=True)
    assert _chain_read(repo, "a.txt") == "a v0", (
        "reset a.txt restores its committed content"
    )
    assert _chain_read(repo, "b.txt") == "b modified longer", (
        "b.txt keeps its dirty content"
    )
    mid = get_status_files(repo)
    assert_file_set(
        mid, ["b.txt"], msg="only b.txt remains dirty after resetting a.txt"
    )
    assert_entry(
        mid,
        "b.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="the un-reset sibling is still a dirty modify",
    )
    assert_absent(mid, "a.txt", msg="the reset file is cleared from status")
    assert has_staged_anchor(repo), "anchor persists while one dirty path remains"

    # Reset b.txt: the tree is now clean.
    repo.reset("b.txt", offline=True)
    assert _chain_read(repo, "b.txt") == "b v0", (
        "reset b.txt restores its committed content"
    )
    assert_file_set(
        get_status_files(repo), [], msg="no-scan clean after resetting both"
    )
    assert_file_set(
        get_status_files_twice(repo, scan=True),
        [],
        msg="--scan clean after both resets",
    )
    assert not has_staged_anchor(repo), "anchor released once both are reset"


@pytest.mark.smoke
def test_chain_dirty_base_switch_merge_feature_reset(new_lore_repo):
    """Chain (switch x merge x carry x reset): dirty-modify base.txt on main +
    a feature branch that ADDED feat.txt & committed -> switch(main) ->
    branch_merge(feature) [dirty carry survives the auto-commit] -> status
    (base.txt still dirty, feat.txt committed-clean) -> reset(base.txt) -> clean.

    Invariant: an unrelated dirty modify carried into a clean merge survives the
    merge auto-commit as keep/dirty while the feature's added file lands
    committed-clean; resetting the carry then restores committed content and
    leaves a fully clean tree."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base v0"})

    # feature branch adds feat.txt and commits.
    repo.branch_create("feature", offline=True)
    with repo.open_file("feat.txt", "w+") as f:
        f.write("feature added")
    repo.stage(scan=True, offline=True)
    repo.commit("feature adds feat.txt", offline=True)

    # Back on main, dirty-modify base.txt (an unrelated path).
    repo.branch_switch("main", offline=True)
    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited longer")
    repo.dirty("base.txt", offline=True)
    pre = get_status_files(repo)
    assert_file_set(pre, ["base.txt"], msg="only the dirty carry pending before merge")
    assert_entry(pre, "base.txt", action="keep", dirty=True, staged=False)

    # Merge feature into main: clean, auto-commits, carry survives.
    repo.branch_merge("feature", offline=True)
    assert _chain_exists(repo, "feat.txt"), "feature add lands on disk after the merge"
    assert _chain_read(repo, "feat.txt") == "feature added", (
        "merged feat.txt content on disk"
    )
    assert _chain_read(repo, "base.txt") == "base locally edited longer", (
        "the dirty carry keeps its local content through the merge"
    )

    post = get_status_files(repo)
    assert_file_set(
        post, ["base.txt"], msg="base still dirty, feat.txt committed-clean"
    )
    assert_entry(
        post,
        "base.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty modify carry survives the clean merge",
    )
    assert_absent(
        post, "feat.txt", msg="feature add is committed and clean after merge"
    )

    # Reset the carry -> clean.
    repo.reset("base.txt", offline=True)
    assert _chain_read(repo, "base.txt") == "base v0", (
        "reset restores committed base content"
    )
    assert_file_set(
        get_status_files(repo), [], msg="no-scan clean after resetting the carry"
    )
    assert_file_set(
        get_status_files_twice(repo, scan=True), [], msg="--scan clean after reset"
    )
    assert not has_staged_anchor(repo), "anchor released once the carry is reset"


@pytest.mark.smoke
def test_chain_modify_scan_sync_back_forward_reset(new_lore_repo):
    """Chain (sync with pending dirty): commit v1 -> commit v2 (side.txt
    edited; file.txt untouched by v2) -> dirty-modify file.txt to a local edit
    -> scan -> sync back to v1 [carry survives onto the v1 base] -> sync forward
    to v2 [carry survives] -> status (still dirty) -> reset -> clean.

    Invariant: sync rebases a genuine pending dirty modify on a path OUTSIDE the
    sync delta onto the target revision; the dirty content is preserved across
    both sync hops while the non-dirty file (side.txt, which IS in the delta)
    tracks the synced revision. A final reset restores file.txt's committed v1
    content (its committed content is identical at v1 and v2 since v2 left it
    untouched)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1 file content", "side.txt": "v1 side"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    # v2 edits ONLY side.txt; file.txt stays at its v1 committed content.
    with repo.open_file("side.txt", "w+") as f:
        f.write("v2 side longer")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 edits side.txt", offline=True)
    assert _chain_read(repo, "file.txt") == "v1 file content", (
        "file.txt unchanged by v2"
    )
    assert _chain_read(repo, "side.txt") == "v2 side longer", "side.txt at v2"

    # Genuine pending dirty modify of file.txt (outside the v1<->v2 delta),
    # detected and persisted via scan.
    with repo.open_file("file.txt", "w+") as f:
        f.write("local pending edit longer")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["file.txt"], msg="scan detects the pending edit")
    assert_entry(scanned, "file.txt", action="keep", dirty=True, staged=False)

    # Sync BACK to v1: file.txt is outside the delta so its dirty edit is carried
    # untouched; side.txt (in the delta) reverts to its v1 content.
    repo.sync(rev_v1, offline=True)
    assert _chain_read(repo, "file.txt") == "local pending edit longer", (
        "dirty modify outside the delta is carried across sync back, not lost"
    )
    assert _chain_read(repo, "side.txt") == "v1 side", (
        "non-dirty file follows the synced revision"
    )
    at_v1 = get_status_files_twice(repo)
    assert_file_set(
        at_v1, ["file.txt"], msg="only the carried dirty path is pending at v1"
    )
    assert_entry(at_v1, "file.txt", action="keep", dirty=True, staged=False)

    # Sync FORWARD to v2: the carry still survives; side.txt returns to v2.
    repo.sync(offline=True)
    assert _chain_read(repo, "file.txt") == "local pending edit longer", (
        "dirty modify is carried across sync forward too"
    )
    assert _chain_read(repo, "side.txt") == "v2 side longer", (
        "side.txt back at v2 after forward sync"
    )
    at_v2 = get_status_files_twice(repo)
    assert_file_set(at_v2, ["file.txt"], msg="carry still pending after sync forward")
    assert_entry(at_v2, "file.txt", action="keep", dirty=True, staged=False)

    # Reset restores file.txt's committed content (identical at v1/v2) and clears
    # tracking; side.txt at the current (v2) revision stays clean.
    repo.reset("file.txt", offline=True)
    assert _chain_read(repo, "file.txt") == "v1 file content", (
        "reset restores file.txt's committed content"
    )
    assert_file_set(get_status_files(repo), [], msg="no-scan clean after reset")
    assert_file_set(
        get_status_files_twice(repo, scan=True), [], msg="--scan clean after reset"
    )
    assert not has_staged_anchor(repo), "anchor released once clean"


@pytest.mark.smoke
def test_chain_two_adds_scan_switch_commit_one_then_other(new_lore_repo):
    """Chain: add TWO new files -> scan (both add/dirty, persist) -> switch(other)
    [both carry] -> stage + commit ONE add -> the other add survives dirty ->
    stage + commit the OTHER -> status fully clean + repository_dump contains
    BOTH added files.

    Invariant: multiple scan-detected dirty ADDs carry across a same-revision
    switch and can be committed one at a time on the target branch — each commit
    lands exactly its file while the un-committed add keeps its add/dirty status
    and on-disk content, and after the last commit the tree is clean with both
    files in the sealed tree."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    # Two brand-new files detected by scan.
    with repo.open_file("first.txt", "w+") as f:
        f.write("first add")
    with repo.open_file("second.txt", "w+") as f:
        f.write("second add")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["first.txt", "second.txt"], msg="scan detects both adds")
    assert_entry(
        scanned, "first.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        scanned, "second.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    persisted = get_status_files(repo)
    assert_file_set(
        persisted, ["first.txt", "second.txt"], msg="both adds persist to no-scan"
    )

    # Carry both adds across the switch.
    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    assert_file_set(
        carried, ["first.txt", "second.txt"], msg="both adds carry onto other"
    )
    assert_entry(carried, "first.txt", action="add", dirty=True, staged=False)
    assert_entry(carried, "second.txt", action="add", dirty=True, staged=False)
    assert _chain_read(repo, "first.txt") == "first add", (
        "first add content carries on disk"
    )
    assert _chain_read(repo, "second.txt") == "second add", (
        "second add content carries on disk"
    )

    # Commit only first.txt; second.txt stays pending as a dirty add.
    repo.stage("first.txt", offline=True)
    repo.commit("commit first add on other", offline=True)
    mid = get_status_files(repo)
    assert_file_set(
        mid, ["second.txt"], msg="only the un-committed add remains pending"
    )
    assert_entry(
        mid,
        "second.txt",
        action="add",
        dirty=True,
        staged=False,
        msg="the un-committed add keeps add/dirty status",
    )
    assert_absent(mid, "first.txt", msg="committed add is clean")
    assert _chain_read(repo, "second.txt") == "second add", (
        "second add content still on disk"
    )
    assert has_staged_anchor(repo), "anchor persists while the second dirty add remains"

    # Commit second.txt too; the tree is now fully clean.
    repo.stage("second.txt", offline=True)
    repo.commit("commit second add on other", offline=True)
    after = get_status_files(repo)
    assert_file_set(after, [], msg="no-scan status clean after committing both adds")
    assert_file_set(
        get_status_files_twice(repo, scan=True),
        [],
        msg="--scan clean after both commits",
    )
    assert not has_staged_anchor(repo), "anchor cleared once both adds are committed"

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "first.txt" in dump, (
        f"first committed add must be in the sealed tree:\n{dump}"
    )
    assert "second.txt" in dump, (
        f"second committed add must be in the sealed tree:\n{dump}"
    )


@pytest.mark.smoke
def test_chain_modify_stage_unstage_scan_switch_reset(new_lore_repo):
    """Chain: modify file.txt -> dirty -> stage -> unstage (dirty survives) ->
    status --scan (confirms keep/dirty, idempotent) -> switch(other) [carry] ->
    reset -> clean.

    Invariant: unstage clears only the staged flag (the dirty modify and its
    on-disk content survive); a scan over the still-dirty modify is idempotent;
    the dirty modify then carries across a same-revision switch and resets to a
    clean tree. flagDirty/flagStaged orthogonality threaded through the chain."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v0 stagechain", "keep.txt": "keep"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("edited stagechain longer")
    repo.dirty("file.txt", offline=True)
    assert_entry(
        get_status_files(repo),
        "file.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="dirty-only modify before staging",
    )

    # Stage: dirty preserved, staged set (orthogonal flags).
    repo.stage("file.txt", offline=True)
    assert_entry(
        get_status_files(repo),
        "file.txt",
        action="keep",
        dirty=True,
        staged=True,
        msg="default stage keeps dirty and sets staged",
    )

    # Unstage: staged cleared, dirty survives.
    repo.unstage(offline=True)
    unstaged = get_status_files(repo)
    assert_file_set(unstaged, ["file.txt"], msg="file.txt still pending after unstage")
    assert_entry(
        unstaged,
        "file.txt",
        action="keep",
        dirty=True,
        staged=False,
        msg="unstage clears staged but keeps the dirty modify",
    )

    # A scan over the still-dirty modify is idempotent and keeps keep/dirty.
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["file.txt"], msg="scan keeps the dirty modify")
    assert_entry(scanned, "file.txt", action="keep", dirty=True, staged=False)

    # Carry across the switch, then reset.
    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    assert_file_set(
        carried, ["file.txt"], msg="dirty modify carries onto other after unstage"
    )
    assert_entry(carried, "file.txt", action="keep", dirty=True, staged=False)
    assert _chain_read(repo, "file.txt") == "edited stagechain longer", (
        "edited content carries"
    )

    repo.reset("file.txt", offline=True)
    assert _chain_read(repo, "file.txt") == "v0 stagechain", (
        "reset restores committed content"
    )
    assert_file_set(get_status_files(repo), [], msg="no-scan clean after reset")
    assert_file_set(
        get_status_files_twice(repo, scan=True), [], msg="--scan clean after reset"
    )
    assert not has_staged_anchor(repo), "anchor released once clean"


@pytest.mark.smoke
def test_chain_add_scan_switch_reset_keeps_untracked(new_lore_repo):
    """Chain (add -> scan -> switch -> reset): add new.txt -> scan (add/dirty,
    persists) -> switch(other) [add carries] -> plain reset(new.txt).

    Invariant: a plain (non --purge) reset of a dirty ADD — even one carried
    across a same-revision switch — keeps the untracked file on disk and merely
    clears its tracking; only `reset --purge` deletes untracked files. A later
    --scan then rediscovers the surviving file.
    """
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    # Scan-detected add, persisted.
    with repo.open_file("new.txt", "w+") as f:
        f.write("brand new content")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["new.txt"], msg="scan detects the add")
    assert_entry(
        scanned, "new.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        get_status_files(repo),
        "new.txt",
        action="add",
        dirty=True,
        staged=False,
        msg="scanned add persists to no-scan status",
    )

    # Carry the add across the switch.
    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    assert_file_set(carried, ["new.txt"], msg="dirty add carries onto other")
    assert_entry(carried, "new.txt", action="add", dirty=True, staged=False)
    assert _chain_read(repo, "new.txt") == "brand new content", (
        "added content carries on disk"
    )

    # Plain reset on other.
    repo.reset("new.txt", offline=True)

    assert _chain_exists(repo, "new.txt"), (
        "plain reset must keep the untracked added file on disk; only --purge removes it"
    )
    assert _chain_read(repo, "new.txt") == "brand new content", (
        "untracked content must be intact after the plain reset"
    )
    no_scan = get_status_files(repo)
    assert_absent(
        no_scan, "new.txt", msg="reset must clear the dirty-add; no-scan status clean"
    )
    assert_file_set(no_scan, [], msg="no files should remain tracked after the reset")

    # The surviving untracked file is rediscovered by --scan (scan's job).
    redetect = get_status_files_twice(repo, scan=True)
    assert_entry(
        redetect,
        "new.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="--scan rediscovers the surviving untracked add",
    )


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_chain_move_scan_switch_status(new_lore_repo):
    """Chain (move -> scan -> switch -> reset): dirty_move old.txt -> new.txt
    -> status (move/fromPath) -> status --scan -> switch(other) -> status ->
    reset(new.txt) clears the tracking.

    Invariant: a dirty MOVE keeps action=move with fromPath=source through a
    no-scan status, through a --scan (the rename is still on disk), AND across a
    same-revision branch switch — the rename provenance is preserved as a move.
    The on-disk rename (source gone, destination present with the moved content)
    is intact at every step.
    """
    repo: Lore = new_lore_repo()
    commit_base(repo, {"old.txt": "movable content", "anchor.txt": "anchor"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    # Rename on disk + dirty_move.
    os.rename(repo._fix_path("old.txt"), repo._fix_path("new.txt"))
    repo.dirty_move("old.txt", "new.txt", offline=True)

    # no-scan status reports the move with its provenance.
    pre = get_status_files(repo)
    assert_file_set(pre, ["new.txt"], msg="no-scan reports only the move destination")
    assert_entry(
        pre,
        "new.txt",
        action="move",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="old.txt",
        msg="no-scan reports the dirty move with provenance",
    )
    assert_absent(pre, "old.txt", msg="move source must not appear")
    assert not _chain_exists(repo, "old.txt"), "move source gone on disk"
    assert _chain_read(repo, "new.txt") == "movable content", "moved content on disk"

    # --scan must preserve the move provenance.
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["new.txt"], msg="scan reports only the move destination")
    assert_entry(
        scanned,
        "new.txt",
        action="move",
        dirty=True,
        node_type="file",
        from_path="old.txt",
        msg="--scan must preserve the dirty-move provenance",
    )
    assert_absent(scanned, "old.txt", msg="move source must not reappear after scan")

    # Switch must carry the move provenance.
    repo.branch_switch("other", offline=True)
    assert not _chain_exists(repo, "old.txt"), "move source still gone after switch"
    assert _chain_read(repo, "new.txt") == "movable content", (
        "moved content intact after switch"
    )
    carried = get_status_files(repo)
    assert_file_set(
        carried, ["new.txt"], msg="switch carries only the move destination"
    )
    assert_entry(
        carried,
        "new.txt",
        action="move",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="old.txt",
        msg="dirty move provenance must survive a same-revision switch, not downgrade to add",
    )

    # Reset closes the chain: the destination's tracking is cleared.
    repo.reset("new.txt", offline=True)
    after = get_status_files(repo)
    assert_file_set(after, [], msg="no-scan clean after resetting the move destination")


@pytest.mark.smoke
def test_chain_modify_delete_scan_switch_stage_commit_dump(new_lore_repo):
    """Chain: commit base -> branch_create(other) -> switch(main) -> modify one
    file + delete another on disk -> scan (keep/dirty + delete/dirty, persists)
    -> switch(other) [both carry] -> default stage + commit on other -> status
    clean, anchor cleared, and the committed tree (dump) holds the modified
    content with the deleted file gone.

    A dirty MODIFY and a dirty DELETE carried across a same-revision switch can
    be staged and committed on the target branch, leaving status fully clean."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"mod.txt": "mod v0\n", "del.txt": "del v0\n", "stay.txt": "stay\n"}
    )

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("mod.txt", "w+") as f:
        f.write("mod locally edited longer\n")
    repo.remove_file("del.txt")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned, ["mod.txt", "del.txt"], msg="scan detects the modify and the delete"
    )
    assert_entry(
        scanned, "mod.txt", action="keep", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        scanned, "del.txt", action="delete", dirty=True, staged=False, node_type="file"
    )

    # Carry the dirty modify + delete across a same-revision switch to other.
    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    assert_file_set(
        carried, ["mod.txt", "del.txt"], msg="dirty modify+delete carry onto other"
    )
    assert_entry(carried, "mod.txt", action="keep", dirty=True, staged=False)
    assert_entry(carried, "del.txt", action="delete", dirty=True, staged=False)
    assert _chain_read(repo, "mod.txt") == "mod locally edited longer\n", (
        "modify carries on disk"
    )
    assert not _chain_exists(repo, "del.txt"), "delete carries on disk"

    # Default stage picks up the carried dirty marks; commit on other.
    repo.stage(offline=True)
    staged = get_status_files(repo)
    assert_entry(
        staged,
        "mod.txt",
        action="keep",
        dirty=True,
        staged=True,
        msg="carried modify staged on other",
    )
    assert_entry(
        staged,
        "del.txt",
        action="delete",
        dirty=True,
        staged=True,
        msg="carried delete staged on other",
    )
    repo.commit("commit carried modify+delete on other", offline=True)

    # Status fully clean, anchor gone, and the dump reflects the committed change.
    after = get_status_files(repo)
    assert_file_set(after, [], msg="no-scan status clean after committing the carry")
    assert_file_set(
        get_status_files_twice(repo, scan=True), [], msg="--scan clean after commit"
    )
    assert not has_staged_anchor(repo), "anchor cleared after a clean commit"

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "mod.txt" in dump, (
        f"committed modify must appear in the sealed tree:\n{dump}"
    )
    assert "del.txt" not in dump, (
        f"committed delete must drop the file from the tree:\n{dump}"
    )
    assert _chain_read(repo, "mod.txt") == "mod locally edited longer\n", (
        "committed content on disk"
    )


@pytest.mark.smoke
def test_chain_branchreset_dirty_modify_stage_commit_dump(new_lore_repo):
    """Chain: commit v1 + a v2 tip -> dirty-modify a file on disk -> branch
    reset to v1 [the dirty modify carries; the tip file realizes v1] -> default
    stage + commit on the reset tip -> status clean, anchor cleared, and the
    dump holds the carried modification committed on top of v1.

    A dirty MODIFY survives `branch reset` (not only adds/deletes) and can be
    staged and committed on the reset tip."""
    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(repo, {"base.txt": "base v1\n", "mod.txt": "mod v1\n"})

    with repo.open_file("mod.txt", "w+") as f:
        f.write("mod locally edited longer\n")
    repo.dirty("mod.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "mod.txt", action="keep", dirty=True, staged=False)

    repo.branch_reset(rev_v1, offline=True)

    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base v1\n", "branch reset realizes v1 of the tip file"
    carried = get_status_files(repo)
    assert_file_set(
        carried, ["mod.txt"], msg="only the carried dirty modify is pending after reset"
    )
    assert_entry(
        carried,
        "mod.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty modify must survive branch reset",
    )
    assert _chain_read(repo, "mod.txt") == "mod locally edited longer\n", (
        "modify carries on disk"
    )

    # Default stage + commit the carried modify on the reset tip.
    repo.stage(offline=True)
    assert_entry(
        get_status_files(repo),
        "mod.txt",
        action="keep",
        dirty=True,
        staged=True,
        msg="carried modify staged on the reset tip",
    )
    repo.commit("commit carried modify after branch reset", offline=True)

    after = get_status_files(repo)
    assert_file_set(after, [], msg="status clean after committing the carried modify")
    assert not has_staged_anchor(repo), "anchor cleared after a clean commit"

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "mod.txt" in dump, (
        f"committed modify must appear in the sealed tree:\n{dump}"
    )
    assert _chain_read(repo, "mod.txt") == "mod locally edited longer\n", (
        "committed content on disk"
    )


@pytest.mark.smoke
def test_chain_mergeconflict_resolve_theirs_commit_switch(new_lore_repo):
    """Chain: modify/modify conflict on main -> merge feature -> resolve theirs
    -> commit the merge -> branch switch to feature and back to main, validating
    that the merged content and a clean status survive the round-trip.

    A resolved-and-committed conflict is a normal revision: switching away and
    back leaves the merged content in place with nothing pending."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "stay.txt": "stay\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edits conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, f"expected a conflict, got:\n{merge_output}"
    assert "conflict.txt" in _mergeconflict_unresolved(repo)

    repo.branch_merge_resolve_theirs("conflict.txt", offline=True)
    repo.commit("merge resolved theirs", offline=True)

    # The resolved merge is committed and clean.
    after = get_status_files(repo)
    assert_file_set(after, [], msg="status clean after committing the resolved merge")
    assert not has_staged_anchor(repo), "anchor cleared after the merge commit"
    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "feature\n", "resolve theirs takes feature's content"

    # Switch away and back: the merged content and clean status survive.
    repo.branch_switch("feature", offline=True)
    repo.branch_switch("main", offline=True)
    assert "On branch main" in repo.status(offline=True), (
        "switch should land back on main"
    )
    roundtrip = get_status_files(repo)
    assert_file_set(
        roundtrip, [], msg="status stays clean after switching away and back"
    )
    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "feature\n", "merged content survives the switch round-trip"


# ===========================================================================
# A scan-detected nested-dir add carries across a branch switch
# ===========================================================================


@pytest.mark.smoke
def test_switch_scan_nested_add_carries(new_lore_repo):
    """A scan-detected dirty ADD in a brand-new nested directory carries across
    a same-revision branch switch, exactly like a flat scan-detected add and an
    explicit `file dirty` nested add both do. The intermediate directory nodes
    carry with the leaf, and the on-disk file is preserved.
    """
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})
    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    repo.make_dirs("nested/sub")
    with repo.open_file("nested/sub/leaf.txt", "w+") as f:
        f.write("nested leaf\n")
    with repo.open_file("flat.txt", "w+") as f:
        f.write("flat add\n")

    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(
        scanned,
        "nested/sub/leaf.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(
        scanned, "flat.txt", action="add", dirty=True, staged=False, node_type="file"
    )

    repo.branch_switch("other", offline=True)
    carried = get_status_files(repo)
    # The flat scan-detected add carries across the switch.
    assert_entry(
        carried,
        "flat.txt",
        action="add",
        dirty=True,
        staged=False,
        msg="a flat scan-detected add carries across a same-revision switch",
    )
    # The scan-detected nested-dir add carries across the switch.
    assert_entry(
        carried,
        "nested/sub/leaf.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="a scan-detected nested-dir add carries across a same-revision switch",
    )
    assert_file_set(
        carried,
        ["flat.txt", "nested/sub/leaf.txt"],
        msg="both scan-detected adds carry across the switch",
    )
    assert os.path.exists(repo._fix_path("nested/sub/leaf.txt")), (
        "the nested add file is present on disk"
    )


# ===========================================================================
# Status / scan / stage detection: nested file-dirty, wide/deep scan,
# staged-state representation, nested stage (default + --scan), and unstage
# across add/delete/modify.
# ===========================================================================


@pytest.mark.smoke
def test_dirty_nested_add_reports_new_dirs(new_lore_repo):
    """An explicit `file dirty` of a brand-new deeply nested leaf reports the
    leaf as action=add/flagDirty and emits each brand-new intermediate
    directory as action=add/type=directory."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/leaf.txt", "w+") as f:
        f.write("nested new content\n")
    repo.dirty("a/b/c/leaf.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["a/b/c/leaf.txt"], msg="only the nested leaf is a changed file"
    )
    assert_entry(
        entries,
        "a/b/c/leaf.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(entries, "a", action="add", node_type="directory")
    assert_entry(entries, "a/b", action="add", node_type="directory")
    assert_entry(entries, "a/b/c", action="add", node_type="directory")


@pytest.mark.smoke
def test_dirty_nested_delete_leaf(new_lore_repo):
    """An explicit `file dirty` of a deleted deeply nested leaf reports the
    leaf as action=delete/flagDirty; surviving ancestor directories and a
    sibling are not reported (the parents still exist on disk)."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "a/b/c/leaf.txt": "leaf content\n",
            "a/b/c/sibling.txt": "sibling stays\n",
            "top.txt": "top\n",
        },
    )

    repo.remove_file("a/b/c/leaf.txt")
    repo.dirty("a/b/c/leaf.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["a/b/c/leaf.txt"], msg="only the deleted nested leaf is reported"
    )
    assert_entry(
        entries,
        "a/b/c/leaf.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_absent(
        entries, "a/b/c", msg="surviving ancestor directory must not be reported"
    )
    assert_absent(entries, "a/b/c/sibling.txt", msg="surviving sibling stays clean")


@pytest.mark.smoke
def test_dirty_nested_modify_leaf(new_lore_repo):
    """An explicit `file dirty` of a modified deeply nested leaf reports the
    leaf as action=keep/flagDirty with no ancestor directory entries (a
    content edit does not change a directory's identity)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/leaf.txt": "leaf original\n", "top.txt": "top\n"})

    with repo.open_file("a/b/c/leaf.txt", "w+") as f:
        f.write("leaf modified longer\n")
    repo.dirty("a/b/c/leaf.txt", offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["a/b/c/leaf.txt"], msg="only the nested leaf is dirty")
    assert_entry(
        entries,
        "a/b/c/leaf.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_absent(
        entries, "a/b/c", msg="unchanged ancestor directory must not be reported"
    )


@pytest.mark.smoke
def test_scan_wide_deep_tree_mixed(new_lore_repo):
    """`status --scan` of a wide and deep tree with changes at several depths
    reports each change with the right action and flagDirty: two deep modifies
    (keep), a deep delete (delete), and an add inside an existing committed
    directory (add) — all flagDirty, none staged. The full set persists into a
    later no-scan status and is idempotent across re-scan."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "x/a.txt": "xa\n",
            "x/y/b.txt": "xyb original\n",
            "x/y/z/c.txt": "xyzc original\n",
            "p/q/d.txt": "pqd original\n",
            "p/keep.txt": "p keep\n",
            "root.txt": "root\n",
        },
    )

    with repo.open_file("x/y/b.txt", "w+") as f:
        f.write("xyb modified much longer\n")
    with repo.open_file("x/y/z/c.txt", "w+") as f:
        f.write("xyzc modified much longer\n")
    repo.remove_file("p/q/d.txt")
    # Add inside an already-committed directory so the add tracks under its full path.
    with repo.open_file("p/added.txt", "w+") as f:
        f.write("add in an existing dir\n")

    changed = ["x/y/b.txt", "x/y/z/c.txt", "p/q/d.txt", "p/added.txt"]

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, changed, msg="scan must report exactly the changed leaves")
    assert_entry(
        scanned, "x/y/b.txt", action="keep", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        scanned,
        "x/y/z/c.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(
        scanned,
        "p/q/d.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_entry(
        scanned, "p/added.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_absent(scanned, "x/a.txt", msg="unchanged deep sibling stays clean")
    assert_absent(scanned, "p/keep.txt", msg="unchanged sibling stays clean")
    assert_absent(scanned, "root.txt", msg="unchanged root file stays clean")

    persisted = get_status_files(repo)
    assert_file_set(
        persisted, changed, msg="the whole wide/deep change set must persist to no-scan"
    )
    assert_entry(persisted, "x/y/b.txt", action="keep", dirty=True, staged=False)
    assert_entry(persisted, "x/y/z/c.txt", action="keep", dirty=True, staged=False)
    assert_entry(persisted, "p/q/d.txt", action="delete", dirty=True, staged=False)
    assert_entry(persisted, "p/added.txt", action="add", dirty=True, staged=False)


# ---------------------------------------------------------------------------
# Staged-state status representation (no-scan AND --scan) across change types
# and flat vs nested.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_status_staged_modify_flat_representation(new_lore_repo):
    """A staged modify (commit, modify, stage with no prior dirty mark) is
    reported as action=keep/flagStaged with flagDirty=True — staging marks the
    working-tree difference dirty — consistently by no-scan status, a
    `status --scan`, and a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.stage("file.txt", offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, ["file.txt"])
    assert_entry(
        no_scan,
        "file.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged modify reported as keep/dirty/staged in no-scan status",
    )

    scanned = get_status_files(repo, scan=True)
    assert_file_set(scanned, ["file.txt"])
    assert_entry(
        scanned,
        "file.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="--scan reports the staged modify identically",
    )

    persisted = get_status_files(repo)
    assert_file_set(persisted, ["file.txt"])
    assert_entry(
        persisted,
        "file.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="the staged dirty modify persists in a later no-scan status",
    )


@pytest.mark.smoke
def test_status_staged_add_flat_representation(new_lore_repo):
    """A staged add (create file, stage with no prior dirty mark) is reported
    as action=add/flagStaged with flagDirty=True — staging marks the
    working-tree difference dirty — consistently by no-scan status, a
    `status --scan`, and a later no-scan status; an untouched committed file
    stays clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("added.txt", "w+") as f:
        f.write("brand new content\n")
    repo.stage("added.txt", offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, ["added.txt"])
    assert_entry(
        no_scan,
        "added.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged add reported as add/dirty/staged in no-scan status",
    )
    assert_absent(no_scan, "base.txt", msg="untouched committed file stays clean")

    scanned = get_status_files(repo, scan=True)
    assert_file_set(scanned, ["added.txt"])
    assert_entry(
        scanned,
        "added.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="--scan reports the staged add identically",
    )

    persisted = get_status_files(repo)
    assert_file_set(persisted, ["added.txt"])
    assert_entry(
        persisted,
        "added.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="the staged dirty add persists in a later no-scan status",
    )


@pytest.mark.smoke
def test_status_staged_delete_flat_representation(new_lore_repo):
    """A staged delete (commit, remove file, dirty, stage) is reported as
    action=delete with flagStaged=True and flagDirty=True (the dirty mark
    persists alongside the stage), identically by no-scan and --scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "will be deleted\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)
    repo.stage("victim.txt", offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, ["victim.txt"])
    assert_entry(
        no_scan,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged delete reported as delete/dirty/staged in no-scan status",
    )
    assert_absent(no_scan, "keep.txt", msg="untouched committed file stays clean")

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["victim.txt"])
    assert_entry(
        scanned,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="--scan reports the staged delete identically",
    )


@pytest.mark.smoke
def test_status_staged_modify_nested_representation(new_lore_repo):
    """A staged modify of a deeply nested leaf is reported as
    action=keep/flagStaged/flagDirty=True for the leaf only (staging marks the
    working-tree difference dirty), with no ancestor directory entries —
    consistently by no-scan status, a `status --scan`, and a later no-scan
    status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/leaf.txt": "leaf original\n", "top.txt": "top\n"})

    with repo.open_file("a/b/c/leaf.txt", "w+") as f:
        f.write("leaf modified longer\n")
    repo.stage("a/b/c/leaf.txt", offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, ["a/b/c/leaf.txt"])
    assert_entry(
        no_scan,
        "a/b/c/leaf.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged nested modify reported as keep/dirty/staged",
    )
    assert_absent(
        no_scan, "a/b/c", msg="unchanged ancestor directory must not be reported"
    )

    scanned = get_status_files(repo, scan=True)
    assert_file_set(scanned, ["a/b/c/leaf.txt"])
    assert_entry(
        scanned,
        "a/b/c/leaf.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="--scan reports the staged nested modify identically",
    )
    assert_absent(scanned, "a/b/c", msg="ancestor dir still unreported under --scan")

    persisted = get_status_files(repo)
    assert_entry(
        persisted,
        "a/b/c/leaf.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="the staged dirty nested modify persists in a later no-scan status",
    )


@pytest.mark.smoke
def test_status_staged_add_nested_representation(new_lore_repo):
    """A staged add of a brand-new nested leaf is reported as
    action=add/flagStaged/flagDirty=True (staging marks the working-tree
    difference dirty), and every brand-new ancestor directory is emitted as an
    action=add/type=directory staged node — consistently by no-scan status, a
    `status --scan`, and a later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/leaf.txt", "w+") as f:
        f.write("nested add content\n")
    repo.stage("a/b/c/leaf.txt", offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(
        no_scan, ["a/b/c/leaf.txt"], msg="only the leaf is a staged add file"
    )
    assert_entry(
        no_scan,
        "a/b/c/leaf.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged nested add leaf reported as add/dirty/staged",
    )
    assert_entry(
        no_scan, "a", action="add", dirty=True, staged=True, node_type="directory"
    )
    assert_entry(
        no_scan, "a/b", action="add", dirty=True, staged=True, node_type="directory"
    )
    assert_entry(
        no_scan, "a/b/c", action="add", dirty=True, staged=True, node_type="directory"
    )

    scanned = get_status_files(repo, scan=True)
    assert_file_set(
        scanned,
        ["a/b/c/leaf.txt"],
        msg="--scan keeps only the leaf as a staged add file",
    )
    assert_entry(
        scanned,
        "a/b/c/leaf.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="--scan reports the staged nested add identically",
    )

    persisted = get_status_files(repo)
    assert_entry(
        persisted,
        "a/b/c/leaf.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="the staged dirty nested add persists in a later no-scan status",
    )


@pytest.mark.smoke
def test_status_staged_delete_nested_representation(new_lore_repo):
    """A staged delete of a deeply nested leaf is reported as action=delete
    with flagStaged=True (and flagDirty=True from the dirty mark) for the leaf,
    with no ancestor directory entries, identically by no-scan and --scan."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/leaf.txt": "leaf content\n", "top.txt": "top\n"})

    repo.remove_file("a/b/c/leaf.txt")
    repo.dirty("a/b/c/leaf.txt", offline=True)
    repo.stage("a/b/c/leaf.txt", offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, ["a/b/c/leaf.txt"])
    assert_entry(
        no_scan,
        "a/b/c/leaf.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged nested delete reported as delete/dirty/staged",
    )
    assert_absent(
        no_scan, "a/b/c", msg="surviving ancestor directory must not be reported"
    )

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["a/b/c/leaf.txt"])
    assert_entry(
        scanned,
        "a/b/c/leaf.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="--scan reports the staged nested delete identically",
    )


# ---------------------------------------------------------------------------
# Nested default stage (from dirty marks) and nested stage --scan.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_stage_default_nested_modify_add_delete(new_lore_repo):
    """Default `stage` (no --scan) of dirty-marked nested leaves stages a
    modify (keep), an add (add) and a delete (delete) several directories deep,
    each flagStaged=True with its dirty flag preserved (flagDirty=True)."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"a/b/mod.txt": "mod original\n", "a/b/del.txt": "del original\n"}
    )

    with repo.open_file("a/b/mod.txt", "w+") as f:
        f.write("mod changed longer\n")
    repo.remove_file("a/b/del.txt")
    with repo.open_file("a/b/add.txt", "w+") as f:
        f.write("nested add content\n")
    repo.dirty(["a/b/mod.txt", "a/b/del.txt", "a/b/add.txt"], offline=True)

    repo.stage(scan=False, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["a/b/mod.txt", "a/b/del.txt", "a/b/add.txt"])
    assert_entry(
        entries, "a/b/mod.txt", action="keep", dirty=True, staged=True, node_type="file"
    )
    assert_entry(
        entries, "a/b/add.txt", action="add", dirty=True, staged=True, node_type="file"
    )
    assert_entry(
        entries,
        "a/b/del.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
    )


@pytest.mark.smoke
def test_stage_default_stages_scan_detected_nested_leaf(new_lore_repo):
    """Default `stage` (no --scan, no explicit path) traverses the directory
    chain and stages a deeply nested leaf whose dirty state was recorded by a
    prior `status --scan`, preserving flagDirty. The unchanged ancestor
    directories are not reported."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/deep.txt": "deep original\n", "top.txt": "top\n"})

    with repo.open_file("a/b/c/deep.txt", "w+") as f:
        f.write("deep modified longer\n")

    # Scan records the nested dirty leaf (no staging yet).
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["a/b/c/deep.txt"], msg="scan detects only the deep leaf")
    assert_entry(
        scanned,
        "a/b/c/deep.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
    )

    # Default stage (no --scan, no path) must traverse to the scan-recorded leaf.
    repo.stage(scan=False, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["a/b/c/deep.txt"], msg="only the scanned leaf is staged")
    assert_entry(
        entries,
        "a/b/c/deep.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="default stage picks up the scan-recorded nested leaf via traversal",
    )
    assert_absent(entries, "a", msg="unchanged ancestor dir must not be reported")
    assert_absent(entries, "a/b", msg="unchanged ancestor dir must not be reported")
    assert_absent(entries, "a/b/c", msg="unchanged ancestor dir must not be reported")


@pytest.mark.smoke
def test_stage_scan_nested_modify_add_delete(new_lore_repo):
    """`stage --scan` walks a nested subtree and stages a deep modify (keep), a
    deep delete (delete), and a deep add in a brand-new directory (add) — every
    staged node flagStaged=True/flagDirty=True (the scan detects and marks each
    change dirty), and the brand-new directory is staged as an
    action=add/type=directory node."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"a/b/mod.txt": "mod original\n", "a/b/del.txt": "del original\n"}
    )

    with repo.open_file("a/b/mod.txt", "w+") as f:
        f.write("mod changed longer\n")
    repo.remove_file("a/b/del.txt")
    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/add.txt", "w+") as f:
        f.write("deep add content\n")

    repo.stage(scan=True, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["a/b/mod.txt", "a/b/del.txt", "a/b/c/add.txt"])
    assert_entry(
        entries, "a/b/mod.txt", action="keep", dirty=True, staged=True, node_type="file"
    )
    assert_entry(
        entries,
        "a/b/del.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
    )
    assert_entry(
        entries,
        "a/b/c/add.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
    )
    assert_entry(
        entries, "a/b/c", action="add", dirty=True, staged=True, node_type="directory"
    )


# ---------------------------------------------------------------------------
# unstage across staged add / delete / modify (flat and nested).
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_unstage_staged_add_flat(new_lore_repo):
    """Unstaging a staged add clears only the staged flag; the add survives as
    action=add/flagDirty (the file is still on disk and differs from committed)
    in both no-scan and --scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("added.txt", "w+") as f:
        f.write("brand new content\n")
    repo.stage("added.txt", offline=True)
    assert_entry(
        get_status_files(repo),
        "added.txt",
        action="add",
        dirty=True,
        staged=True,
        msg="staged+dirty before unstage",
    )

    repo.unstage(offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, ["added.txt"], msg="the dirty add survives unstage")
    assert_entry(
        no_scan,
        "added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="unstage clears staged but keeps the dirty add",
    )
    assert os.path.exists(repo._fix_path("added.txt")), "the added file stays on disk"

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["added.txt"], msg="--scan agrees the dirty add remains")
    assert_entry(scanned, "added.txt", action="add", dirty=True, staged=False)


@pytest.mark.smoke
def test_unstage_staged_delete_dirty_flat(new_lore_repo):
    """Unstaging a staged delete that carries a dirty mark clears only the
    staged flag; the deletion survives as action=delete/flagDirty (the file is
    still gone on disk) in both no-scan and --scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "will be deleted\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)
    repo.stage("victim.txt", offline=True)
    assert_entry(
        get_status_files(repo),
        "victim.txt",
        action="delete",
        dirty=True,
        staged=True,
        msg="staged before unstage",
    )

    repo.unstage(offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, ["victim.txt"], msg="the dirty delete survives unstage")
    assert_entry(
        no_scan,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="unstage clears staged but keeps the dirty delete",
    )
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "the deleted file stays gone on disk"
    )

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned, ["victim.txt"], msg="--scan agrees the dirty delete remains"
    )
    assert_entry(scanned, "victim.txt", action="delete", dirty=True, staged=False)


@pytest.mark.smoke
def test_unstage_staged_modify_survives_when_differs(new_lore_repo):
    """Unstaging a staged+dirty modify whose on-disk content still differs from
    committed clears only the staged flag; the modification survives as
    action=keep/flagDirty, with its on-disk content intact."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)
    repo.stage("file.txt", offline=True)
    assert_entry(
        get_status_files(repo),
        "file.txt",
        action="keep",
        dirty=True,
        staged=True,
        msg="staged before unstage",
    )

    repo.unstage(offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, ["file.txt"], msg="the dirty modify survives unstage")
    assert_entry(
        no_scan,
        "file.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="unstage clears staged but keeps the dirty modify",
    )
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "modified content longer\n", (
            "unstage must not touch on-disk content"
        )


@pytest.mark.smoke
def test_unstage_staged_modify_clears_when_reverted(new_lore_repo):
    """Unstaging a staged+dirty modify whose on-disk content was reverted back
    to the committed content drops the stale dirty flag entirely: status is
    clean in both no-scan and --scan (dirty survives only where on-disk still
    differs)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "original content\n"})

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)
    repo.stage("file.txt", offline=True)
    # Revert the on-disk content to exactly the committed bytes before unstaging.
    with repo.open_file("file.txt", "w+") as f:
        f.write("original content\n")

    repo.unstage(offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(
        no_scan, [], msg="a reverted staged modify clears entirely on unstage"
    )
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, [], msg="--scan agrees the reverted file is clean")


@pytest.mark.smoke
def test_unstage_staged_add_nested(new_lore_repo):
    """Unstaging a staged add of a brand-new nested leaf clears only the staged
    flag; the add survives as action=add/flagDirty (file still on disk) and the
    brand-new ancestor directories survive as dirty add directory nodes, in both
    no-scan and --scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/leaf.txt", "w+") as f:
        f.write("nested add content\n")
    repo.stage("a/b/c/leaf.txt", scan=True, offline=True)
    assert_entry(
        get_status_files(repo),
        "a/b/c/leaf.txt",
        action="add",
        dirty=True,
        staged=True,
        msg="staged+dirty before unstage",
    )

    repo.unstage(offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(
        no_scan, ["a/b/c/leaf.txt"], msg="the nested dirty add survives unstage"
    )
    assert_entry(
        no_scan,
        "a/b/c/leaf.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="unstage clears staged but keeps the nested dirty add",
    )
    assert_entry(
        no_scan, "a", action="add", dirty=True, staged=False, node_type="directory"
    )
    assert_entry(
        no_scan, "a/b", action="add", dirty=True, staged=False, node_type="directory"
    )
    assert_entry(
        no_scan, "a/b/c", action="add", dirty=True, staged=False, node_type="directory"
    )
    assert os.path.exists(repo._fix_path("a/b/c/leaf.txt")), (
        "the nested added file stays on disk"
    )

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned, ["a/b/c/leaf.txt"], msg="--scan agrees the nested dirty add remains"
    )
    assert_entry(scanned, "a/b/c/leaf.txt", action="add", dirty=True, staged=False)


@pytest.mark.smoke
def test_unstage_staged_delete_nested(new_lore_repo):
    """Unstaging a staged delete of a deeply nested leaf (dirty-marked) clears
    only the staged flag; the deletion survives as action=delete/flagDirty in
    both no-scan and --scan status, with no ancestor directory entries."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/leaf.txt": "leaf content\n", "top.txt": "top\n"})

    repo.remove_file("a/b/c/leaf.txt")
    repo.dirty("a/b/c/leaf.txt", offline=True)
    repo.stage("a/b/c/leaf.txt", offline=True)
    assert_entry(
        get_status_files(repo),
        "a/b/c/leaf.txt",
        action="delete",
        dirty=True,
        staged=True,
        msg="staged before unstage",
    )

    repo.unstage(offline=True)

    no_scan = get_status_files(repo)
    assert_file_set(
        no_scan, ["a/b/c/leaf.txt"], msg="the nested dirty delete survives unstage"
    )
    assert_entry(
        no_scan,
        "a/b/c/leaf.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="unstage clears staged but keeps the nested dirty delete",
    )
    assert_absent(
        no_scan, "a/b/c", msg="surviving ancestor directory must not be reported"
    )

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(
        scanned, ["a/b/c/leaf.txt"], msg="--scan agrees the nested dirty delete remains"
    )
    assert_entry(scanned, "a/b/c/leaf.txt", action="delete", dirty=True, staged=False)


# ---------------------------------------------------------------------------
# move/copy into nested/new directories (skipped: move/copy not fully implemented).
# ---------------------------------------------------------------------------


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_scan_dirty_move_into_nested_dir(new_lore_repo):
    """`status --scan` of a dirty move into a brand-new nested directory keeps
    the destination as action=move/fromPath=source (the rename is still on
    disk), surfaces the new destination directory nodes, and never reports the
    source."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"src.txt": "movable content\n"})

    repo.make_dirs("dest/sub")
    os.rename(repo._fix_path("src.txt"), repo._fix_path("dest/sub/src.txt"))
    repo.dirty_move("src.txt", "dest/sub/src.txt", offline=True)

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["dest/sub/src.txt"])
    assert_entry(
        scanned,
        "dest/sub/src.txt",
        action="move",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="src.txt",
        msg="--scan must preserve the nested dirty-move provenance",
    )
    assert_absent(scanned, "src.txt", msg="move source must not reappear after scan")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_stage_dirty_move_into_nested_dir(new_lore_repo):
    """Default `stage` of a dirty move into a brand-new nested directory stages
    the destination as action=move/fromPath=source (flagDirty preserved) and
    stages the new destination directory nodes; the source is never reported."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"src.txt": "movable content\n"})

    repo.make_dirs("dest/sub")
    os.rename(repo._fix_path("src.txt"), repo._fix_path("dest/sub/src.txt"))
    repo.dirty_move("src.txt", "dest/sub/src.txt", offline=True)

    repo.stage(scan=False, offline=True)

    entries = get_status_files(repo)
    assert_file_set(entries, ["dest/sub/src.txt"])
    assert_entry(
        entries,
        "dest/sub/src.txt",
        action="move",
        dirty=True,
        staged=True,
        node_type="file",
        from_path="src.txt",
        msg="default stage of a dirty move keeps move provenance and sets staged",
    )
    assert_absent(entries, "src.txt", msg="move source must not appear")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_scan_dirty_copy_into_nested_dir(new_lore_repo):
    """`status --scan` of a dirty copy into a brand-new nested directory keeps
    the destination as action=copy/fromPath=source and leaves the source
    clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"orig.txt": "source content\n"})

    repo.make_dirs("newdir/sub")
    with repo.open_file("newdir/sub/copy.txt", "w+") as f:
        f.write("source content\n")
    repo.dirty_copy("orig.txt", "newdir/sub/copy.txt", offline=True)

    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, ["newdir/sub/copy.txt"])
    assert_entry(
        scanned,
        "newdir/sub/copy.txt",
        action="copy",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="orig.txt",
        msg="--scan must preserve the nested dirty-copy provenance",
    )
    assert_absent(scanned, "orig.txt", msg="copy source is unchanged")


# ===========================================================================
# file reset / commit / branch reset
# ===========================================================================


# ---------------------------------------------------------------------------
# file reset (repo.reset) of a STAGED node refuses; the stage survives
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_reset_refuses_staged_add_flat(new_lore_repo):
    """Resetting a STAGED add refuses with the staged-node guard; the staged
    add survives and its on-disk content is untouched."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("added.txt", "w+") as f:
        f.write("added content\n")
    repo.stage("added.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(
        before, "added.txt", action="add", dirty=True, staged=True, node_type="file"
    )

    with pytest.raises(LoreException) as excinfo:
        repo.reset("added.txt", offline=True)
    assert "Failed to reset staged node" in str(excinfo.value), (
        f"refusal should name the staged-node guard, got: {excinfo.value}"
    )

    after = get_status_files(repo)
    assert_entry(
        after,
        "added.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged add must remain staged after a refused reset",
    )
    assert_file_set(after, ["added.txt"], msg="only the staged add is tracked")
    with repo.open_file("added.txt", "r") as f:
        assert f.read() == "added content\n", "refused reset must not touch content"


@pytest.mark.smoke
def test_reset_refuses_staged_add_nested(new_lore_repo):
    """Resetting a STAGED add several directories deep refuses with the
    staged-node guard; the staged add survives."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/added.txt", "w+") as f:
        f.write("nested added\n")
    repo.stage("a/b/c/added.txt", scan=True, offline=True)

    before = get_status_files(repo)
    assert_entry(
        before,
        "a/b/c/added.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
    )

    with pytest.raises(LoreException) as excinfo:
        repo.reset("a/b/c/added.txt", offline=True)
    assert "Failed to reset staged node" in str(excinfo.value), (
        f"refusal should name the staged-node guard, got: {excinfo.value}"
    )

    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/c/added.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged add must remain staged after a refused reset",
    )
    assert_file_set(after, ["a/b/c/added.txt"], msg="only the staged add is tracked")


@pytest.mark.smoke
def test_reset_refuses_staged_modify_nested(new_lore_repo):
    """Resetting a STAGED modify several directories deep refuses with the
    staged-node guard; the staged modify survives and content is untouched."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/deep.txt": "deep original\n", "top.txt": "top\n"})

    with repo.open_file("a/b/c/deep.txt", "w+") as f:
        f.write("deep modified longer\n")
    repo.stage("a/b/c/deep.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(
        before,
        "a/b/c/deep.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
    )

    with pytest.raises(LoreException) as excinfo:
        repo.reset("a/b/c/deep.txt", offline=True)
    assert "Failed to reset staged node" in str(excinfo.value), (
        f"refusal should name the staged-node guard, got: {excinfo.value}"
    )

    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/c/deep.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged modify must remain staged after a refused reset",
    )
    assert_file_set(after, ["a/b/c/deep.txt"], msg="only the staged modify is tracked")
    with repo.open_file("a/b/c/deep.txt", "r") as f:
        assert f.read() == "deep modified longer\n", (
            "refused reset must not touch content"
        )


@pytest.mark.smoke
def test_reset_refuses_staged_delete_flat(new_lore_repo):
    """Resetting a STAGED delete refuses with the staged-node guard; the
    staged delete survives and the file stays removed from disk."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "precious\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)
    repo.stage("victim.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(
        before, "victim.txt", action="delete", dirty=True, staged=True, node_type="file"
    )

    with pytest.raises(LoreException) as excinfo:
        repo.reset("victim.txt", offline=True)
    assert "Failed to reset staged node" in str(excinfo.value), (
        f"refusal should name the staged-node guard, got: {excinfo.value}"
    )

    after = get_status_files(repo)
    assert_entry(
        after,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged delete must remain staged after a refused reset",
    )
    assert_file_set(after, ["victim.txt"], msg="only the staged delete is tracked")
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "refused reset must not restore the staged-deleted file"
    )


@pytest.mark.smoke
def test_reset_refuses_staged_delete_nested(new_lore_repo):
    """Resetting a STAGED delete several directories deep refuses with the
    staged-node guard; the staged delete survives and the file stays removed."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/deep.txt": "deep precious\n", "top.txt": "top\n"})

    repo.remove_file("a/b/c/deep.txt")
    repo.dirty("a/b/c/deep.txt", offline=True)
    repo.stage("a/b/c/deep.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(
        before,
        "a/b/c/deep.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
    )

    with pytest.raises(LoreException) as excinfo:
        repo.reset("a/b/c/deep.txt", offline=True)
    assert "Failed to reset staged node" in str(excinfo.value), (
        f"refusal should name the staged-node guard, got: {excinfo.value}"
    )

    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/c/deep.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged delete must remain staged after a refused reset",
    )
    assert_file_set(after, ["a/b/c/deep.txt"], msg="only the staged delete is tracked")
    assert not os.path.exists(repo._fix_path("a/b/c/deep.txt")), (
        "refused reset must not restore the staged-deleted nested file"
    )


# ---------------------------------------------------------------------------
# file reset (repo.reset) of a dirty node, NESTED
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_reset_dirty_add_nested_keeps_file(new_lore_repo):
    """Resetting a dirty add several directories deep keeps the untracked file
    on disk and clears its dirty tracking; no-scan status is clean and a later
    --scan rediscovers it as an add."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/new.txt", "w+") as f:
        f.write("nested brand new\n")
    repo.dirty("a/b/c/new.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(before, "a/b/c/new.txt", action="add", dirty=True, staged=False)

    repo.reset("a/b/c/new.txt", offline=True)

    assert os.path.exists(repo._fix_path("a/b/c/new.txt")), (
        "plain reset must keep the untracked nested add on disk; only --purge removes it"
    )
    with repo.open_file("a/b/c/new.txt", "r") as f:
        assert f.read() == "nested brand new\n", (
            "untracked content must be intact after reset"
        )

    no_scan = get_status_files(repo)
    assert_file_set(
        no_scan, [], msg="reset must clear the dirty-add; no-scan status clean"
    )
    for ancestor in ("a", "a/b", "a/b/c"):
        assert_absent(no_scan, ancestor, msg="ancestor dir must not stay dirty")

    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(
        scanned,
        "a/b/c/new.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="--scan rediscovers the surviving untracked nested file as an add",
    )


@pytest.mark.smoke
def test_reset_dirty_delete_nested_restores(new_lore_repo):
    """Resetting a dirty delete several directories deep restores the file on
    disk from the committed revision and clears it (and every intermediate
    parent) from status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/deep.txt": "deep precious\n", "a/sibling.txt": "sib\n"})

    repo.remove_file("a/b/c/deep.txt")
    repo.dirty("a/b/c/deep.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(before, "a/b/c/deep.txt", action="delete", dirty=True, staged=False)

    repo.reset("a/b/c/deep.txt", offline=True)

    assert os.path.exists(repo._fix_path("a/b/c/deep.txt")), (
        "deleted nested file must be restored"
    )
    with repo.open_file("a/b/c/deep.txt", "r") as f:
        assert f.read() == "deep precious\n", "restored content must match committed"

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, [], msg="reset must clear the leaf and all dirty parents")
    for ancestor in ("a", "a/b", "a/b/c"):
        assert_absent(no_scan, ancestor, msg="intermediate parent must not stay dirty")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, [], msg="--scan agrees the subtree is clean")
    assert not has_staged_anchor(repo), (
        "anchor released after the only dirty leaf reset"
    )


# ---------------------------------------------------------------------------
# commit of a STAGED change; the dump reflects the committed change
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_commit_staged_add_flat_dump(new_lore_repo):
    """Committing a staged add lands the new file in the sealed tree and
    leaves a clean status (no-scan and --scan)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    with repo.open_file("added.txt", "w+") as f:
        f.write("added content\n")
    repo.stage("added.txt", offline=True)

    staged = get_status_files(repo)
    assert_entry(
        staged, "added.txt", action="add", dirty=True, staged=True, node_type="file"
    )

    repo.commit("commit staged add", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status must be clean after committing the add, got {summarize(entries)}"
    )
    scanned = get_status_files_twice(repo, scan=True)
    assert scanned == [], f"--scan status must be clean, got {summarize(scanned)}"

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "added.txt" in dump, (
        f"staged add should be present in the sealed tree:\n{dump}"
    )


@pytest.mark.smoke
def test_commit_staged_modify_flat_dump(new_lore_repo):
    """Committing a staged modify updates the file's content hash in the sealed
    tree (it differs from the base revision) and leaves a clean status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"mod.txt": "mod original\n"})
    repo.status(reset=True, offline=True)
    base_addr = _node_addr_in_dump(repo.repository_dump(), "mod.txt")
    assert base_addr, "base addr should parse"

    with repo.open_file("mod.txt", "w+") as f:
        f.write("mod modified longer\n")
    repo.stage("mod.txt", offline=True)

    staged = get_status_files(repo)
    assert_entry(
        staged, "mod.txt", action="keep", dirty=True, staged=True, node_type="file"
    )

    repo.commit("commit staged modify", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status must be clean after committing the modify, got {summarize(entries)}"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "mod.txt" in dump, f"modified file must remain in the sealed tree:\n{dump}"
    assert _node_addr_in_dump(dump, "mod.txt") != base_addr, (
        f"staged modify's content hash should change in the sealed tree:\n{dump}"
    )


@pytest.mark.smoke
def test_commit_staged_delete_flat_dump(new_lore_repo):
    """Committing a staged delete drops the file from the sealed tree and
    leaves a clean status; an unrelated committed file is untouched."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "will be deleted\n", "keep.txt": "stays\n"})

    repo.remove_file("victim.txt")
    repo.stage("victim.txt", scan=True, offline=True)

    staged = get_status_files(repo)
    assert_entry(
        staged, "victim.txt", action="delete", dirty=True, staged=True, node_type="file"
    )

    repo.commit("commit staged delete", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status must be clean after committing the delete, got {summarize(entries)}"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "victim.txt" not in dump, (
        f"deleted file must be gone from the sealed tree:\n{dump}"
    )
    assert "keep.txt" in dump, f"unrelated file must remain in the sealed tree:\n{dump}"


@pytest.mark.smoke
def test_commit_staged_modify_nested_dump(new_lore_repo):
    """Committing a staged modify of a deeply nested file updates that file's
    content hash in the sealed tree and leaves a clean status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/c/deep.txt": "deep original\n", "top.txt": "top\n"})
    repo.status(reset=True, offline=True)
    base_addr = _node_addr_in_dump(repo.repository_dump(), "a/b/c/deep.txt")
    assert base_addr, "base addr should parse"

    with repo.open_file("a/b/c/deep.txt", "w+") as f:
        f.write("deep modified longer\n")
    repo.stage("a/b/c/deep.txt", offline=True)

    staged = get_status_files(repo)
    assert_entry(
        staged,
        "a/b/c/deep.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
    )

    repo.commit("commit nested staged modify", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status must be clean after the commit, got {summarize(entries)}"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "a/b/c/deep.txt" in dump, (
        f"nested modified file must remain in the sealed tree:\n{dump}"
    )
    assert _node_addr_in_dump(dump, "a/b/c/deep.txt") != base_addr, (
        f"nested staged modify's content hash should change in the sealed tree:\n{dump}"
    )


# ---------------------------------------------------------------------------
# commit with a dirty-only NESTED node that survives the commit
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_commit_dirty_only_modify_nested_survives(new_lore_repo):
    """Committing a staged change leaves a dirty-only modify of a deeply nested
    file pending (action=keep/flagDirty); the staged anchor persists and the
    dirty-only file's content does not reach the sealed tree."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"a/b/c/deep.txt": "deep original\n", "staged.txt": "staged original\n"}
    )
    repo.status(reset=True, offline=True)
    base_addr = _node_addr_in_dump(repo.repository_dump(), "a/b/c/deep.txt")
    assert base_addr, "base addr should parse"

    with repo.open_file("a/b/c/deep.txt", "w+") as f:
        f.write("deep dirty longer\n")
    repo.dirty("a/b/c/deep.txt", offline=True)

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged modified longer\n")
    repo.stage("staged.txt", offline=True)

    repo.commit("commit staged", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["a/b/c/deep.txt"], msg="only the dirty-only nested modify remains"
    )
    assert_absent(entries, "staged.txt", msg="staged modify is committed and clean")
    assert_entry(
        entries,
        "a/b/c/deep.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-only nested modify survives commit as a kept dirty file",
    )
    assert has_staged_anchor(repo), (
        "anchor must persist while a dirty-only node remains"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert _node_addr_in_dump(dump, "a/b/c/deep.txt") == base_addr, (
        f"dirty-only modify must not reach the sealed tree (hash unchanged):\n{dump}"
    )


@pytest.mark.smoke
def test_commit_dirty_only_add_nested_survives(new_lore_repo):
    """Committing a staged change leaves a dirty-only add in a brand-new nested
    directory pending (action=add/flagDirty); the added file and its new dirs
    are absent from the sealed tree."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"staged.txt": "staged original\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/new.txt", "w+") as f:
        f.write("nested dirty add\n")
    repo.dirty("a/b/c/new.txt", offline=True)

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged modified longer\n")
    repo.stage("staged.txt", offline=True)

    repo.commit("commit staged", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["a/b/c/new.txt"], msg="only the dirty-only nested add remains"
    )
    assert_absent(entries, "staged.txt", msg="staged modify is committed and clean")
    assert_entry(
        entries,
        "a/b/c/new.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-only nested add stays pending after commit",
    )
    assert has_staged_anchor(repo), (
        "anchor must persist while a dirty-only node remains"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "new.txt" not in dump, (
        f"dirty-only add must not be in the sealed tree:\n{dump}"
    )
    assert "a/b/c/" not in dump, (
        f"dirty-only added dirs must not be in the sealed tree:\n{dump}"
    )


@pytest.mark.smoke
def test_commit_dirty_only_delete_nested_survives(new_lore_repo):
    """Committing an unrelated staged change leaves a dirty-only delete of a
    deeply nested file pending (action=delete/flagDirty); the deleted file is
    reverted in the sealed tree (still present in the dump)."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"a/b/c/deep.txt": "deep precious\n", "other.txt": "other original\n"}
    )

    repo.remove_file("a/b/c/deep.txt")
    repo.dirty("a/b/c/deep.txt", offline=True)

    with repo.open_file("other.txt", "w+") as f:
        f.write("other modified longer\n")
    repo.stage("other.txt", offline=True)

    repo.commit("commit unrelated", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["a/b/c/deep.txt"], msg="only the dirty-only nested delete remains"
    )
    assert_absent(entries, "other.txt", msg="staged modify is committed and clean")
    assert_entry(
        entries,
        "a/b/c/deep.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-only nested delete stays pending after commit",
    )
    assert has_staged_anchor(repo), (
        "anchor must persist while a dirty-only node remains"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "a/b/c/deep.txt" in dump, (
        f"dirty-only delete must be reverted in the sealed tree:\n{dump}"
    )


# ---------------------------------------------------------------------------
# branch reset (repo.branch_reset) to an earlier revision carrying a dirty node
# ---------------------------------------------------------------------------


def _commit_two_revs(repo: Lore, files: dict[str, str]) -> str:
    """Commit `files` as v1, then bump base.txt to make a v2 tip, returning the
    v1 revision signature (the branch-reset target)."""
    commit_base(repo, files)
    rev_v1 = repo.revision_history(1, offline=True)[0].signature
    with repo.open_file("base.txt", "w+") as f:
        f.write("base v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)
    return rev_v1


@pytest.mark.smoke
def test_branchreset_carries_dirty_add_flat(new_lore_repo):
    """A dirty-only add pending on the current branch is still reported as a
    pending add after `branch reset` moves the tip to an earlier revision; its
    on-disk content survives."""
    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(repo, {"base.txt": "base v1\n"})

    with repo.open_file("added.txt", "w+") as f:
        f.write("dirty add content\n")
    repo.dirty("added.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "added.txt", action="add", dirty=True, staged=False)

    repo.branch_reset(rev_v1, offline=True)

    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base v1\n", "branch reset must realize v1 of the tip file"

    entries = get_status_files(repo)
    assert_file_set(entries, ["added.txt"], msg="only the carried dirty add is pending")
    assert_entry(
        entries,
        "added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty add must survive branch reset",
    )
    with repo.open_file("added.txt", "r") as f:
        assert f.read() == "dirty add content\n", (
            "carried add keeps its on-disk content"
        )


@pytest.mark.smoke
def test_branchreset_carries_dirty_add_nested(new_lore_repo):
    """A dirty-only add in a brand-new nested directory is still reported as a
    pending add after `branch reset` moves the tip to an earlier revision."""
    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(repo, {"base.txt": "base v1\n"})

    repo.make_dirs("a/b/c")
    with repo.open_file("a/b/c/added.txt", "w+") as f:
        f.write("nested dirty add\n")
    repo.dirty("a/b/c/added.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "a/b/c/added.txt", action="add", dirty=True, staged=False)

    repo.branch_reset(rev_v1, offline=True)

    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base v1\n", "branch reset must realize v1 of the tip file"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["a/b/c/added.txt"], msg="only the carried nested dirty add is pending"
    )
    assert_entry(
        entries,
        "a/b/c/added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty add must survive branch reset",
    )
    with repo.open_file("a/b/c/added.txt", "r") as f:
        assert f.read() == "nested dirty add\n", (
            "carried nested add keeps its on-disk content"
        )


@pytest.mark.smoke
def test_branchreset_carries_dirty_delete_flat(new_lore_repo):
    """A dirty-only delete pending on the current branch is still reported as a
    pending delete after `branch reset` moves the tip to an earlier revision;
    the file stays removed from disk."""
    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(
        repo, {"base.txt": "base v1\n", "victim.txt": "precious\n"}
    )

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "victim.txt", action="delete", dirty=True, staged=False)

    repo.branch_reset(rev_v1, offline=True)

    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base v1\n", "branch reset must realize v1 of the tip file"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["victim.txt"], msg="only the carried dirty delete is pending"
    )
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty delete must survive branch reset",
    )
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "carried delete stays removed on disk"
    )


@pytest.mark.smoke
def test_branchreset_carries_dirty_delete_nested(new_lore_repo):
    """A dirty-only delete of a deeply nested file is still reported as a
    pending delete after `branch reset` moves the tip to an earlier revision;
    the file stays removed from disk."""
    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(
        repo, {"base.txt": "base v1\n", "a/b/c/deep.txt": "deep precious\n"}
    )

    repo.remove_file("a/b/c/deep.txt")
    repo.dirty("a/b/c/deep.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "a/b/c/deep.txt", action="delete", dirty=True, staged=False)

    repo.branch_reset(rev_v1, offline=True)

    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base v1\n", "branch reset must realize v1 of the tip file"

    entries = get_status_files(repo)
    assert_file_set(
        entries,
        ["a/b/c/deep.txt"],
        msg="only the carried nested dirty delete is pending",
    )
    assert_entry(
        entries,
        "a/b/c/deep.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty delete must survive branch reset",
    )
    assert not os.path.exists(repo._fix_path("a/b/c/deep.txt")), (
        "carried nested delete stays removed"
    )


# ---------------------------------------------------------------------------
# branch reset (repo.branch_reset) refuses when there is a staged state
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_branchreset_refuses_staged_modify_flat(new_lore_repo):
    """`branch reset` refuses with the staged-state guard when a staged modify
    is present; the staged modify survives the rejected reset."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(
        repo, {"base.txt": "base v1\n", "mod.txt": "mod original\n"}
    )

    with repo.open_file("mod.txt", "w+") as f:
        f.write("mod modified longer\n")
    repo.stage("mod.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.branch_reset(rev_v1, offline=True)
    assert "Unable to reset branch when there is a staged state" in str(
        excinfo.value
    ), f"reset should refuse on a staged modify, got: {excinfo.value}"

    after = get_status_files(repo)
    assert_entry(
        after,
        "mod.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged modify must survive the rejected branch reset",
    )


@pytest.mark.smoke
def test_branchreset_refuses_staged_modify_nested(new_lore_repo):
    """`branch reset` refuses with the staged-state guard when a deeply nested
    staged modify is present; the staged modify survives the rejected reset."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(
        repo, {"base.txt": "base v1\n", "a/b/c/deep.txt": "deep original\n"}
    )

    with repo.open_file("a/b/c/deep.txt", "w+") as f:
        f.write("deep modified longer\n")
    repo.stage("a/b/c/deep.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.branch_reset(rev_v1, offline=True)
    assert "Unable to reset branch when there is a staged state" in str(
        excinfo.value
    ), f"reset should refuse on a nested staged modify, got: {excinfo.value}"

    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/c/deep.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged modify must survive the rejected branch reset",
    )


@pytest.mark.smoke
def test_branchreset_refuses_staged_delete_flat(new_lore_repo):
    """`branch reset` refuses with the staged-state guard when a staged delete
    is present; the staged delete survives the rejected reset."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(
        repo, {"base.txt": "base v1\n", "victim.txt": "precious\n"}
    )

    repo.remove_file("victim.txt")
    repo.stage("victim.txt", scan=True, offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.branch_reset(rev_v1, offline=True)
    assert "Unable to reset branch when there is a staged state" in str(
        excinfo.value
    ), f"reset should refuse on a staged delete, got: {excinfo.value}"

    after = get_status_files(repo)
    assert_entry(
        after,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged delete must survive the rejected branch reset",
    )


@pytest.mark.smoke
def test_branchreset_refuses_staged_delete_nested(new_lore_repo):
    """`branch reset` refuses with the staged-state guard when a deeply nested
    staged delete is present; the staged delete survives the rejected reset."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    rev_v1 = _commit_two_revs(
        repo, {"base.txt": "base v1\n", "a/b/c/deep.txt": "deep precious\n"}
    )

    repo.remove_file("a/b/c/deep.txt")
    repo.stage("a/b/c/deep.txt", scan=True, offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.branch_reset(rev_v1, offline=True)
    assert "Unable to reset branch when there is a staged state" in str(
        excinfo.value
    ), f"reset should refuse on a nested staged delete, got: {excinfo.value}"

    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/c/deep.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged delete must survive the rejected branch reset",
    )


# ---------------------------------------------------------------------------
# move / copy: commit of a staged move/copy; reset of a dirty move/copy
# ---------------------------------------------------------------------------


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_commit_staged_move(new_lore_repo):
    """Committing a staged move (rename on disk + dirty_move + stage) records
    the rename in the sealed tree: the destination is present, the source is
    gone, and status is clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"old.txt": "movable content\n", "anchor.txt": "anchor\n"})

    os.rename(repo._fix_path("old.txt"), repo._fix_path("new.txt"))
    repo.dirty_move("old.txt", "new.txt", offline=True)
    repo.stage("new.txt", offline=True)

    staged = get_status_files(repo)
    assert_entry(
        staged,
        "new.txt",
        action="move",
        dirty=True,
        staged=True,
        node_type="file",
        from_path="old.txt",
    )

    repo.commit("commit staged move", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status must be clean after committing the move, got {summarize(entries)}"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "new.txt" in dump, f"move destination should be in the sealed tree:\n{dump}"
    assert "old.txt" not in dump, (
        f"move source should be gone from the sealed tree:\n{dump}"
    )


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_commit_staged_copy(new_lore_repo):
    """Committing a staged copy (duplicate on disk + dirty_copy + stage) records
    both the source and the destination in the sealed tree, and status is
    clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"orig.txt": "source content\n"})

    with repo.open_file("copy.txt", "w+") as f:
        f.write("source content\n")
    repo.dirty_copy("orig.txt", "copy.txt", offline=True)
    repo.stage("copy.txt", offline=True)

    staged = get_status_files(repo)
    assert_entry(
        staged,
        "copy.txt",
        action="copy",
        dirty=True,
        staged=True,
        node_type="file",
        from_path="orig.txt",
    )

    repo.commit("commit staged copy", offline=True)

    entries = get_status_files(repo)
    assert entries == [], (
        f"status must be clean after committing the copy, got {summarize(entries)}"
    )

    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    assert "copy.txt" in dump, f"copy destination should be in the sealed tree:\n{dump}"
    assert "orig.txt" in dump, f"copy source should remain in the sealed tree:\n{dump}"


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_reset_dirty_move(new_lore_repo):
    """Resetting a dirty move (action=move/fromPath=source) restores the source
    on disk, removes the destination, and leaves a clean status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"old.txt": "movable content\n"})

    os.rename(repo._fix_path("old.txt"), repo._fix_path("new.txt"))
    repo.dirty_move("old.txt", "new.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(
        before, "new.txt", action="move", dirty=True, staged=False, from_path="old.txt"
    )

    repo.reset("new.txt", offline=True)

    assert os.path.exists(repo._fix_path("old.txt")), (
        "reset must restore the move source"
    )
    with repo.open_file("old.txt", "r") as f:
        assert f.read() == "movable content\n", (
            "restored source content must match committed"
        )

    no_scan = get_status_files(repo)
    assert_file_set(no_scan, [], msg="status clean after resetting the dirty move")
    scanned = get_status_files_twice(repo, scan=True)
    assert_file_set(scanned, [], msg="--scan agrees the tree is clean after move reset")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_reset_dirty_copy(new_lore_repo):
    """Resetting a dirty copy (action=copy/fromPath=source) clears the copy's
    tracking and leaves the source unchanged with a clean status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"orig.txt": "source content\n"})

    with repo.open_file("copy.txt", "w+") as f:
        f.write("source content\n")
    repo.dirty_copy("orig.txt", "copy.txt", offline=True)

    before = get_status_files(repo)
    assert_entry(
        before,
        "copy.txt",
        action="copy",
        dirty=True,
        staged=False,
        from_path="orig.txt",
    )

    repo.reset("copy.txt", offline=True)

    with repo.open_file("orig.txt", "r") as f:
        assert f.read() == "source content\n", (
            "copy source must be unchanged after reset"
        )

    no_scan = get_status_files(repo)
    assert_absent(no_scan, "copy.txt", msg="reset must clear the dirty copy's tracking")
    assert_file_set(no_scan, [], msg="status clean after resetting the dirty copy")


def _node_addr_in_dump(dump: str, name: str) -> str | None:
    """Return the addr field of the dump line for an exact node name, or None.
    Dump lines look like: '<name> id N parent .. addr <hash>-<id>'."""
    for raw in dump.splitlines():
        parts = raw.strip().split()
        if not parts or parts[0] != name:
            continue
        if "addr" in parts:
            return parts[parts.index("addr") + 1]
        return None
    return None


# ===========================================================================
# sync REFUSES on a staged state (add / modify / delete x flat / nested)
# ===========================================================================


@pytest.mark.smoke
def test_sync_refuses_staged_add_flat(new_lore_repo):
    """Sync to an earlier revision refuses while a flat add is staged: it raises
    "Unable to sync when there is a staged state", the stage survives, and the
    working tree stays on the current revision's content."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 longer content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged add\n")
    repo.stage("staged.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to sync when there is a staged state"
    ):
        repo.sync(rev_v1, offline=True)

    after = get_status_files(repo)
    assert_entry(
        after,
        "staged.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged add must survive a refused sync",
    )
    assert_file_set(
        after, ["staged.txt"], msg="only the staged add is pending after refusal"
    )
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v2 longer content\n", (
            "refused sync must not move the working tree"
        )


@pytest.mark.smoke
def test_sync_refuses_staged_modify_flat(new_lore_repo):
    """Sync to an earlier revision refuses while a flat modify is staged: it
    raises "Unable to sync when there is a staged state", the staged modify
    survives, and the working tree stays at the current revision."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"mod.txt": "v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("mod.txt", "w+") as f:
        f.write("v2 longer content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("mod.txt", "w+") as f:
        f.write("staged edit longer\n")
    repo.stage("mod.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to sync when there is a staged state"
    ):
        repo.sync(rev_v1, offline=True)

    after = get_status_files(repo)
    assert_entry(
        after,
        "mod.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged modify must survive a refused sync",
    )
    assert_file_set(
        after, ["mod.txt"], msg="only the staged modify is pending after refusal"
    )
    with repo.open_file("mod.txt", "r") as f:
        assert f.read() == "staged edit longer\n", (
            "staged content stays on disk after refusal"
        )


@pytest.mark.smoke
def test_sync_refuses_staged_delete_flat(new_lore_repo):
    """Sync to an earlier revision refuses while a flat delete is staged: it
    raises "Unable to sync when there is a staged state", the staged delete
    survives, and the deleted file stays removed."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"gone.txt": "v1\n", "keep.txt": "keep\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("keep.txt", "w+") as f:
        f.write("v2 longer content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    repo.remove_file("gone.txt")
    repo.dirty("gone.txt", offline=True)
    repo.stage("gone.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to sync when there is a staged state"
    ):
        repo.sync(rev_v1, offline=True)

    after = get_status_files(repo)
    assert_entry(
        after,
        "gone.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged delete must survive a refused sync",
    )
    assert_file_set(
        after, ["gone.txt"], msg="only the staged delete is pending after refusal"
    )
    assert not os.path.exists(repo._fix_path("gone.txt")), (
        "staged-deleted file stays removed"
    )


@pytest.mark.smoke
def test_sync_refuses_staged_add_nested(new_lore_repo):
    """Sync to an earlier revision refuses while a nested add is staged: it
    raises "Unable to sync when there is a staged state", the staged nested add
    survives, and the working tree stays at the current revision."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 longer content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    repo.make_dirs("nested/sub")
    with repo.open_file("nested/sub/leaf.txt", "w+") as f:
        f.write("nested staged add\n")
    repo.stage("nested/sub/leaf.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to sync when there is a staged state"
    ):
        repo.sync(rev_v1, offline=True)

    after = get_status_files(repo)
    assert_entry(
        after,
        "nested/sub/leaf.txt",
        action="add",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged add must survive a refused sync",
    )
    assert_file_set(
        after, ["nested/sub/leaf.txt"], msg="only the nested staged add is pending"
    )
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v2 longer content\n", (
            "refused sync must not move the working tree"
        )


@pytest.mark.smoke
def test_sync_refuses_staged_modify_nested(new_lore_repo):
    """Sync to an earlier revision refuses while a nested modify is staged: it
    raises "Unable to sync when there is a staged state", the staged nested
    modify survives, and its edited content stays on disk."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/deep.txt": "v1\n", "top.txt": "top\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("top.txt", "w+") as f:
        f.write("top v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("a/b/deep.txt", "w+") as f:
        f.write("deep staged edit longer\n")
    repo.stage("a/b/deep.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to sync when there is a staged state"
    ):
        repo.sync(rev_v1, offline=True)

    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/deep.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged modify must survive a refused sync",
    )
    assert_file_set(
        after, ["a/b/deep.txt"], msg="only the nested staged modify is pending"
    )
    with repo.open_file("a/b/deep.txt", "r") as f:
        assert f.read() == "deep staged edit longer\n", (
            "staged content stays on disk after refusal"
        )


@pytest.mark.smoke
def test_sync_refuses_staged_delete_nested(new_lore_repo):
    """Sync to an earlier revision refuses while a nested delete is staged: it
    raises "Unable to sync when there is a staged state", the staged nested
    delete survives, and the deleted leaf stays removed."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/gone.txt": "v1\n", "top.txt": "top\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("top.txt", "w+") as f:
        f.write("top v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    repo.remove_file("a/b/gone.txt")
    repo.dirty("a/b/gone.txt", offline=True)
    repo.stage("a/b/gone.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to sync when there is a staged state"
    ):
        repo.sync(rev_v1, offline=True)

    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/gone.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged delete must survive a refused sync",
    )
    assert_file_set(
        after, ["a/b/gone.txt"], msg="only the nested staged delete is pending"
    )
    assert not os.path.exists(repo._fix_path("a/b/gone.txt")), (
        "staged-deleted leaf stays removed"
    )


# ===========================================================================
# switch REFUSES on a staged state (modify / delete x flat / nested)
# ===========================================================================


@pytest.mark.smoke
def test_switch_refuses_staged_modify_flat(new_lore_repo):
    """A same-revision branch switch refuses while a flat modify is staged: it
    raises "Unable to switch branch when there is a staged state", the repo
    stays on the original branch, and the staged modify survives."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"mod.txt": "original\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("mod.txt", "w+") as f:
        f.write("staged edit longer\n")
    repo.stage("mod.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to switch branch when there is a staged state"
    ):
        repo.branch_switch("other", offline=True)

    assert "On branch main" in repo.status(offline=True), (
        "a refused switch must leave the repo on the original branch"
    )
    after = get_status_files(repo)
    assert_entry(
        after,
        "mod.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged modify must survive a refused switch",
    )
    assert_file_set(
        after, ["mod.txt"], msg="only the staged modify is pending after refusal"
    )
    with repo.open_file("mod.txt", "r") as f:
        assert f.read() == "staged edit longer\n", (
            "staged content stays on disk after refusal"
        )


@pytest.mark.smoke
def test_switch_refuses_staged_delete_flat(new_lore_repo):
    """A same-revision branch switch refuses while a flat delete is staged: it
    raises "Unable to switch branch when there is a staged state", the repo
    stays on the original branch, and the staged delete survives."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"gone.txt": "gone content\n", "keep.txt": "keep\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    repo.remove_file("gone.txt")
    repo.dirty("gone.txt", offline=True)
    repo.stage("gone.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to switch branch when there is a staged state"
    ):
        repo.branch_switch("other", offline=True)

    assert "On branch main" in repo.status(offline=True), (
        "a refused switch must leave the repo on the original branch"
    )
    after = get_status_files(repo)
    assert_entry(
        after,
        "gone.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged delete must survive a refused switch",
    )
    assert_file_set(
        after, ["gone.txt"], msg="only the staged delete is pending after refusal"
    )
    assert not os.path.exists(repo._fix_path("gone.txt")), (
        "staged-deleted file stays removed"
    )


@pytest.mark.smoke
def test_switch_refuses_staged_modify_nested(new_lore_repo):
    """A same-revision branch switch refuses while a nested modify is staged: it
    raises "Unable to switch branch when there is a staged state", the repo
    stays on the original branch, and the staged nested modify survives."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/deep.txt": "deep original\n", "top.txt": "top\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("a/b/deep.txt", "w+") as f:
        f.write("deep staged edit longer\n")
    repo.stage("a/b/deep.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to switch branch when there is a staged state"
    ):
        repo.branch_switch("other", offline=True)

    assert "On branch main" in repo.status(offline=True), (
        "a refused switch must leave the repo on the original branch"
    )
    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/deep.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged modify must survive a refused switch",
    )
    assert_file_set(
        after, ["a/b/deep.txt"], msg="only the nested staged modify is pending"
    )
    with repo.open_file("a/b/deep.txt", "r") as f:
        assert f.read() == "deep staged edit longer\n", (
            "staged content stays on disk after refusal"
        )


@pytest.mark.smoke
def test_switch_refuses_staged_delete_nested(new_lore_repo):
    """A same-revision branch switch refuses while a nested delete is staged: it
    raises "Unable to switch branch when there is a staged state", the repo
    stays on the original branch, and the staged nested delete survives."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/gone.txt": "deep gone\n", "top.txt": "top\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    repo.remove_file("a/b/gone.txt")
    repo.dirty("a/b/gone.txt", offline=True)
    repo.stage("a/b/gone.txt", offline=True)

    with pytest.raises(
        LoreException, match="Unable to switch branch when there is a staged state"
    ):
        repo.branch_switch("other", offline=True)

    assert "On branch main" in repo.status(offline=True), (
        "a refused switch must leave the repo on the original branch"
    )
    after = get_status_files(repo)
    assert_entry(
        after,
        "a/b/gone.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="nested staged delete must survive a refused switch",
    )
    assert_file_set(
        after, ["a/b/gone.txt"], msg="only the nested staged delete is pending"
    )
    assert not os.path.exists(repo._fix_path("a/b/gone.txt")), (
        "staged-deleted leaf stays removed"
    )


# ===========================================================================
# sync carries a dirty add / delete across back+forward syncs (flat / nested)
# ===========================================================================


@pytest.mark.smoke
def test_sync_carries_dirty_add_flat(new_lore_repo):
    """A genuine pending dirty ADD is carried across a sync back to an earlier
    revision and back forward again: the added file stays on disk with its
    content and is reported action=add/flagDirty at both endpoints, while a
    committed file follows the synced revision."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"file.txt": "v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 longer content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("added.txt", "w+") as f:
        f.write("dirty add content\n")
    repo.dirty("added.txt", offline=True)

    repo.sync(rev_v1, offline=True)
    assert os.path.exists(repo._fix_path("added.txt")), (
        "dirty add carried onto the v1 base"
    )
    with repo.open_file("added.txt", "r") as f:
        assert f.read() == "dirty add content\n", (
            "carried add keeps its content over v1"
        )
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v1\n", (
            "non-dirty committed file follows the synced revision"
        )
    back = get_status_files_twice(repo)
    assert_entry(
        back, "added.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(back, ["added.txt"], msg="only the carried add is pending at v1")

    repo.sync(offline=True)
    assert os.path.exists(repo._fix_path("added.txt")), (
        "dirty add still present after sync forward"
    )
    with repo.open_file("added.txt", "r") as f:
        assert f.read() == "dirty add content\n", (
            "carried add survives the forward sync"
        )
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v2 longer content\n", (
            "committed file restored to v2 forward"
        )
    fwd = get_status_files_twice(repo)
    assert_entry(
        fwd, "added.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(fwd, ["added.txt"], msg="only the carried add is pending at v2")


@pytest.mark.smoke
def test_sync_carries_dirty_delete_flat(new_lore_repo):
    """A genuine pending dirty DELETE is carried across a sync back to an earlier
    revision and forward again: the deleted file stays removed and is reported
    action=delete/flagDirty at both endpoints, while a committed file follows
    the synced revision."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"victim.txt": "victim v1\n", "other.txt": "other v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("other.txt", "w+") as f:
        f.write("other v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)

    repo.sync(rev_v1, offline=True)
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "dirty delete carried onto v1"
    )
    with repo.open_file("other.txt", "r") as f:
        assert f.read() == "other v1\n", "non-dirty file follows the synced revision"
    back = get_status_files_twice(repo)
    assert_entry(
        back, "victim.txt", action="delete", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(
        back, ["victim.txt"], msg="only the carried delete is pending at v1"
    )

    repo.sync(offline=True)
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "dirty delete still applied at v2"
    )
    with repo.open_file("other.txt", "r") as f:
        assert f.read() == "other v2 longer\n", "committed file restored to v2 forward"
    fwd = get_status_files_twice(repo)
    assert_entry(
        fwd, "victim.txt", action="delete", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(fwd, ["victim.txt"], msg="only the carried delete is pending at v2")


@pytest.mark.smoke
def test_sync_carries_dirty_add_nested(new_lore_repo):
    """A genuine pending dirty ADD inside an existing nested directory is carried
    across a sync back and forward: the nested file stays on disk and is
    reported action=add/flagDirty at both endpoints."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/keep.txt": "keep v1\n", "top.txt": "top\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("top.txt", "w+") as f:
        f.write("top v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("a/b/added.txt", "w+") as f:
        f.write("nested dirty add\n")
    repo.dirty("a/b/added.txt", offline=True)

    repo.sync(rev_v1, offline=True)
    assert os.path.exists(repo._fix_path("a/b/added.txt")), (
        "nested dirty add carried onto v1"
    )
    with repo.open_file("a/b/added.txt", "r") as f:
        assert f.read() == "nested dirty add\n", "carried nested add keeps its content"
    back = get_status_files_twice(repo)
    assert_entry(
        back, "a/b/added.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(
        back, ["a/b/added.txt"], msg="only the nested carried add is pending at v1"
    )

    repo.sync(offline=True)
    assert os.path.exists(repo._fix_path("a/b/added.txt")), (
        "nested dirty add survives forward sync"
    )
    fwd = get_status_files_twice(repo)
    assert_entry(
        fwd, "a/b/added.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(
        fwd, ["a/b/added.txt"], msg="only the nested carried add is pending at v2"
    )


@pytest.mark.smoke
def test_sync_carries_dirty_delete_nested(new_lore_repo):
    """A genuine pending dirty DELETE of a nested leaf is carried across a sync
    back and forward: the nested leaf stays removed and is reported
    action=delete/flagDirty at both endpoints."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"a/b/gone.txt": "gone v1\n", "top.txt": "top\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("top.txt", "w+") as f:
        f.write("top v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    repo.remove_file("a/b/gone.txt")
    repo.dirty("a/b/gone.txt", offline=True)

    repo.sync(rev_v1, offline=True)
    assert not os.path.exists(repo._fix_path("a/b/gone.txt")), (
        "nested dirty delete carried onto v1"
    )
    back = get_status_files_twice(repo)
    assert_entry(
        back,
        "a/b/gone.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
    )
    assert_file_set(
        back, ["a/b/gone.txt"], msg="only the nested carried delete is pending at v1"
    )

    repo.sync(offline=True)
    assert not os.path.exists(repo._fix_path("a/b/gone.txt")), (
        "nested dirty delete still applied at v2"
    )
    fwd = get_status_files_twice(repo)
    assert_entry(
        fwd, "a/b/gone.txt", action="delete", dirty=True, staged=False, node_type="file"
    )
    assert_file_set(
        fwd, ["a/b/gone.txt"], msg="only the nested carried delete is pending at v2"
    )


@pytest.mark.smoke
def test_sync_carries_all_dirty_classes(new_lore_repo):
    """Sync to an earlier revision carries a full dirty set together — a modify
    (action=keep), an add (action=add) and a delete (action=delete) all remain
    flagDirty on the synced base with their on-disk content preserved, while an
    untouched committed file follows the synced revision."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {
            "mod.txt": "mod v1\n",
            "del.txt": "del v1\n",
            "stay.txt": "stay v1\n",
            "drv.txt": "drv v1\n",
        },
    )
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("drv.txt", "w+") as f:
        f.write("drv v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("mod.txt", "w+") as f:
        f.write("mod locally edited\n")
    with repo.open_file("added.txt", "w+") as f:
        f.write("added content\n")
    repo.remove_file("del.txt")
    repo.dirty(["mod.txt", "added.txt", "del.txt"], offline=True)

    repo.sync(rev_v1, offline=True)

    entries = get_status_files_twice(repo)
    assert_entry(
        entries, "mod.txt", action="keep", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        entries, "added.txt", action="add", dirty=True, staged=False, node_type="file"
    )
    assert_entry(
        entries, "del.txt", action="delete", dirty=True, staged=False, node_type="file"
    )
    assert_absent(entries, "stay.txt", msg="untouched committed file stays clean")
    assert_file_set(
        entries,
        ["mod.txt", "added.txt", "del.txt"],
        msg="the whole dirty set carries together across sync",
    )

    with repo.open_file("mod.txt", "r") as f:
        assert f.read() == "mod locally edited\n", (
            "dirty modify keeps local content over v1"
        )
    with repo.open_file("added.txt", "r") as f:
        assert f.read() == "added content\n", "dirty add keeps local content over v1"
    assert not os.path.exists(repo._fix_path("del.txt")), (
        "dirty delete stays deleted over v1"
    )
    with repo.open_file("drv.txt", "r") as f:
        assert f.read() == "drv v1\n", (
            "non-dirty committed file follows the synced revision"
        )


# ===========================================================================
# switch carries a nested dirty add+modify
# ===========================================================================


@pytest.mark.smoke
def test_switch_carries_nested_dirty_add_and_modify(new_lore_repo):
    """A same-revision branch switch carries a dirty ADD and a dirty MODIFY that
    both live inside an existing nested directory: the add (action=add) and the
    modify (action=keep) remain flagDirty afterward with their on-disk content
    preserved, while an untouched nested sibling stays clean."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo,
        {"a/b/mod.txt": "mod original\n", "a/b/keep.txt": "keep original\n"},
    )

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("a/b/mod.txt", "w+") as f:
        f.write("mod locally edited\n")
    with repo.open_file("a/b/added.txt", "w+") as f:
        f.write("nested added content\n")
    repo.dirty(["a/b/mod.txt", "a/b/added.txt"], offline=True)

    pre = get_status_files(repo)
    assert_file_set(
        pre, ["a/b/mod.txt", "a/b/added.txt"], msg="nested dirty set before switch"
    )

    repo.branch_switch("other", offline=True)

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "a/b/mod.txt",
        action="keep",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty modify carried across the switch",
    )
    assert_entry(
        entries,
        "a/b/added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty add carried across the switch",
    )
    assert_absent(entries, "a/b/keep.txt", msg="untouched nested sibling stays clean")
    assert_file_set(
        entries,
        ["a/b/mod.txt", "a/b/added.txt"],
        msg="the nested dirty add+modify carry across the switch",
    )

    with repo.open_file("a/b/mod.txt", "r") as f:
        assert f.read() == "mod locally edited\n", (
            "nested dirty modify keeps local content"
        )
    with repo.open_file("a/b/added.txt", "r") as f:
        assert f.read() == "nested added content\n", (
            "nested dirty add keeps local content"
        )


# ===========================================================================
# MOVE / COPY carried across switch / sync (intended provenance)
# ===========================================================================


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_sync_carries_dirty_move(new_lore_repo):
    """A dirty MOVE is carried across a sync back to an earlier revision: the
    destination is still reported action=move/fromPath=source, the on-disk
    rename is intact, and a committed file follows the synced revision."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"old.txt": "movable content\n", "other.txt": "other v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("other.txt", "w+") as f:
        f.write("other v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    os.rename(repo._fix_path("old.txt"), repo._fix_path("new.txt"))
    repo.dirty_move("old.txt", "new.txt", offline=True)

    repo.sync(rev_v1, offline=True)

    assert not os.path.exists(repo._fix_path("old.txt")), (
        "move source gone on disk after sync"
    )
    with repo.open_file("new.txt", "r") as f:
        assert f.read() == "movable content\n", (
            "move destination intact on disk after sync"
        )

    entries = get_status_files_twice(repo)
    assert_entry(
        entries,
        "new.txt",
        action="move",
        dirty=True,
        staged=False,
        from_path="old.txt",
        msg="dirty move provenance is carried across sync",
    )
    assert_absent(entries, "old.txt", msg="move source must not appear after sync")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_sync_carries_dirty_copy(new_lore_repo):
    """A dirty COPY is carried across a sync back to an earlier revision: the
    destination is still reported action=copy/fromPath=source, the source stays
    clean, and a committed file follows the synced revision."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"orig.txt": "source content\n", "other.txt": "other v1\n"})
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("other.txt", "w+") as f:
        f.write("other v2 longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("copy.txt", "w+") as f:
        f.write("source content\n")
    repo.dirty_copy("orig.txt", "copy.txt", offline=True)

    repo.sync(rev_v1, offline=True)

    with repo.open_file("copy.txt", "r") as f:
        assert f.read() == "source content\n", (
            "copy destination intact on disk after sync"
        )

    entries = get_status_files_twice(repo)
    assert_entry(
        entries,
        "copy.txt",
        action="copy",
        dirty=True,
        staged=False,
        from_path="orig.txt",
        msg="dirty copy provenance is carried across sync",
    )
    assert_absent(entries, "orig.txt", msg="copy source stays clean after sync")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_switch_carries_dirty_copy(new_lore_repo):
    """A dirty COPY is carried across a same-revision branch switch: the
    destination is still reported action=copy/fromPath=source and the source
    stays clean, with both files present on disk."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"orig.txt": "source content\n", "anchor.txt": "anchor\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("copy.txt", "w+") as f:
        f.write("source content\n")
    repo.dirty_copy("orig.txt", "copy.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(
        pre, "copy.txt", action="copy", dirty=True, staged=False, from_path="orig.txt"
    )

    repo.branch_switch("other", offline=True)

    with repo.open_file("orig.txt", "r") as f:
        assert f.read() == "source content\n", "copy source intact on disk after switch"
    with repo.open_file("copy.txt", "r") as f:
        assert f.read() == "source content\n", (
            "copy destination intact on disk after switch"
        )

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "copy.txt",
        action="copy",
        dirty=True,
        staged=False,
        from_path="orig.txt",
        msg="dirty copy provenance is carried across a same-revision switch",
    )
    assert_absent(entries, "orig.txt", msg="copy source stays clean after switch")


# ===========================================================================
# branch merge / cherry-pick / revert: dirty carry across change types
# ===========================================================================
#
# Each op refuses on a staged MODIFY and a staged DELETE (flat and nested),
# the stage surviving the refusal; carries a dirty delete through a clean op
# in a NESTED directory; carries a dirty add in a brand-new nested dir and a
# dirty delete through a CONFLICT resolved with THEIRS; and carries a dirty
# move through each op.


def _commit_two_revs_flat(repo: Lore) -> str:
    """Base + a v2 add on the current branch; return the v2 revision signature.
    Used by revert refusal tests that need a revision to revert."""
    commit_base(repo, {"base.txt": "base v1\n"})
    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 add revertable", offline=True)
    return repo.revision_history(1, offline=True)[0].signature


def _feature_branch_with_add(repo: Lore) -> str:
    """From a base on main, create a `feature` branch that adds feat.txt, then
    return to main. Return the feature head signature (also usable as a
    cherry-pick source)."""
    repo.branch_create("feature", offline=True)
    with repo.open_file("feat.txt", "w+") as f:
        f.write("feature add\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature add feat.txt", offline=True)
    rev = repo.revision_history(1, offline=True)[0].signature
    repo.branch_switch("main", offline=True)
    return rev


# ---------------------------------------------------------------------------
# Each op refuses on a staged MODIFY / staged DELETE, flat and nested, with a
# tracked modify/delete as the staged node. The stage must survive the refusal.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_refuses_staged_modify_flat(new_lore_repo):
    """branch merge refuses to start when a staged MODIFY of a committed flat
    file exists, raising "Cannot merge with staged state"; the staged modify
    survives the refusal and the feature add does not land."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n", "tracked.txt": "tracked v1\n"})
    feat_rev = _feature_branch_with_add(repo)
    assert len(feat_rev) == 64

    with repo.open_file("tracked.txt", "w+") as f:
        f.write("tracked modified longer\n")
    repo.stage("tracked.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.branch_merge("feature", offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"merge must refuse on a staged modify, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_file_set(entries, ["tracked.txt"], msg="only the staged modify remains")
    assert_entry(
        entries,
        "tracked.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged modify must survive the refused merge",
    )
    assert_absent(
        entries, "feat.txt", msg="feature add must not land after a refused merge"
    )


@pytest.mark.smoke
def test_merge_refuses_staged_delete_nested(new_lore_repo):
    """branch merge refuses to start when a staged DELETE of a committed file in
    a nested directory exists, raising "Cannot merge with staged state"; the
    staged delete survives the refusal."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n", "pkg/sub/gone.txt": "gone v1\n"})
    feat_rev = _feature_branch_with_add(repo)
    assert len(feat_rev) == 64

    repo.remove_file("pkg/sub/gone.txt")
    repo.dirty("pkg/sub/gone.txt", offline=True)
    repo.stage("pkg/sub/gone.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.branch_merge("feature", offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"merge must refuse on a staged nested delete, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_file_set(entries, ["pkg/sub/gone.txt"], msg="only the staged delete remains")
    assert_entry(
        entries,
        "pkg/sub/gone.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged nested delete must survive the refused merge",
    )
    assert_absent(
        entries, "feat.txt", msg="feature add must not land after a refused merge"
    )


@pytest.mark.smoke
def test_cherrypick_refuses_staged_modify_nested(new_lore_repo):
    """revision cherry-pick refuses to start when a staged MODIFY of a committed
    file in a nested directory exists, raising "Cannot merge with staged state";
    the staged modify survives the refusal."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"base.txt": "base original\n", "pkg/sub/tracked.txt": "tracked v1\n"}
    )
    source_rev = _feature_branch_with_add(repo)

    with repo.open_file("pkg/sub/tracked.txt", "w+") as f:
        f.write("tracked modified longer\n")
    repo.stage("pkg/sub/tracked.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.revision_cherry_pick(source_rev, offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"cherry-pick must refuse on a staged nested modify, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["pkg/sub/tracked.txt"], msg="only the staged modify remains"
    )
    assert_entry(
        entries,
        "pkg/sub/tracked.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged nested modify must survive the refused cherry-pick",
    )
    assert_absent(entries, "feat.txt", msg="nothing from the source was applied")


@pytest.mark.smoke
def test_cherrypick_refuses_staged_delete_flat(new_lore_repo):
    """revision cherry-pick refuses to start when a staged DELETE of a committed
    flat file exists, raising "Cannot merge with staged state"; the staged
    delete survives the refusal."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n", "gone.txt": "gone v1\n"})
    source_rev = _feature_branch_with_add(repo)

    repo.remove_file("gone.txt")
    repo.dirty("gone.txt", offline=True)
    repo.stage("gone.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.revision_cherry_pick(source_rev, offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"cherry-pick must refuse on a staged delete, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_file_set(entries, ["gone.txt"], msg="only the staged delete remains")
    assert_entry(
        entries,
        "gone.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged delete must survive the refused cherry-pick",
    )
    assert_absent(entries, "feat.txt", msg="nothing from the source was applied")


@pytest.mark.smoke
def test_revert_refuses_staged_modify_flat(new_lore_repo):
    """revision revert refuses to start when a staged MODIFY of a committed flat
    file exists, raising "Cannot merge with staged state"; the staged modify
    survives the refusal and the target revision stays present."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    rev_v2 = _commit_two_revs_flat(repo)

    with repo.open_file("base.txt", "w+") as f:
        f.write("base modified longer\n")
    repo.stage("base.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.revision_revert(rev_v2, offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"revert must refuse on a staged modify, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_file_set(entries, ["base.txt"], msg="only the staged modify remains")
    assert_entry(
        entries,
        "base.txt",
        action="keep",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged modify must survive the refused revert",
    )
    assert os.path.exists(repo._fix_path("revertable.txt")), (
        "the revert was rejected so revertable.txt is still present"
    )


@pytest.mark.smoke
def test_revert_refuses_staged_delete_nested(new_lore_repo):
    """revision revert refuses to start when a staged DELETE of a committed file
    in a nested directory exists, raising "Cannot merge with staged state"; the
    staged delete survives the refusal and the target revision stays present."""
    from error_types import LoreException

    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base v1\n", "pkg/sub/gone.txt": "gone v1\n"})
    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 add revertable", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    repo.remove_file("pkg/sub/gone.txt")
    repo.dirty("pkg/sub/gone.txt", offline=True)
    repo.stage("pkg/sub/gone.txt", offline=True)

    with pytest.raises(LoreException) as excinfo:
        repo.revision_revert(rev_v2, offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"revert must refuse on a staged nested delete, got:\n{excinfo.value}"
    )

    entries = get_status_files(repo)
    assert_file_set(entries, ["pkg/sub/gone.txt"], msg="only the staged delete remains")
    assert_entry(
        entries,
        "pkg/sub/gone.txt",
        action="delete",
        dirty=True,
        staged=True,
        node_type="file",
        msg="staged nested delete must survive the refused revert",
    )
    assert os.path.exists(repo._fix_path("revertable.txt")), (
        "the revert was rejected so revertable.txt is still present"
    )


# ---------------------------------------------------------------------------
# dirty DELETE in a NESTED directory carried through a CLEAN op: carry replay
# against a deeper node.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_carry_nested_delete_clean(new_lore_repo):
    """A dirty DELETE of a committed file in a nested directory survives a clean
    branch merge: the feature add lands clean and the nested delete carry is
    still reported as a pending action=delete/flagDirty after the auto-commit."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"base.txt": "base original\n", "pkg/sub/victim.txt": "delete me\n"}
    )
    feat_rev = _feature_branch_with_add(repo)
    assert len(feat_rev) == 64

    repo.remove_file("pkg/sub/victim.txt")
    repo.dirty("pkg/sub/victim.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "pkg/sub/victim.txt", action="delete", dirty=True, staged=False)

    repo.branch_merge("feature", offline=True)

    with repo.open_file("feat.txt", "r") as f:
        assert f.read() == "feature add\n", "feature add must land on disk"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["pkg/sub/victim.txt"], msg="only the nested delete carry remains"
    )
    assert_entry(
        entries,
        "pkg/sub/victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty delete carry survives the clean merge",
    )
    assert_absent(entries, "feat.txt", msg="feature add is committed and clean")
    assert not os.path.exists(repo._fix_path("pkg/sub/victim.txt")), (
        "the carried dirty delete must keep the file absent on disk"
    )


@pytest.mark.smoke
def test_cherrypick_carry_nested_delete_clean(new_lore_repo):
    """A dirty DELETE of a committed file in a nested directory survives a clean
    cherry-pick: the picked file lands clean and the nested delete carry is still
    reported as a pending action=delete/flagDirty after the pick commit."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n", "pkg/sub/victim.txt": "delete me\n"})
    source_rev = _feature_branch_with_add(repo)

    repo.remove_file("pkg/sub/victim.txt")
    repo.dirty("pkg/sub/victim.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "pkg/sub/victim.txt", action="delete", dirty=True, staged=False)

    repo.revision_cherry_pick(source_rev, offline=True)

    assert os.path.exists(repo._fix_path("feat.txt"))
    entries = get_status_files(repo)
    assert_file_set(
        entries, ["pkg/sub/victim.txt"], msg="only the nested delete carry remains"
    )
    assert_entry(
        entries,
        "pkg/sub/victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty delete carry survives the clean pick",
    )
    assert_absent(entries, "feat.txt", msg="picked file is committed and clean")
    assert not os.path.exists(repo._fix_path("pkg/sub/victim.txt")), (
        "the carried dirty delete must keep the file absent on disk"
    )


@pytest.mark.smoke
def test_revert_carry_nested_delete_clean(new_lore_repo):
    """A dirty DELETE of a committed file in a nested directory survives a clean
    revert of an add commit: the reverted add is removed and the nested delete
    carry is still reported as a pending action=delete/flagDirty after the revert
    auto-commit."""
    repo: Lore = new_lore_repo()
    commit_base(
        repo, {"base.txt": "base original\n", "pkg/sub/victim.txt": "delete me\n"}
    )

    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 add revertable", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    repo.remove_file("pkg/sub/victim.txt")
    repo.dirty("pkg/sub/victim.txt", offline=True)

    pre = get_status_files(repo)
    assert_entry(pre, "pkg/sub/victim.txt", action="delete", dirty=True, staged=False)

    repo.revision_revert(rev_v2, offline=True)

    assert not os.path.exists(repo._fix_path("revertable.txt")), (
        "reverted add removed from disk"
    )
    entries = get_status_files(repo)
    assert_absent(entries, "revertable.txt", msg="reverted add gone from status")
    assert_file_set(
        entries, ["pkg/sub/victim.txt"], msg="only the nested delete carry remains"
    )
    assert_entry(
        entries,
        "pkg/sub/victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty delete carry survives the clean revert",
    )
    assert not os.path.exists(repo._fix_path("pkg/sub/victim.txt")), (
        "the carried dirty delete must keep the file absent on disk"
    )


# ---------------------------------------------------------------------------
# CONFLICT resolved with THEIRS, carrying a dirty add-in-new-nested-dir and a
# dirty delete across the resolution.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_conflict_theirs_carry_nested_add(new_lore_repo):
    """A dirty ADD in a brand-new nested directory survives a conflicted merge
    resolved with THEIRS: the carry is replayed (recreating its directory nodes)
    after resolve + commit while the conflicted file takes feature's content."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edits conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    repo.make_dirs("new_dir/sub")
    with repo.open_file("new_dir/sub/added.txt", "w+") as f:
        f.write("brand new nested\n")
    repo.dirty("new_dir/sub/added.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected merge to surface a conflict, got:\n{merge_output}"
    )

    repo.branch_merge_resolve_theirs("conflict.txt", offline=True)
    repo.commit("merge resolved theirs", offline=True)

    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "feature\n", "resolve theirs must take feature's content"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["new_dir/sub/added.txt"], msg="only the dirty-add carry should remain"
    )
    assert_entry(
        entries,
        "new_dir/sub/added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty-add carry must survive a theirs resolve",
    )
    assert_entry(entries, "new_dir", node_type="directory", action="add")
    assert_entry(entries, "new_dir/sub", node_type="directory", action="add")
    assert_absent(entries, "conflict.txt", msg="resolved conflict is clean post-commit")


@pytest.mark.smoke
def test_merge_conflict_theirs_carry_delete(new_lore_repo):
    """A dirty DELETE of an unrelated committed file survives a conflicted merge
    resolved with THEIRS: after resolve + commit the conflicted file takes
    feature's content and the delete carry is still pending action=delete."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "victim.txt": "delete me\n"})

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edits conflict.txt", offline=True)

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected merge to surface a conflict, got:\n{merge_output}"
    )

    repo.branch_merge_resolve_theirs("conflict.txt", offline=True)
    repo.commit("merge resolved theirs", offline=True)

    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "feature\n", "resolve theirs must take feature's content"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["victim.txt"], msg="only the dirty-delete carry should remain"
    )
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-delete carry must survive a theirs resolve",
    )
    assert_absent(entries, "conflict.txt", msg="resolved conflict is clean post-commit")
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "the carried dirty delete must keep the file absent on disk"
    )


@pytest.mark.smoke
def test_cherrypick_conflict_theirs_carry_delete(new_lore_repo):
    """A dirty DELETE of an unrelated committed file survives a conflicted
    cherry-pick resolved with THEIRS: after resolve + commit the conflicted file
    takes the picked side and the delete carry is still pending action=delete."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"conflict.txt": "base\n", "victim.txt": "delete me\n"})

    repo.branch_create("source", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("source side\n")
    repo.stage(scan=True, offline=True)
    repo.commit("source edits conflict.txt", offline=True)
    source_rev = repo.revision_history(1, offline=True)[0].signature
    repo.branch_switch("main", offline=True)

    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main side\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    repo.remove_file("victim.txt")
    repo.dirty("victim.txt", offline=True)

    pick_output = repo.revision_cherry_pick(source_rev, offline=True)
    assert "conflicted" in pick_output, (
        f"expected cherry-pick to surface a conflict, got:\n{pick_output}"
    )

    repo.revision_cherry_pick_resolve_theirs("conflict.txt", offline=True)
    repo.commit("cherry-pick resolved theirs", offline=True)

    with repo.open_file("conflict.txt", "r") as f:
        assert f.read() == "source side\n", (
            "resolve theirs takes the picked-revision side"
        )

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["victim.txt"], msg="only the dirty-delete carry should remain"
    )
    assert_entry(
        entries,
        "victim.txt",
        action="delete",
        dirty=True,
        staged=False,
        node_type="file",
        msg="dirty-delete carry must survive a theirs resolve",
    )
    assert_absent(entries, "conflict.txt", msg="resolved conflict is clean post-commit")
    assert not os.path.exists(repo._fix_path("victim.txt")), (
        "the carried dirty delete must keep the file absent on disk"
    )


@pytest.mark.smoke
def test_revert_conflict_theirs_carry_nested_add(new_lore_repo):
    """A dirty ADD in a brand-new nested directory survives a conflicted revert
    resolved with THEIRS: after resolve + commit the conflicted file takes the
    revert result (v1) and the carry is replayed, recreating its dir nodes."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"target.txt": "v1\n"})

    with repo.open_file("target.txt", "w+") as f:
        f.write("v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 to be reverted", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("target.txt", "w+") as f:
        f.write("v3\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v3", offline=True)

    repo.make_dirs("new_dir/sub")
    with repo.open_file("new_dir/sub/added.txt", "w+") as f:
        f.write("brand new nested\n")
    repo.dirty("new_dir/sub/added.txt", offline=True)

    revert_output = repo.revision_revert(rev_v2, offline=True)
    assert "conflicted" in revert_output, (
        f"reverting v2 should conflict with v3, got:\n{revert_output}"
    )

    repo.revision_revert_resolve_theirs("target.txt", offline=True)
    repo.commit("revert resolved theirs", offline=True)

    with repo.open_file("target.txt", "r") as f:
        assert f.read() == "v1\n", "resolve theirs takes the revert result (v1)"

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["new_dir/sub/added.txt"], msg="only the dirty-add carry should remain"
    )
    assert_entry(
        entries,
        "new_dir/sub/added.txt",
        action="add",
        dirty=True,
        staged=False,
        node_type="file",
        msg="nested dirty-add carry must survive a theirs resolve",
    )
    assert_entry(entries, "new_dir", node_type="directory", action="add")
    assert_entry(entries, "new_dir/sub", node_type="directory", action="add")
    assert_absent(entries, "target.txt", msg="resolved conflict is clean post-commit")


# ---------------------------------------------------------------------------
# MOVE/COPY carried through an op, asserting the intended provenance. Skipped
# until move/copy is fully implemented.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_merge_carry_dirty_move(new_lore_repo):
    """A dirty MOVE (rename on disk + dirty_move) survives a clean branch merge
    with its provenance intact: after the auto-commit the destination is still
    reported action=move/fromPath=source and the source path stays absent."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base original\n", "old.txt": "movable content\n"})
    feat_rev = _feature_branch_with_add(repo)
    assert len(feat_rev) == 64

    os.rename(repo._fix_path("old.txt"), repo._fix_path("new.txt"))
    repo.dirty_move("old.txt", "new.txt", offline=True)

    repo.branch_merge("feature", offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["new.txt"], msg="only the carried move destination remains"
    )
    assert_entry(
        entries,
        "new.txt",
        action="move",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="old.txt",
        msg="dirty move carry must keep move provenance through merge",
    )
    assert_absent(entries, "old.txt", msg="move source must not reappear after merge")
    assert_absent(entries, "feat.txt", msg="feature add is committed and clean")


@pytest.mark.smoke
@pytest.mark.skip(
    reason="move/copy not fully implemented yet; asserts the intended behavior, re-enable when full move/copy support lands"
)
def test_cherrypick_carry_dirty_copy(new_lore_repo):
    """A dirty COPY survives a clean cherry-pick with its provenance intact:
    after the pick commit the destination is still reported action=copy/
    fromPath=source and the unchanged source stays clean."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n", "orig.txt": "source content\n"})
    source_rev = _feature_branch_with_add(repo)

    with repo.open_file("copy.txt", "w+") as f:
        f.write("source content\n")
    repo.dirty_copy("orig.txt", "copy.txt", offline=True)

    repo.revision_cherry_pick(source_rev, offline=True)

    entries = get_status_files(repo)
    assert_file_set(
        entries, ["copy.txt"], msg="only the carried copy destination remains"
    )
    assert_entry(
        entries,
        "copy.txt",
        action="copy",
        dirty=True,
        staged=False,
        node_type="file",
        from_path="orig.txt",
        msg="dirty copy carry must keep copy provenance through pick",
    )
    assert_absent(entries, "feat.txt", msg="picked file is committed and clean")
    assert_absent(entries, "orig.txt", msg="copy source is unchanged")


# ===========================================================================
# Empty directories (no files) as first-class nodes: scan / dirty / stage /
# commit / reset / delete + branch-switch carry and materialization
# ===========================================================================


@pytest.mark.smoke
def test_emptydir_scan_add_detected_and_persists(new_lore_repo):
    """A brand-new empty directory (no files inside) is detected by
    `status --scan` as action=add/type=directory/flagDirty and PERSISTS into a
    later no-scan status."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("empty")

    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(
        scanned, "empty", action="add", dirty=True, staged=False, node_type="directory"
    )
    assert_file_set(scanned, [], msg="an empty directory contributes no file entries")

    persisted = get_status_files(repo)
    assert_entry(
        persisted,
        "empty",
        action="add",
        dirty=True,
        staged=False,
        node_type="directory",
        msg="scanned empty-directory add must persist into a no-scan status",
    )


@pytest.mark.smoke
def test_emptydir_nested_scan_add(new_lore_repo):
    """`--scan` of a brand-new nested empty-directory chain reports every new
    directory node as action=add/type=directory and contributes no files."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("a/b/c")

    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(scanned, "a", action="add", node_type="directory")
    assert_entry(scanned, "a/b", action="add", node_type="directory")
    assert_entry(scanned, "a/b/c", action="add", node_type="directory")
    assert_file_set(scanned, [], msg="nested empty directories contribute no files")


@pytest.mark.smoke
def test_emptydir_stage_scan_commit_retained(new_lore_repo):
    """Staging a brand-new empty directory via `stage --scan` and committing
    lands the directory node in the committed tree and leaves a clean status
    (both no-scan and --scan)."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("empty")
    repo.stage(scan=True, offline=True)
    repo.commit("add empty directory", offline=True)

    dump = repo.repository_dump()
    assert "empty/" in dump, (
        f"committed empty directory should appear in the tree:\n{dump}"
    )

    entries = get_status_files(repo)
    assert entries == [], (
        f"status should be clean after commit, got {summarize(entries)}"
    )
    scanned = get_status_files_twice(repo, scan=True)
    assert scanned == [], f"--scan status should be clean, got {summarize(scanned)}"


@pytest.mark.smoke
def test_emptydir_dirty_mark_then_stage(new_lore_repo):
    """`file dirty` of a brand-new empty directory marks it action=add/
    type=directory; the default (no --scan) stage then promotes it to a staged
    add."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("empty")
    repo.dirty("empty", offline=True)

    marked = get_status_files(repo)
    assert_entry(
        marked,
        "empty",
        action="add",
        dirty=True,
        staged=False,
        node_type="directory",
        msg="file dirty must mark a new empty directory as a dirty add",
    )

    repo.stage(offline=True)
    staged = get_status_files(repo)
    assert_entry(
        staged,
        "empty",
        action="add",
        dirty=True,
        staged=True,
        node_type="directory",
        msg="default stage promotes the dirty-marked empty directory",
    )


@pytest.mark.smoke
def test_emptydir_reset_dirty_add_keeps_dir(new_lore_repo):
    """`reset` (without --purge) of a scan-detected empty-directory add discards
    the add tracking but leaves the directory on disk."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("empty")
    get_status_files_twice(repo, scan=True)

    repo.reset(["empty"], offline=True)

    entries = get_status_files(repo)
    assert_absent(
        entries, "empty", msg="reset discards the empty-directory add tracking"
    )
    assert repo.path_exists("empty"), (
        "reset without --purge keeps the empty directory on disk"
    )


@pytest.mark.smoke
def test_emptydir_reset_purge_removes_dir(new_lore_repo):
    """`reset --purge` of a scan-detected empty-directory add removes the
    directory from disk."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("empty")
    get_status_files_twice(repo, scan=True)

    repo.reset(["empty"], purge=True, offline=True)

    entries = get_status_files(repo)
    assert_absent(
        entries, "empty", msg="purged empty-directory add is gone from status"
    )
    assert not repo.path_exists("empty"), (
        "reset --purge removes the empty directory from disk"
    )


@pytest.mark.smoke
def test_emptydir_delete_committed_scan_detected(new_lore_repo):
    """Removing a committed empty directory from disk and scanning reports it as
    action=delete/type=directory and contributes no file entries."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.make_dirs("gone")
    repo.stage(scan=True, offline=True)
    repo.commit("add gone directory", offline=True)

    repo.remove_dir("gone")

    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(
        scanned,
        "gone",
        action="delete",
        node_type="directory",
        msg="removing a committed empty directory is a scan-detected delete",
    )
    assert_file_set(
        scanned, [], msg="empty-directory delete contributes no file entries"
    )


@pytest.mark.smoke
def test_emptydir_sibling_isolation(new_lore_repo):
    """A brand-new empty directory does not disturb a sibling committed file or
    a sibling committed directory: only the new empty directory is reported."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"keep/file.txt": "keep\n", "root.txt": "root\n"})

    repo.make_dirs("fresh")

    scanned = get_status_files_twice(repo, scan=True)
    assert_entry(scanned, "fresh", action="add", node_type="directory")
    assert_file_set(scanned, [], msg="only the empty directory changed; no files")
    assert_absent(scanned, "keep", msg="sibling committed directory is untouched")
    assert_absent(scanned, "keep/file.txt", msg="sibling committed file is untouched")
    assert_absent(scanned, "root.txt", msg="sibling committed file is untouched")


@pytest.mark.smoke
def test_emptydir_switch_carries_dirty_add(new_lore_repo):
    """A scan-detected empty-directory add is carried across a same-revision
    branch switch, remaining action=add/type=directory and present on disk."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    repo.make_dirs("carried")
    get_status_files_twice(repo, scan=True)

    pre = get_status_files(repo)
    assert_entry(
        pre,
        "carried",
        action="add",
        node_type="directory",
        msg="dirty add before switch",
    )

    repo.branch_switch("other", offline=True)

    entries = get_status_files(repo)
    assert_entry(
        entries,
        "carried",
        action="add",
        dirty=True,
        staged=False,
        node_type="directory",
        msg="empty-directory dirty add carried across same-revision switch",
    )
    assert repo.path_exists("carried"), (
        "carried empty directory remains on disk after switch"
    )


@pytest.mark.smoke
def test_emptydir_commit_on_branch_then_switch_reverts(new_lore_repo):
    """An empty directory committed on a feature branch is removed from disk
    when switching to a branch without it, and restored when switching back."""
    repo: Lore = new_lore_repo()
    commit_base(repo, {"base.txt": "base\n"})

    repo.branch_create("feature", offline=True)
    repo.make_dirs("feat_dir")
    repo.stage(scan=True, offline=True)
    repo.commit("add feat_dir", offline=True)
    assert "feat_dir/" in repo.repository_dump(), "empty dir committed on feature"

    repo.branch_switch("main", offline=True)
    assert not repo.path_exists("feat_dir"), (
        "empty dir absent on main after switch away"
    )

    repo.branch_switch("feature", offline=True)
    assert repo.path_exists("feat_dir"), (
        "empty dir restored when switching back to feature"
    )
