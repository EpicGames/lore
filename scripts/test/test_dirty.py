# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests for dirty file tracking.

Tests the file dirty API, stage/unstage/reset/commit interactions with dirty flags,
scan (--scan) setting dirty flags, and cross-operation sequences.
All tests use --json structured output to validate event data.
"""

import json
import logging
import os

import pytest
from lore_parsers import parse_jsonl, parse_status_json, parse_status_summary_json
from test_utils import to_posix

from lore import Lore

logger = logging.getLogger(__name__)


def get_status_files(repo: Lore, **kwargs) -> list[dict]:
    """Get status file events via JSON output."""
    output = repo.status(json=True, offline=True, **kwargs)
    return parse_status_json(output)


def get_status_files_twice(repo: Lore, **kwargs) -> list[dict]:
    """Run status twice with the same args and assert the second invocation
    reports the same set of files with identical dirty/staged flags as the
    first. Returns the entries from the second run.

    Regression guard: --scan (and its --unstaged alias) must be idempotent —
    a second invocation must not forget files (added, modified, or deleted)
    that the first invocation reported.
    """
    first = get_status_files(repo, **kwargs)
    second = get_status_files(repo, **kwargs)

    def fingerprint(entries: list[dict]) -> list[tuple]:
        return sorted(
            (to_posix(e.get("path", "")), e.get("flagDirty"), e.get("flagStaged"))
            for e in entries
        )

    first_fp = fingerprint(first)
    second_fp = fingerprint(second)
    assert first_fp == second_fp, (
        f"status should be idempotent across repeated invocations with {kwargs}.\n"
        f"first:  {first_fp}\n"
        f"second: {second_fp}"
    )
    return second


def find_status_entry(entries: list[dict], path: str) -> dict | None:
    """Find a status entry by path (posix-normalized)."""
    target = to_posix(path)
    for entry in entries:
        if to_posix(entry.get("path", "")) == target:
            return entry
    return None


def has_staged_anchor(repo: Lore) -> bool:
    """Check whether the repository has a staged revision."""
    output = repo.status(json=True, offline=True)
    zero_hash = "0" * 64
    for line in output.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        data = event.get("data", {})
        revision_staged = data.get("revisionStaged", "")
        if revision_staged and revision_staged != zero_hash:
            return True
    return False


# ===========================================================================
# Task 18: file dirty API (modify/add/delete/dir/move/copy)
# ===========================================================================


@pytest.mark.smoke
def test_dirty_modify(new_lore_repo):
    """Mark an existing file as dirty and verify status shows it."""
    repo: Lore = new_lore_repo()

    # Create, stage, commit a file
    with repo.open_file("file.txt", "w+") as f:
        f.write("original content\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify the file on disk
    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content\n")

    # Mark as dirty
    repo.dirty("file.txt", offline=True)

    # Verify status shows dirty file
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should appear in status"
    assert entry["flagDirty"] is True, "file.txt should be flagDirty"
    assert entry["flagStaged"] is False, "file.txt should not be flagStaged"


@pytest.mark.smoke
def test_dirty_add(new_lore_repo):
    """Mark a new file as dirty (add) and verify status."""
    repo: Lore = new_lore_repo()

    # Create and commit an initial file
    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Create a new file (not in revision) and mark dirty
    with repo.open_file("new_file.txt", "w+") as f:
        f.write("new content\n")
    repo.dirty("new_file.txt", offline=True)

    # Verify status
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "new_file.txt")
    assert entry is not None, "new_file.txt should appear in status"
    assert entry["flagDirty"] is True


@pytest.mark.smoke
def test_dirty_delete(new_lore_repo):
    """Mark a deleted file as dirty and verify status."""
    repo: Lore = new_lore_repo()

    with repo.open_file("to_delete.txt", "w+") as f:
        f.write("will be deleted\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Delete the file from disk
    os.remove(os.path.join(repo.path, "to_delete.txt"))

    # Mark as dirty
    repo.dirty("to_delete.txt", offline=True)

    # Verify status shows dirty delete
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "to_delete.txt")
    assert entry is not None, "to_delete.txt should appear in status"
    assert entry["flagDirty"] is True


@pytest.mark.smoke
def test_dirty_directory(new_lore_repo):
    """Mark a directory as dirty and verify all children are processed."""
    repo: Lore = new_lore_repo()

    # Create directory with files, stage, commit
    repo.make_dirs("src")
    with repo.open_file(os.path.join("src", "a.txt"), "w+") as f:
        f.write("aaa\n")
    with repo.open_file(os.path.join("src", "b.txt"), "w+") as f:
        f.write("bbb\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify one, delete one, add one
    with repo.open_file(os.path.join("src", "a.txt"), "w+") as f:
        f.write("aaa modified\n")
    os.remove(os.path.join(repo.path, "src", "b.txt"))
    with repo.open_file(os.path.join("src", "c.txt"), "w+") as f:
        f.write("new file\n")

    # Mark the directory as dirty
    repo.dirty("src", offline=True)

    # Verify all children are dirty
    entries = get_status_files(repo)
    a_entry = find_status_entry(entries, os.path.join("src", "a.txt"))
    b_entry = find_status_entry(entries, os.path.join("src", "b.txt"))
    c_entry = find_status_entry(entries, os.path.join("src", "c.txt"))

    assert a_entry is not None, "src/a.txt should appear (modify)"
    assert a_entry["flagDirty"] is True
    assert b_entry is not None, "src/b.txt should appear (delete)"
    assert b_entry["flagDirty"] is True
    assert c_entry is not None, "src/c.txt should appear (add)"
    assert c_entry["flagDirty"] is True


@pytest.mark.smoke
def test_dirty_move(new_lore_repo):
    """Mark a file as dirty-moved and verify status."""
    repo: Lore = new_lore_repo()

    repo.make_dirs("src")
    repo.make_dirs("dest")
    with repo.open_file(os.path.join("src", "file.txt"), "w+") as f:
        f.write("content\n")
    with repo.open_file(os.path.join("dest", "placeholder.txt"), "w+") as f:
        f.write("placeholder\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Move the file on disk
    os.rename(
        os.path.join(repo.path, "src", "file.txt"),
        os.path.join(repo.path, "dest", "file.txt"),
    )

    # Mark as dirty move
    repo.dirty_move(
        os.path.join("src", "file.txt"), os.path.join("dest", "file.txt"), offline=True
    )

    # Verify
    entries = get_status_files(repo)
    # The moved file should appear at the destination
    entry = find_status_entry(entries, os.path.join("dest", "file.txt"))
    assert entry is not None, "dest/file.txt should appear in status"
    assert entry["flagDirty"] is True


@pytest.mark.smoke
def test_dirty_copy(new_lore_repo):
    """Mark a file as dirty-copied and verify status."""
    repo: Lore = new_lore_repo()

    with repo.open_file("original.txt", "w+") as f:
        f.write("content\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Mark as dirty copy
    repo.dirty_copy("original.txt", "copy.txt", offline=True)

    # Verify copy appears in status
    entries = get_status_files(repo)
    copy_entry = find_status_entry(entries, "copy.txt")
    assert copy_entry is not None, "copy.txt should appear in status"
    assert copy_entry["flagDirty"] is True

    # Original should NOT be dirty
    orig_entry = find_status_entry(entries, "original.txt")
    assert orig_entry is None, "original.txt should not appear (not dirty)"


@pytest.mark.smoke
def test_dirty_ignore(new_lore_repo):
    """File that doesn't exist on disk or in revision is ignored."""
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Mark non-existent file as dirty — should be silently ignored
    repo.dirty("ghost.txt", offline=True)

    # Verify nothing extra in status
    entries = get_status_files(repo)
    ghost_entry = find_status_entry(entries, "ghost.txt")
    assert ghost_entry is None, "ghost.txt should not appear"


# ===========================================================================
# Task 15: stage/unstage interaction with Dirty
# ===========================================================================


@pytest.mark.smoke
def test_stage_preserves_dirty(new_lore_repo):
    """Staging a dirty file preserves the Dirty flag (orthogonal)."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify and mark dirty
    with repo.open_file("file.txt", "w+") as f:
        f.write("modified\n")
    repo.dirty("file.txt", offline=True)

    # Stage the file
    repo.stage(scan=True, offline=True)

    # Verify both dirty AND staged
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should appear"
    assert entry["flagDirty"] is True, "Dirty should be preserved after stage"
    assert entry["flagStaged"] is True, "Should be staged"


@pytest.mark.smoke
def test_unstage_preserves_dirty_when_file_differs(new_lore_repo):
    """Unstaging a dirty+staged file preserves Dirty when file still differs."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)
    repo.stage(scan=True, offline=True)

    # Unstage
    repo.unstage(offline=True)

    # Dirty should remain (file still differs from committed)
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should appear"
    assert entry["flagDirty"] is True, "Dirty should remain after unstage"
    assert entry["flagStaged"] is False, "Should not be staged after unstage"


@pytest.mark.smoke
def test_unstage_preserves_anchor_when_dirty_remain(new_lore_repo):
    """Unstaging all staged files preserves anchor if dirty-only files remain."""
    repo: Lore = new_lore_repo()

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged\n")
    with repo.open_file("dirty.txt", "w+") as f:
        f.write("dirty original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify both
    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged modified\n")
    with repo.open_file("dirty.txt", "w+") as f:
        f.write("dirty modified longer\n")

    # Mark dirty.txt as dirty only
    repo.dirty("dirty.txt", offline=True)
    # Stage only staged.txt
    repo.stage("staged.txt", offline=True)
    # Unstage staged.txt
    repo.unstage("staged.txt", offline=True)

    # Anchor should still exist (dirty.txt is still dirty)
    assert has_staged_anchor(repo), "Anchor should be preserved when dirty nodes remain"


# ===========================================================================
# Task 17: reset interaction with Dirty
# ===========================================================================


@pytest.mark.smoke
def test_reset_clears_dirty(new_lore_repo):
    """Resetting a dirty-only file clears the Dirty flag."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)

    # Verify dirty before reset
    entries = get_status_files(repo)
    assert find_status_entry(entries, "file.txt") is not None

    # Reset
    repo.reset("file.txt", offline=True)

    # File should be restored
    with repo.open_file("file.txt", "r") as f:
        content = f.read()
    assert content == "original\n", "File content should be restored"

    # Dirty should be cleared
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is None, "file.txt should not appear after reset (clean)"


# ===========================================================================
# Task 16: commit with mixed dirty/staged states
# ===========================================================================


@pytest.mark.smoke
def test_commit_preserves_dirty_only(new_lore_repo):
    """Commit clears dirty on committed files, preserves dirty-only files."""
    repo: Lore = new_lore_repo()

    with repo.open_file("committed.txt", "w+") as f:
        f.write("will commit\n")
    with repo.open_file("dirty_only.txt", "w+") as f:
        f.write("stay dirty\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify both
    with repo.open_file("committed.txt", "w+") as f:
        f.write("committed modified longer\n")
    with repo.open_file("dirty_only.txt", "w+") as f:
        f.write("dirty modified longer\n")

    # Mark both dirty
    repo.dirty(["committed.txt", "dirty_only.txt"], offline=True)

    # Stage only committed.txt
    repo.stage("committed.txt", offline=True)

    # Commit
    repo.commit(offline=True)

    # dirty_only.txt should still be dirty
    entries = get_status_files(repo)
    dirty_entry = find_status_entry(entries, "dirty_only.txt")
    assert dirty_entry is not None, "dirty_only.txt should still appear"
    assert dirty_entry["flagDirty"] is True, "dirty_only.txt should still be dirty"

    # committed.txt should be clean
    committed_entry = find_status_entry(entries, "committed.txt")
    assert committed_entry is None, "committed.txt should not appear (committed)"

    # Anchor should still exist
    assert has_staged_anchor(repo), "Anchor should exist (dirty_only.txt remains)"


@pytest.mark.smoke
def test_commit_deletes_anchor_when_clean(new_lore_repo):
    """Commit deletes anchor when no dirty or staged nodes remain."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified\n")
    repo.dirty("file.txt", offline=True)
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # No dirty, no staged — anchor should be gone
    assert not has_staged_anchor(repo), "Anchor should be deleted when clean"


# ===========================================================================
# Task 19: --scan setting dirty flags
# ===========================================================================


@pytest.mark.smoke
def test_scan_detects_modified_file(new_lore_repo):
    """--scan detects filesystem modifications and persists Dirty."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify file without calling dirty
    with repo.open_file("file.txt", "w+") as f:
        f.write("modified by scan detection\n")

    # Run scan twice — should detect the change and be idempotent
    entries = get_status_files_twice(repo, scan=True)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should be detected by scan"

    # Verify persisted — status without scan should show it
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should persist as dirty after scan"
    assert entry["flagDirty"] is True


@pytest.mark.smoke
def test_scan_detects_deleted_file(new_lore_repo):
    """--scan detects deleted files."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("content\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    os.remove(os.path.join(repo.path, "file.txt"))

    entries = get_status_files_twice(repo, scan=True)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "Deleted file should be detected by scan"


@pytest.mark.smoke
def test_scan_detects_new_file(new_lore_repo):
    """--scan detects new (untracked) files and persists Dirty+Add."""
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("new_file.txt", "w+") as f:
        f.write("untracked\n")

    entries = get_status_files_twice(repo, scan=True)
    entry = find_status_entry(entries, "new_file.txt")
    assert entry is not None, "New file should be detected by scan"

    # Verify persisted — status without scan should show it
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "new_file.txt")
    assert entry is not None, "New file should persist as dirty after scan"
    assert entry["flagDirty"] is True


@pytest.mark.smoke
def test_scan_detects_new_empty_file(new_lore_repo):
    """--scan detects empty untracked files idempotently.

    An empty file hashes to the zero address, which equals the zero address
    of a DirtyAdd state node — a naive content comparison would classify
    such a file as unmodified and clear its dirty flag on the second scan.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Empty file (zero bytes) — hashes to zero address.
    with repo.open_file("empty.txt", "w+"):
        pass

    entries = get_status_files_twice(repo, scan=True)
    entry = find_status_entry(entries, "empty.txt")
    assert entry is not None, "Empty new file should be detected by scan"
    assert entry["flagDirty"] is True

    # Verify persisted — status without scan should still show it.
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "empty.txt")
    assert entry is not None, "Empty new file should persist as dirty after scan"
    assert entry["flagDirty"] is True


@pytest.mark.smoke
def test_scan_drops_node_when_unstaged_add_deleted(new_lore_repo):
    """--scan discards a DirtyAdd node when its file is removed from disk.

    An unstaged add only exists in staged state. Removing the file reverts
    the add, so the node must not remain in state_staged — neither as
    DirtyAdd nor as a spurious Delete change.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("empty.txt", "w+"):
        pass

    entries = get_status_files_twice(repo, scan=True)
    assert find_status_entry(entries, "empty.txt") is not None

    repo.remove_file("empty.txt")

    entries = get_status_files_twice(repo, scan=True)
    assert find_status_entry(entries, "empty.txt") is None, (
        "Deleted unstaged-add should be removed from state, not reported"
    )

    # Confirm without scan as well — the node must be gone from state.
    entries = get_status_files(repo)
    assert find_status_entry(entries, "empty.txt") is None


@pytest.mark.smoke
def test_unstaged_alias_works(new_lore_repo):
    """--unstaged is a hidden alias for --scan."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified\n")

    entries = get_status_files_twice(repo, unstaged=True)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "--unstaged should work as alias for --scan"


# ===========================================================================
# Task 14: cross-operation sequences
# ===========================================================================


@pytest.mark.smoke
def test_dirty_stage_unstage_sequence(new_lore_repo):
    """dirty → stage → unstage preserves dirty state."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")

    # dirty → stage → unstage
    repo.dirty("file.txt", offline=True)
    repo.stage(scan=True, offline=True)
    repo.unstage(offline=True)

    # Should still be dirty (file still differs)
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should still appear"
    assert entry["flagDirty"] is True, "Dirty should survive stage→unstage"
    assert entry["flagStaged"] is False, "Should not be staged"


@pytest.mark.smoke
def test_dirty_stage_commit_preserves_other_dirty(new_lore_repo):
    """dirty → stage → commit: committed file clean, other dirty survives."""
    repo: Lore = new_lore_repo()

    with repo.open_file("a.txt", "w+") as f:
        f.write("aaa\n")
    with repo.open_file("b.txt", "w+") as f:
        f.write("bbb\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("a.txt", "w+") as f:
        f.write("aaa modified longer\n")
    with repo.open_file("b.txt", "w+") as f:
        f.write("bbb modified longer\n")

    repo.dirty(["a.txt", "b.txt"], offline=True)
    repo.stage("a.txt", offline=True)
    repo.commit(offline=True)

    entries = get_status_files(repo)
    a_entry = find_status_entry(entries, "a.txt")
    b_entry = find_status_entry(entries, "b.txt")
    assert a_entry is None, "a.txt should be clean after commit"
    assert b_entry is not None, "b.txt should survive commit"
    assert b_entry["flagDirty"] is True


@pytest.mark.smoke
def test_scan_clears_stale_dirty_on_reverted_file(new_lore_repo):
    """--scan clears Dirty on a file that was reverted to match committed content."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify and mark dirty
    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)

    # Verify dirty
    entries = get_status_files(repo)
    assert find_status_entry(entries, "file.txt") is not None, (
        "Should be dirty before revert"
    )

    # Revert the file back to original content
    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")

    # Run scan twice — should detect file matches committed, clear Dirty, and stay clean
    get_status_files_twice(repo, scan=True)

    # Verify dirty cleared — status without scan should show nothing
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is None, "Dirty should be cleared after scan on reverted file"


@pytest.mark.smoke
def test_scan_clears_one_dirty_keeps_other(new_lore_repo):
    """--scan clears Dirty on reverted file but keeps Dirty on still-modified file."""
    repo: Lore = new_lore_repo()

    with repo.open_file("reverted.txt", "w+") as f:
        f.write("original reverted\n")
    with repo.open_file("still_dirty.txt", "w+") as f:
        f.write("original dirty\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify both and mark dirty
    with repo.open_file("reverted.txt", "w+") as f:
        f.write("modified reverted longer\n")
    with repo.open_file("still_dirty.txt", "w+") as f:
        f.write("modified dirty longer\n")
    repo.dirty(["reverted.txt", "still_dirty.txt"], offline=True)

    # Revert one file
    with repo.open_file("reverted.txt", "w+") as f:
        f.write("original reverted\n")

    # Scan twice — idempotent
    get_status_files_twice(repo, scan=True)

    # Check: reverted.txt should be clean, still_dirty.txt should remain dirty
    entries = get_status_files(repo)
    reverted_entry = find_status_entry(entries, "reverted.txt")
    dirty_entry = find_status_entry(entries, "still_dirty.txt")
    assert reverted_entry is None, "reverted.txt should be clean after scan"
    assert dirty_entry is not None, "still_dirty.txt should still be dirty"
    assert dirty_entry["flagDirty"] is True


@pytest.mark.smoke
def test_scan_after_commit_shows_remaining_dirty(new_lore_repo):
    """After commit with dirty-only files, scan confirms they're still dirty."""
    repo: Lore = new_lore_repo()

    with repo.open_file("committed.txt", "w+") as f:
        f.write("committed\n")
    with repo.open_file("remaining.txt", "w+") as f:
        f.write("remaining\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("committed.txt", "w+") as f:
        f.write("committed modified\n")
    with repo.open_file("remaining.txt", "w+") as f:
        f.write("remaining modified longer\n")

    repo.dirty(["committed.txt", "remaining.txt"], offline=True)
    repo.stage("committed.txt", offline=True)
    repo.commit(offline=True)

    # Scan twice should confirm remaining.txt is still dirty
    entries = get_status_files_twice(repo, scan=True)
    entry = find_status_entry(entries, "remaining.txt")
    assert entry is not None, "remaining.txt should still appear after commit + scan"
    assert entry["flagDirty"] is True


# ===========================================================================
# Stage from dirty-marked files
# ===========================================================================


@pytest.mark.smoke
def test_stage_from_dirty_marks(new_lore_repo):
    """stage (default) stages dirty-marked files without filesystem walk."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content\n")

    # Mark dirty, then stage (default = from dirty marks)
    repo.dirty("file.txt", offline=True)
    repo.stage(scan=True, offline=True)

    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should be staged"
    assert entry["flagStaged"] is True


@pytest.mark.smoke
def test_stage_scan_stages_without_dirty(new_lore_repo):
    """stage --scan stages modified files even without dirty marks."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content\n")

    # Stage with --scan (no dirty mark needed)
    repo.stage(scan=True, offline=True)

    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should be staged with --scan"
    assert entry["flagStaged"] is True


@pytest.mark.smoke
def test_stage_default_only_stages_dirty(new_lore_repo):
    """stage (default) only stages dirty-marked files, ignores unmarked changes."""
    repo: Lore = new_lore_repo()

    with repo.open_file("dirty.txt", "w+") as f:
        f.write("original\n")
    with repo.open_file("unmarked.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("dirty.txt", "w+") as f:
        f.write("modified dirty\n")
    with repo.open_file("unmarked.txt", "w+") as f:
        f.write("modified unmarked\n")

    # Only mark one file dirty
    repo.dirty("dirty.txt", offline=True)
    repo.stage(offline=True, scan=False)

    entries = get_status_files(repo)
    dirty_entry = find_status_entry(entries, "dirty.txt")
    unmarked_entry = find_status_entry(entries, "unmarked.txt")
    assert dirty_entry is not None, "dirty.txt should be staged"
    assert dirty_entry["flagStaged"] is True
    assert unmarked_entry is None, "unmarked.txt should NOT be staged"


@pytest.mark.smoke
def test_stage_single_file_without_dirty(new_lore_repo):
    """stage <file> works without dirty mark (backward compat, filesystem check)."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content\n")

    # Stage specific file without marking dirty — should still work
    repo.stage("file.txt", offline=True)

    entries = get_status_files(repo)
    entry = find_status_entry(entries, "file.txt")
    assert entry is not None, "file.txt should be staged via direct file path"
    assert entry["flagStaged"] is True


@pytest.mark.smoke
def test_stage_directory_recurses_dirty_added_subdirectory(new_lore_repo):
    """`stage .` stages dirty-marked adds in a new subdirectory of a tracked dir.

    A committed directory `some/` gains a brand-new subdirectory `some/test/`
    with files, alongside a modified root file. After `status --scan` records
    the changes, a plain `status` reports the nested adds and `stage .` (staging
    the repository root from the dirty marks, no `--scan`) stages them along
    with the modified root file, and the subsequent commit lands them in the
    committed revision.
    """
    repo: Lore = new_lore_repo()

    # Commit a baseline root file and a tracked `some/` directory.
    with repo.open_file("root.txt", "w+") as f:
        f.write("original root\n")
    repo.make_dirs("some")
    with repo.open_file("some/keep.txt", "w+") as f:
        f.write("kept content\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Modify the root file and add a new subdirectory under the tracked
    # `some/`, with the new files one level deeper still.
    with repo.open_file("root.txt", "w+") as f:
        f.write("modified root content\n")
    repo.make_dirs("some/test")
    with repo.open_file("some/test/alpha.txt", "w+") as f:
        f.write("alpha content\n")
    with repo.open_file("some/test/beta.txt", "w+") as f:
        f.write("beta content\n")

    nested = ("some/test/alpha.txt", "some/test/beta.txt")

    # `status --scan` detects and persists all changes as dirty.
    scanned = get_status_files(repo, scan=True)
    assert find_status_entry(scanned, "root.txt") is not None, (
        "scan should report the modified root file"
    )
    for path in nested:
        assert find_status_entry(scanned, path) is not None, (
            f"scan should report the added {path}"
        )

    # A plain status (current vs staged) must still surface the persisted
    # nested adds — not just the modified root file.
    persisted = get_status_files(repo)
    for path in ("root.txt", *nested):
        assert find_status_entry(persisted, path) is not None, (
            f"{path} should persist in plain status after scan"
        )

    # Stage the repository root from the dirty marks (no --scan). Every dirty
    # path under root must be staged, including the nested adds.
    repo.stage(offline=True)

    entries = get_status_files(repo)
    for path in ("root.txt", *nested):
        entry = find_status_entry(entries, path)
        assert entry is not None, f"{path} should appear in status after stage"
        assert entry["flagStaged"] is True, (
            f"{path} should be staged by `stage .` from dirty marks"
        )

    # End-to-end: committing the staged tree lands the nested files in the
    # committed revision.
    repo.commit(offline=True)
    repo.status(reset=True, offline=True)
    dump = repo.repository_dump()
    for name in ("alpha.txt", "beta.txt"):
        assert name in dump, f"{name} should appear in the committed revision:\n{dump}"


@pytest.mark.smoke
def test_dirty_stage_commit_empty_directories(new_lore_repo):
    """Empty directories flagged dirty stage and commit as directory adds.

    A base revision commits a root file and a subdirectory holding a file. Two
    brand-new empty directories are then created -- one in the repository root
    and one inside the committed subdirectory -- and flagged with the dirty API
    (no scan). Each empty directory is its own change with no child file to
    anchor it.

    Plain `status` (no scan) must report exactly those two directories as
    unstaged adds and nothing else (the committed `subdir` parent gaining a
    child is not itself a reported change). `status --check-dirty` must report
    the exact same set. Staging the two directories must report two directory
    adds and no files, after which plain `status` reports them as staged adds.
    Committing then succeeds and leaves a clean status.
    """
    repo: Lore = new_lore_repo()

    # Base revision: a root file and a subdirectory with a file.
    with repo.open_file("file.txt", "w+") as f:
        f.write("root content\n")
    repo.make_dirs("subdir")
    with repo.open_file("subdir/sub_file.txt", "w+") as f:
        f.write("sub content\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Empty directory in the root and an empty directory inside the committed
    # subdirectory, both absent from the base revision.
    empty_dirs = ["empty_root", "subdir/empty_sub"]
    for path in empty_dirs:
        repo.make_dirs(path)

    repo.dirty(empty_dirs, offline=True)

    expected = {to_posix(p) for p in empty_dirs}

    def assert_empty_dir_adds(entries: list[dict], *, staged: bool, label: str) -> None:
        by_path = {to_posix(e["path"]): e for e in entries}
        assert set(by_path) == expected, (
            f"{label}: status should report exactly the two empty directories, "
            f"got {sorted(by_path)}"
        )
        for path in expected:
            entry = by_path[path]
            assert entry["type"] == "directory", (
                f"{label}: {path} should be a directory: {entry}"
            )
            assert entry["action"] == "add", (
                f"{label}: {path} should be an add: {entry}"
            )
            assert entry["flagDirty"] is True, (
                f"{label}: {path} should be dirty: {entry}"
            )
            assert entry["flagStaged"] is staged, (
                f"{label}: {path} flagStaged should be {staged}: {entry}"
            )

    # Expect: plain status (no scan) reports exactly the two empty directories
    # as unstaged dirty adds -- and nothing else.
    assert_empty_dir_adds(get_status_files(repo), staged=False, label="plain status")

    # Expect: --check-dirty re-verifies the dirty markers against the filesystem
    # and reports the exact same two unstaged dirty adds (a dir add is never
    # cleared as "unmodified").
    assert_empty_dir_adds(
        get_status_files(repo, check_dirty=True), staged=False, label="--check-dirty"
    )

    # Stage both directories by their own paths. Expect: the stage summary
    # counts exactly two directory adds and nothing else -- no files, and no
    # modify of the committed `subdir` parent that one of them lives under.
    stage_output = repo.stage(empty_dirs, json=True, offline=True)
    stage_ends = parse_jsonl(stage_output, "fileStageEnd")
    assert stage_ends, f"stage should emit a fileStageEnd event:\n{stage_output}"
    count = stage_ends[-1]["count"]
    assert count["directoryAddCount"] == 2, count
    assert count["directoryModifyCount"] == 0, count
    assert count["directoryDeleteCount"] == 0, count
    assert count["directoryMoveCount"] == 0, count
    assert count["fileAddCount"] == 0, count
    assert count["fileModifyCount"] == 0, count
    assert count["fileDeleteCount"] == 0, count
    assert count["fileMoveCount"] == 0, count
    assert count["totalCount"] == 2, count

    # Expect: plain status now reports the same two directories, still adds, but
    # flagged staged.
    assert_empty_dir_adds(get_status_files(repo), staged=True, label="after stage")

    # Expect: the commit succeeds and, with nothing left pending, a following
    # plain status reports no changes at all.
    repo.commit(offline=True)
    assert get_status_files(repo) == [], (
        "status should report no changes after committing the empty directories"
    )


# ===========================================================================
# Dirty add in new directories, nonexistent paths, ignored paths
# ===========================================================================


@pytest.mark.smoke
def test_dirty_add_in_new_directory(new_lore_repo):
    """file dirty on a new file in a new directory creates intermediate directory nodes."""
    repo: Lore = new_lore_repo()

    with repo.open_file("existing.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Create a file in a new directory (not in current revision)
    repo.make_dirs("new_dir/sub_dir")
    with repo.open_file("new_dir/sub_dir/new_file.txt", "w+") as f:
        f.write("new content\n")

    repo.dirty("new_dir/sub_dir/new_file.txt", offline=True)

    entries = get_status_files(repo)
    entry = find_status_entry(entries, "new_dir/sub_dir/new_file.txt")
    assert entry is not None, "new file in new dir should be dirty"
    assert entry["flagDirty"] is True


@pytest.mark.smoke
def test_dirty_nonexistent_path_ignored(new_lore_repo):
    """file dirty on a path that doesn't exist on disk or in state is ignored."""
    repo: Lore = new_lore_repo()

    with repo.open_file("existing.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Call dirty on a completely nonexistent path
    repo.dirty("nonexistent/path/file.txt", offline=True)

    # Should have no effect
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "nonexistent/path/file.txt")
    assert entry is None, "nonexistent path should be ignored"


@pytest.mark.smoke
def test_dirty_ignored_path_skipped(new_lore_repo):
    """file dirty on a path under an ignored directory is skipped."""
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Set up ignore rule for some/path/
    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("some/path/\n")

    # Create a file under the ignored path
    repo.make_dirs("some/path")
    with repo.open_file("some/path/file.txt", "w+") as f:
        f.write("should be ignored\n")

    repo.dirty("some/path/file.txt", offline=True)

    entries = get_status_files(repo)
    entry = find_status_entry(entries, "some/path/file.txt")
    assert entry is None, "file under ignored path should be skipped"


@pytest.mark.smoke
def test_commit_excludes_dirty_only_node(new_lore_repo):
    """Dirty-only nodes must not change the committed revision.

    Setup committed with a baseline file inside a kept directory. Then
    in one staged commit, three independent things happen:

    1. A file in a brand-new directory is staged — must end up in the
       committed tree.
    2. A file in a different brand-new directory is marked dirty (add)
       without being staged — its node and its dirty-add parent must be
       discarded before the merkle tree is sealed.
    3. A previously-committed file is deleted on disk and marked dirty
       (delete) without being staged — the dirty-delete must be reverted
       at commit time so the file stays in the committed tree.

    After commit, status (no scan) must still flag both pending dirty
    entries; after --reset drops the staged tracking, a repository dump
    of the latest committed revision must show the staged path and the
    untouched previously-committed file, but not the dirty-add path.
    """
    repo: Lore = new_lore_repo()

    # Base revision with a file that will later be deleted+dirtied.
    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.make_dirs("kept_dir")
    with repo.open_file("kept_dir/kept.txt", "w+") as f:
        f.write("kept content here\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Stage a file in a brand-new directory.
    repo.make_dirs("staged_dir")
    with repo.open_file("staged_dir/staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged_dir/staged.txt", offline=True)

    # Mark a file in a different brand-new directory as dirty (add).
    repo.make_dirs("dirty_dir")
    with repo.open_file("dirty_dir/dirty.txt", "w+") as f:
        f.write("dirty content\n")
    repo.dirty("dirty_dir/dirty.txt", offline=True)

    # Delete a committed file from disk and mark it dirty (delete) — not
    # staged.
    os.remove(os.path.join(repo.path, "kept_dir", "kept.txt"))
    repo.dirty("kept_dir/kept.txt", offline=True)

    # Before commit, status reports both dirty entries as pending and the
    # deleted file as an unstaged delete.
    pre = get_status_files(repo)
    pre_add = find_status_entry(pre, "dirty_dir/dirty.txt")
    assert pre_add is not None, "dirty add should be reported pre-commit"
    assert pre_add["flagDirty"] is True
    assert pre_add["flagStaged"] is False
    pre_del = find_status_entry(pre, "kept_dir/kept.txt")
    assert pre_del is not None, "dirty delete should be reported pre-commit"
    assert pre_del["flagDirty"] is True
    assert pre_del["flagStaged"] is False
    assert pre_del.get("action") == "delete", (
        f"kept_dir/kept.txt should report action=delete, got {pre_del.get('action')}"
    )

    repo.commit(offline=True)

    # Status (no scan) must still flag both dirty entries as pending —
    # neither was staged, so commit must leave them tracked.
    entries = get_status_files(repo)
    dirty_add = find_status_entry(entries, "dirty_dir/dirty.txt")
    assert dirty_add is not None, "dirty add should remain pending after commit"
    assert dirty_add["flagDirty"] is True
    assert dirty_add["flagStaged"] is False
    dirty_del = find_status_entry(entries, "kept_dir/kept.txt")
    assert dirty_del is not None, "dirty delete should remain pending after commit"
    assert dirty_del["flagDirty"] is True
    assert dirty_del["flagStaged"] is False
    assert dirty_del.get("action") == "delete", (
        f"kept_dir/kept.txt should still report action=delete, got {dirty_del.get('action')}"
    )
    # The staged file is now part of the committed revision and must not
    # appear in status anymore.
    assert find_status_entry(entries, "staged_dir/staged.txt") is None, (
        "staged_dir/staged.txt should be clean after commit"
    )

    # Drop the tracked staged state so dump shows only the committed tree.
    repo.status(reset=True, offline=True)

    dump = repo.repository_dump()
    assert "staged.txt" in dump, (
        f"staged_dir/staged.txt should appear in committed revision:\n{dump}"
    )
    # Dirty-only add must be absent from the committed tree.
    assert "dirty.txt" not in dump, (
        f"dirty-only added file should not appear in committed revision:\n{dump}"
    )
    assert "dirty_dir" not in dump, (
        f"dirty-only added directory should not appear in committed revision:\n{dump}"
    )
    # Dirty-only delete must be reverted — the file is still in the tree.
    assert "kept.txt" in dump, (
        f"dirty-only deleted file should remain in committed revision:\n{dump}"
    )
    assert "kept_dir" in dump, (
        f"directory of dirty-only deleted file should remain in committed revision:\n{dump}"
    )


@pytest.mark.smoke
def test_status_reapplies_ignore_to_dirty_add(new_lore_repo):
    """Status applies the current ignore filter to dirty-marked unstaged
    files. After marking a file dirty in a subdirectory two levels deep,
    status reports it as an unstaged add. Adding the parent directory to
    the ignore file and re-running status must drop the file from the
    report — the user may have updated the ignore between the dirty mark
    and the status query.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Create an untracked file two levels deep and mark it dirty.
    repo.make_dirs("some/path")
    with repo.open_file("some/path/file.txt", "w+") as f:
        f.write("new content\n")
    repo.dirty("some/path/file.txt", offline=True)

    # Before the ignore is added, status reports the file as an unstaged add.
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "some/path/file.txt")
    assert entry is not None, (
        "some/path/file.txt should be reported as unstaged add before ignore"
    )
    assert entry["flagDirty"] is True
    assert entry["flagStaged"] is False

    # Add the second-level directory to the ignore file after the dirty
    # mark has already been persisted.
    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("some/path/\n")

    # Status must re-apply the current ignore filter and stop reporting
    # the dirty-marked file as unstaged add.
    entries = get_status_files(repo)
    entry = find_status_entry(entries, "some/path/file.txt")
    assert entry is None, (
        "some/path/file.txt should not be reported after ignore is added"
    )


# ===========================================================================
# Dirty flag does not block operations that check filesystem state
# ===========================================================================


@pytest.mark.smoke
def test_branch_switch_with_stale_dirty_flag(new_lore_repo):
    """Branch switch proceeds when a file's Dirty flag is set but its on-disk content matches the committed revision."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Target branch to switch to.
    repo.branch_create("other", offline=True)
    repo.branch_switch("main", offline=True)

    # Modify, mark dirty, revert content so the filesystem matches the committed revision.
    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)
    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")

    repo.branch_switch("other", offline=True)
    assert "On branch other" in repo.status(offline=True)


@pytest.mark.smoke
def test_branch_create_with_stale_dirty_flag(new_lore_repo):
    """Branch create proceeds when a file's Dirty flag is set but its on-disk content matches the committed revision."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)
    with repo.open_file("file.txt", "w+") as f:
        f.write("original\n")

    repo.branch_create("new-branch", offline=True)
    assert "On branch new-branch" in repo.status(offline=True)


@pytest.mark.smoke
def test_sync_with_stale_dirty_flag(new_lore_repo):
    """Sync proceeds when a file's Dirty flag is set but its on-disk content matches the current revision."""
    repo: Lore = new_lore_repo()

    # Two commits give sync a target to move toward.
    with repo.open_file("file.txt", "w+") as f:
        f.write("v1\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 longer content\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Step back to v1, then mark file dirty against an intermediate modification before reverting on disk.
    repo.sync(rev_v1, offline=True)
    with repo.open_file("file.txt", "w+") as f:
        f.write("modified content longer\n")
    repo.dirty("file.txt", offline=True)
    with repo.open_file("file.txt", "w+") as f:
        f.write("v1\n")

    repo.sync(offline=True)
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v2 longer content\n"


@pytest.mark.smoke
def test_status_reset_clears_tracked_state(new_lore_repo):
    """--reset drops the existing tracked state before computing status.

    Phase 1: with tracked dirty files and a staged file, --reset alone
    clears all tracking and returns an empty status.

    Phase 2: after reset, modifying and staging a single file then
    running --reset --scan drops the stage and rescans from the
    filesystem, listing the file as an unstaged modification against
    the committed revision.
    """
    repo: Lore = new_lore_repo()

    for name in ("a.txt", "b.txt", "c.txt", "d.txt"):
        with repo.open_file(name, "w+") as f:
            f.write(f"{name} original\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Track two files as dirty
    with repo.open_file("a.txt", "w+") as f:
        f.write("a modified content longer\n")
    with repo.open_file("b.txt", "w+") as f:
        f.write("b modified content longer\n")
    repo.dirty(["a.txt", "b.txt"], offline=True)

    # Stage a third file
    with repo.open_file("c.txt", "w+") as f:
        f.write("c modified content longer\n")
    repo.stage("c.txt", offline=True)

    pre_paths = {to_posix(e["path"]) for e in get_status_files(repo)}
    assert pre_paths == {"a.txt", "b.txt", "c.txt"}, (
        f"Expected three pending paths before reset, got {pre_paths}"
    )

    # --reset alone drops the tracked state; status reports nothing
    entries = get_status_files(repo, reset=True)
    assert entries == [], f"--reset should yield empty status, got {entries}"
    assert not has_staged_anchor(repo), "Anchor should be cleared after --reset"

    # Revert prior on-disk modifications so the tree matches the committed revision
    for name in ("a.txt", "b.txt", "c.txt"):
        with repo.open_file(name, "w+") as f:
            f.write(f"{name} original\n")

    # Modify and stage a single file on the cleared tracked state
    with repo.open_file("d.txt", "w+") as f:
        f.write("d modified content longer\n")
    repo.stage("d.txt", offline=True)

    # --reset drops the stage; --scan re-detects d.txt as a dirty modification
    entries = get_status_files(repo, reset=True, scan=True)
    paths = {to_posix(e["path"]) for e in entries}
    assert paths == {"d.txt"}, (
        f"After --reset --scan only d.txt should appear as unstaged modified, got {paths}"
    )
    entry = find_status_entry(entries, "d.txt")
    assert entry["flagDirty"] is True, "d.txt should be dirty after rescan"
    assert entry["flagStaged"] is False, "d.txt should be unstaged after --reset"


@pytest.mark.smoke
def test_branch_switch_rebases_staged_anchor(new_lore_repo):
    """Branch switch leaves a clean status reflecting the new branch's content."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("alice content here\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    repo.branch_create("other", offline=True)
    with repo.open_file("file.txt", "w+") as f:
        f.write("bob content here\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    repo.branch_switch("main", offline=True)
    assert not has_staged_anchor(repo), (
        "Staged anchor should be cleared after branch switch with no dirty work"
    )
    entries = get_status_files_twice(repo, scan=True)
    file_entry = find_status_entry(entries, "file.txt")
    assert file_entry is None, (
        f"file.txt should not appear in status after branch switch, got: {file_entry}"
    )
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "alice content here\n"


@pytest.mark.smoke
def test_sync_rebases_staged_anchor(new_lore_repo):
    """Sync to a previous revision updates the staged anchor so status reflects the new current."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("v1 content\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    rev_v1 = repo.revision_history(offline=True)[0].signature

    with repo.open_file("file.txt", "w+") as f:
        f.write("v2 content longer\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # Sync back to v1 and scan; file should match v1 with no false modifications.
    repo.sync(rev_v1, offline=True)
    entries = get_status_files_twice(repo, scan=True)
    entry = find_status_entry(entries, "file.txt")
    assert entry is None, (
        f"file.txt should not appear in status after sync, got: {entry}"
    )
    with repo.open_file("file.txt", "r") as f:
        assert f.read() == "v1 content\n"


@pytest.mark.smoke
def test_branch_switch_rebases_only_dirty_files(new_lore_repo):
    """Switching branches carries only dirty paths into the new staged state.

    The repo has files in multiple paths committed on main. A feature branch
    commits unrelated modify/add/delete changes. While on feature, separate
    files are dirtied (modify, add, delete). Switching back to main rebases
    the staged anchor: only the dirtied paths remain dirty, and the rest of
    the tree matches main's revision.
    """
    repo: Lore = new_lore_repo()

    repo.write_files(
        {
            "app/main.py": "main entrypoint\n",
            "app/utils/helper.py": "helper original\n",
            "docs/readme.md": "readme original\n",
            "data/sample.txt": "sample data\n",
            "data/config.json": "{}\n",
        }
    )
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    repo.branch_create("feature", offline=True)

    repo.write_files(
        {
            "app/main.py": "modified on feature\n",
            "app/new_feature.py": "new feature code\n",
        }
    )
    os.remove(repo._fix_path("docs/readme.md"))
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    repo.write_files(
        {
            "data/sample.txt": "dirty modified sample\n",
            "data/extra.txt": "dirty new file\n",
        }
    )
    os.remove(repo._fix_path("app/utils/helper.py"))
    repo.dirty(
        ["data/sample.txt", "data/extra.txt", "app/utils/helper.py"], offline=True
    )

    pre_entries = get_status_files_twice(repo)
    pre_dirty = {to_posix(e["path"]) for e in pre_entries if e.get("flagDirty")}
    assert pre_dirty == {"data/sample.txt", "data/extra.txt", "app/utils/helper.py"}, (
        f"Dirty paths before switch should match expectations, got {pre_dirty}"
    )

    repo.branch_switch("main", offline=True)

    entries = get_status_files_twice(repo)
    dirty_paths = {to_posix(e["path"]) for e in entries if e.get("flagDirty")}
    expected_dirty = {"data/sample.txt", "data/extra.txt", "app/utils/helper.py"}
    assert dirty_paths == expected_dirty, (
        f"After switch, only dirtied paths should be dirty. "
        f"Expected {expected_dirty}, got {dirty_paths}"
    )

    with repo.open_file("app/main.py", "r") as f:
        assert f.read() == "main entrypoint\n"
    assert not os.path.exists(repo._fix_path("app/new_feature.py")), (
        "Feature-only file should not exist on main"
    )
    with repo.open_file("docs/readme.md", "r") as f:
        assert f.read() == "readme original\n"
    with repo.open_file("data/config.json", "r") as f:
        assert f.read() == "{}\n"

    with repo.open_file("data/sample.txt", "r") as f:
        assert f.read() == "dirty modified sample\n"
    with repo.open_file("data/extra.txt", "r") as f:
        assert f.read() == "dirty new file\n"
    assert not os.path.exists(repo._fix_path("app/utils/helper.py")), (
        "Locally deleted file should remain absent after switch"
    )


# ===========================================================================
# Regression guard for "Discard hierarchy broken" in the scan-cleanup path
# ===========================================================================


@pytest.mark.smoke
def test_status_scan_partial_revert_with_remaining(new_lore_repo):
    """`status --scan` over a wide+deep dirty-add tree where half the
    files in each leaf are removed from disk and half remain. Verifies
    the cleanup leaves each parent's chain consistent enough that the
    surviving siblings are still reported as DirtyAdd.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    tops = 10
    mids = 5
    leaves = 3
    files_per_dir = 8

    paths_to_remove = []
    paths_to_keep = []
    for t in range(tops):
        for m in range(mids):
            for l in range(leaves):
                sub = os.path.join(f"top{t:02d}", f"mid{m:02d}", f"leaf{l:02d}")
                repo.make_dirs(sub)
                for f in range(files_per_dir):
                    p = os.path.join(sub, f"file{f:02d}.txt")
                    with repo.open_file(p, "w+") as fh:
                        fh.write(f"{p}\n")
                    if f % 2 == 0:
                        paths_to_remove.append(p)
                    else:
                        paths_to_keep.append(p)

    get_status_files(repo, scan=True)

    for p in paths_to_remove:
        os.remove(os.path.join(repo.path, p))

    # Cleanup scans — first the full tree, then overlapping path args
    # matching the user's failing invocation shape.
    get_status_files(repo, scan=True)
    get_status_files(
        repo,
        path=[f"top{t:02d}" for t in range(tops)] + ["."],
        scan=True,
    )

    entries = get_status_files(repo, scan=True)
    by_path = {to_posix(e["path"]): e for e in entries if e.get("type") == "file"}

    for p in paths_to_keep:
        entry = by_path.get(to_posix(p))
        assert entry is not None, f"kept dirty-add {p} should still appear in status"
        assert entry.get("flagDirty") is True, (
            f"kept dirty-add {p} should still be flagDirty, got {entry}"
        )
        assert entry.get("flagStaged") is False
        assert entry.get("action") == "add", (
            f"kept dirty-add {p} should report action=add, got {entry.get('action')}"
        )

    for p in paths_to_remove:
        assert to_posix(p) not in by_path, (
            f"removed dirty-add {p} should be cleaned up, got {by_path.get(to_posix(p))}"
        )


# ===========================================================================
# Branch merge with dirty-only tracking
# ===========================================================================


@pytest.mark.smoke
def test_branch_merge_refuses_with_staged_nodes(new_lore_repo):
    """`lore branch merge` refuses to start if any actually-staged node
    exists. Dirty-only tracking is tolerated (see the next test); only
    the `Staged` flag blocks the merge.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    repo.branch_create("feature", offline=True)
    with repo.open_file("feature.txt", "w+") as f:
        f.write("feature content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature commit", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged.txt", offline=True)

    from error_types import LoreException

    with pytest.raises(LoreException) as excinfo:
        repo.branch_merge("feature", offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"merge should refuse on actually-staged nodes, got:\n{excinfo.value}"
    )

    entry = find_status_entry(get_status_files(repo), "staged.txt")
    assert entry is not None and entry["flagStaged"] is True, (
        "staged.txt should remain staged after the rejected merge"
    )


@pytest.mark.smoke
def test_branch_merge_carries_dirty_through_clean_merge(new_lore_repo):
    """A dirty-only file pending on the current branch is still reported
    as pending after a clean `lore branch merge` lands its auto-commit.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base original\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    repo.branch_create("feature", offline=True)
    with repo.open_file("feature.txt", "w+") as f:
        f.write("feature content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature commit", offline=True)
    repo.branch_switch("main", offline=True)

    # Dirty (not staged) modification on main — no overlap with feature
    # branch's changes, so the merge is clean.
    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited\n")
    repo.dirty("base.txt", offline=True)

    pre = find_status_entry(get_status_files(repo), "base.txt")
    assert pre is not None and pre["flagDirty"] is True and pre["flagStaged"] is False

    repo.branch_merge("feature", offline=True)

    # feature.txt got merged onto disk and is now part of the committed
    # revision — should not appear in status.
    assert os.path.exists(os.path.join(repo.path, "feature.txt"))
    entries = get_status_files(repo)
    assert find_status_entry(entries, "feature.txt") is None, (
        "feature.txt should be clean after merge"
    )

    # The carry survived — base.txt is still pending as a dirty modify.
    post = find_status_entry(entries, "base.txt")
    assert post is not None, "dirty base.txt should survive the merge"
    assert post["flagDirty"] is True
    assert post["flagStaged"] is False


@pytest.mark.smoke
def test_branch_merge_carries_dirty_through_conflict_merge(new_lore_repo):
    """A dirty-only file unrelated to a merge conflict is still reported
    as pending after `merge resolve` + `lore commit` finishes the merge.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("conflict.txt", "w+") as f:
        f.write("base\n")
    with repo.open_file("untouched.txt", "w+") as f:
        f.write("untouched\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edits conflict.txt", offline=True)
    repo.branch_switch("main", offline=True)

    # Make a conflicting change on main and commit it.
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    # Dirty (not staged) modification on a file the merge does not touch.
    with repo.open_file("untouched.txt", "w+") as f:
        f.write("locally edited\n")
    repo.dirty("untouched.txt", offline=True)

    # Merge hits a conflict on conflict.txt — pick "mine" to resolve.
    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected merge to surface a conflict, got:\n{merge_output}"
    )
    repo.branch_merge_resolve_mine("conflict.txt", offline=True)
    repo.commit("merge resolved", offline=True)

    # After the merge commit, the dirty carry has been replayed.
    entries = get_status_files(repo)
    untouched_entry = find_status_entry(entries, "untouched.txt")
    assert untouched_entry is not None, (
        "dirty untouched.txt should survive the conflicted merge"
    )
    assert untouched_entry["flagDirty"] is True
    assert untouched_entry["flagStaged"] is False
    assert find_status_entry(entries, "conflict.txt") is None, (
        "conflict.txt should be clean after merge resolve + commit"
    )


@pytest.mark.smoke
def test_branch_merge_abort_keeps_dirty_carry(new_lore_repo):
    """After `merge abort`, the dirty tracking captured at `merge start`
    is dropped — a subsequent unrelated commit does not re-apply it.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    repo.branch_create("feature", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature edit", offline=True)
    repo.branch_switch("main", offline=True)

    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edit", offline=True)

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited\n")
    repo.dirty("base.txt", offline=True)

    merge_output = repo.branch_merge("feature", offline=True)
    assert "conflicted" in merge_output, (
        f"expected merge to surface a conflict, got:\n{merge_output}"
    )
    repo.branch_merge_abort(offline=True)

    # Abort cancels the merge but preserves the unrelated dirty-only carry; an
    # unrelated later commit leaves the carry pending as a dirty modify.
    with repo.open_file("staged_post.txt", "w+") as f:
        f.write("post-abort\n")
    repo.stage("staged_post.txt", offline=True)
    repo.commit("post-abort commit", offline=True)

    entries = get_status_files(repo)
    assert find_status_entry(entries, "staged_post.txt") is None, (
        "staged_post.txt should be clean after commit"
    )
    carry = find_status_entry(entries, "base.txt")
    assert (
        carry is not None
        and carry["flagDirty"] is True
        and carry["flagStaged"] is False
    ), "merge abort preserves the pre-existing dirty-only carry"


# ===========================================================================
# Cherry-pick with dirty-only tracking (same merge_carry pattern as branch
# merge: refuse on actually-staged, forward dirty-only paths)
# ===========================================================================


@pytest.mark.smoke
def test_cherry_pick_refuses_with_staged_nodes(new_lore_repo):
    """revision cherry-pick refuses if any actually-staged node exists.

    Same guarantee as branch merge — only the `Staged` flag blocks the
    operation. Dirty-only markers are forwarded through `merge_carry`.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    repo.branch_create("source", offline=True)
    with repo.open_file("from_source.txt", "w+") as f:
        f.write("source content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("source commit", offline=True)
    source_rev = repo.revision_history(1, offline=True)[0].signature

    repo.branch_switch("main", offline=True)

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged.txt", offline=True)

    from error_types import LoreException

    with pytest.raises(LoreException) as excinfo:
        repo.revision_cherry_pick(source_rev, offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"cherry-pick should refuse on actually-staged nodes, got:\n{excinfo.value}"
    )

    entry = find_status_entry(get_status_files(repo), "staged.txt")
    assert entry is not None and entry["flagStaged"] is True, (
        "staged.txt should remain staged after the rejected cherry-pick"
    )


@pytest.mark.smoke
def test_cherry_pick_carries_dirty_through_clean_pick(new_lore_repo):
    """Clean cherry-pick forwards dirty-only tracking onto the resulting
    commit via the `merge_carry` blob — same pattern as branch merge."""
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base original\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    repo.branch_create("source", offline=True)
    with repo.open_file("from_source.txt", "w+") as f:
        f.write("source content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("source commit", offline=True)
    source_rev = repo.revision_history(1, offline=True)[0].signature

    repo.branch_switch("main", offline=True)

    # Dirty (not staged) modification on main — doesn't overlap with the
    # cherry-picked revision so the pick is clean.
    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited\n")
    repo.dirty("base.txt", offline=True)

    pre = find_status_entry(get_status_files(repo), "base.txt")
    assert pre is not None and pre["flagDirty"] is True and pre["flagStaged"] is False

    repo.revision_cherry_pick(source_rev, offline=True)

    # from_source.txt should now be in the committed tree and clean.
    assert os.path.exists(os.path.join(repo.path, "from_source.txt"))
    entries = get_status_files(repo)
    assert find_status_entry(entries, "from_source.txt") is None, (
        "from_source.txt should be clean after cherry-pick"
    )

    # base.txt still pending as a dirty modify — carry replayed.
    post = find_status_entry(entries, "base.txt")
    assert post is not None, "dirty base.txt should survive the cherry-pick"
    assert post["flagDirty"] is True
    assert post["flagStaged"] is False


@pytest.mark.smoke
def test_cherry_pick_carries_dirty_through_conflict_pick(new_lore_repo):
    """Cherry-pick that ends in a conflict still preserves dirty-only
    tracking — the carry is stored at `cherry-pick start`, the merge
    state occupies the staged anchor for conflict resolution, and the
    eventual `lore commit` replays the carry."""
    repo: Lore = new_lore_repo()

    with repo.open_file("conflict.txt", "w+") as f:
        f.write("base\n")
    with repo.open_file("untouched.txt", "w+") as f:
        f.write("untouched\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    repo.branch_create("source", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("source\n")
    repo.stage(scan=True, offline=True)
    repo.commit("source edits conflict.txt", offline=True)
    source_rev = repo.revision_history(1, offline=True)[0].signature

    repo.branch_switch("main", offline=True)

    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edits conflict.txt", offline=True)

    # Dirty (not staged) modification on a file the cherry-pick does not touch.
    with repo.open_file("untouched.txt", "w+") as f:
        f.write("locally edited\n")
    repo.dirty("untouched.txt", offline=True)

    pick_output = repo.revision_cherry_pick(source_rev, offline=True)
    assert "conflicted" in pick_output, (
        f"expected cherry-pick to surface a conflict, got:\n{pick_output}"
    )
    repo.revision_cherry_pick_resolve_mine("conflict.txt", offline=True)
    repo.commit("cherry-pick resolved", offline=True)

    entries = get_status_files(repo)
    untouched_entry = find_status_entry(entries, "untouched.txt")
    assert untouched_entry is not None, (
        "dirty untouched.txt should survive the conflicted cherry-pick"
    )
    assert untouched_entry["flagDirty"] is True
    assert untouched_entry["flagStaged"] is False
    assert find_status_entry(entries, "conflict.txt") is None, (
        "conflict.txt should be clean after cherry-pick resolve + commit"
    )


@pytest.mark.smoke
def test_cherry_pick_abort_keeps_dirty_carry(new_lore_repo):
    """`cherry-pick abort` clears the `merge_carry` blob (it delegates
    to `merge_abort`, which already handles carry cleanup)."""
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("base\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    repo.branch_create("source", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("source\n")
    repo.stage(scan=True, offline=True)
    repo.commit("source edit", offline=True)
    source_rev = repo.revision_history(1, offline=True)[0].signature

    repo.branch_switch("main", offline=True)
    with repo.open_file("conflict.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("main edit", offline=True)

    with repo.open_file("base.txt", "w+") as f:
        f.write("base locally edited\n")
    repo.dirty("base.txt", offline=True)

    pick_output = repo.revision_cherry_pick(source_rev, offline=True)
    assert "conflicted" in pick_output, (
        f"expected cherry-pick to surface a conflict, got:\n{pick_output}"
    )
    repo.revision_cherry_pick_abort(offline=True)

    # Abort cancels the pick but preserves the unrelated dirty-only carry; a
    # subsequent unrelated commit leaves the carry pending as a dirty modify.
    with repo.open_file("staged_post.txt", "w+") as f:
        f.write("post-abort\n")
    repo.stage("staged_post.txt", offline=True)
    repo.commit("post-abort commit", offline=True)

    entries = get_status_files(repo)
    assert find_status_entry(entries, "staged_post.txt") is None, (
        "staged_post.txt should be clean after commit"
    )
    carry = find_status_entry(entries, "base.txt")
    assert (
        carry is not None
        and carry["flagDirty"] is True
        and carry["flagStaged"] is False
    ), "cherry-pick abort preserves the pre-existing dirty-only carry"


# ===========================================================================
# Branch reset with dirty-only tracking
# ===========================================================================


@pytest.mark.smoke
def test_branch_reset_refuses_with_staged_nodes(new_lore_repo):
    """`lore branch reset` refuses if any actually-staged node exists.
    Dirty-only tracking is tolerated (see the next test); only the
    `Staged` flag blocks the reset.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base v1\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v1", offline=True)
    rev_v1 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("base.txt", "w+") as f:
        f.write("base v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged.txt", offline=True)

    from error_types import LoreException

    with pytest.raises(LoreException) as excinfo:
        repo.branch_reset(rev_v1, offline=True)
    assert "Unable to reset branch when there is a staged state" in str(
        excinfo.value
    ), f"reset should refuse on actually-staged nodes, got:\n{excinfo.value}"

    entry = find_status_entry(get_status_files(repo), "staged.txt")
    assert entry is not None and entry["flagStaged"] is True, (
        "staged.txt should remain staged after the rejected reset"
    )


@pytest.mark.smoke
def test_branch_reset_carries_dirty_through_same_branch_reset(new_lore_repo):
    """A dirty-only file pending on the current branch is still reported
    as pending after `lore branch reset` moves the branch tip to an
    earlier revision.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base v1\n")
    with repo.open_file("untouched.txt", "w+") as f:
        f.write("untouched v1\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v1", offline=True)
    rev_v1 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("base.txt", "w+") as f:
        f.write("base v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)

    # Dirty (not staged) modification on a file the reset target doesn't
    # touch — at v1, untouched.txt is "untouched v1", on disk it's the
    # locally edited version below.
    with repo.open_file("untouched.txt", "w+") as f:
        f.write("untouched locally edited\n")
    repo.dirty("untouched.txt", offline=True)

    pre = find_status_entry(get_status_files(repo), "untouched.txt")
    assert pre is not None and pre["flagDirty"] is True and pre["flagStaged"] is False

    repo.branch_reset(rev_v1, offline=True)

    with repo.open_file("base.txt", "r") as f:
        assert f.read() == "base v1\n"

    entries = get_status_files(repo)
    post = find_status_entry(entries, "untouched.txt")
    assert post is not None, "dirty untouched.txt should survive the reset"
    assert post["flagDirty"] is True
    assert post["flagStaged"] is False


# ===========================================================================
# Revert with dirty-only tracking
# ===========================================================================


@pytest.mark.smoke
def test_revert_refuses_with_staged_nodes(new_lore_repo):
    """`lore revision revert` refuses to start if any actually-staged
    node exists. Dirty-only tracking is tolerated; only the `Staged`
    flag blocks the operation.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base v1\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v1", offline=True)

    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added in v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    with repo.open_file("staged.txt", "w+") as f:
        f.write("staged content\n")
    repo.stage("staged.txt", offline=True)

    from error_types import LoreException

    with pytest.raises(LoreException) as excinfo:
        repo.revision_revert(rev_v2, offline=True)
    assert "Cannot merge with staged state" in str(excinfo.value), (
        f"revert should refuse on actually-staged nodes, got:\n{excinfo.value}"
    )

    entry = find_status_entry(get_status_files(repo), "staged.txt")
    assert entry is not None and entry["flagStaged"] is True, (
        "staged.txt should remain staged after the rejected revert"
    )


@pytest.mark.smoke
def test_revert_carries_dirty_through_clean_revert(new_lore_repo):
    """A dirty-only file in a brand-new subdirectory is still reported
    as pending after a clean `lore revision revert` auto-commits — the
    carry replay has to recreate the intermediate directory node in the
    fresh staged state.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base original\n")
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)

    with repo.open_file("revertable.txt", "w+") as f:
        f.write("added later\n")
    repo.stage(scan=True, offline=True)
    repo.commit("add revertable", offline=True)
    rev_to_revert = repo.revision_history(1, offline=True)[0].signature

    # Dirty add of a new file in a directory that doesn't exist in any
    # committed revision — the carry must recreate the dir hierarchy.
    repo.make_dirs("dirty_subdir")
    with repo.open_file("dirty_subdir/dirty_file.txt", "w+") as f:
        f.write("new dirty content\n")
    repo.dirty("dirty_subdir/dirty_file.txt", offline=True)

    pre = find_status_entry(get_status_files(repo), "dirty_subdir/dirty_file.txt")
    assert pre is not None and pre["flagDirty"] is True and pre["flagStaged"] is False

    repo.revision_revert(rev_to_revert, offline=True)

    # The reverted file is removed from disk and the committed tree.
    assert not os.path.exists(os.path.join(repo.path, "revertable.txt"))
    entries = get_status_files(repo)
    assert find_status_entry(entries, "revertable.txt") is None, (
        "revertable.txt should be gone from status after revert"
    )

    post = find_status_entry(entries, "dirty_subdir/dirty_file.txt")
    assert post is not None, (
        "dirty dirty_subdir/dirty_file.txt should survive the revert"
    )
    assert post["flagDirty"] is True
    assert post["flagStaged"] is False


@pytest.mark.smoke
def test_revert_carries_dirty_through_conflict_revert(new_lore_repo):
    """A dirty-only file in a brand-new subdirectory survives a
    conflicted revert all the way through `revert resolve` + `lore
    commit` — the carry replay has to recreate the intermediate
    directory node in the fresh staged state.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("target.txt", "w+") as f:
        f.write("v1\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v1", offline=True)

    with repo.open_file("target.txt", "w+") as f:
        f.write("v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v2 - to be reverted", offline=True)
    rev_v2 = repo.revision_history(1, offline=True)[0].signature

    # A further edit to target.txt makes reverting v2 conflict with v3.
    with repo.open_file("target.txt", "w+") as f:
        f.write("v3\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v3", offline=True)

    # Dirty add of a new file in a directory that doesn't exist in any
    # committed revision — the carry must recreate the dir hierarchy.
    repo.make_dirs("dirty_subdir")
    with repo.open_file("dirty_subdir/dirty_file.txt", "w+") as f:
        f.write("new dirty content\n")
    repo.dirty("dirty_subdir/dirty_file.txt", offline=True)

    revert_output = repo.revision_revert(rev_v2, offline=True)
    assert "conflicted" in revert_output, (
        f"expected revert to surface a conflict, got:\n{revert_output}"
    )
    repo.revision_revert_resolve_mine("target.txt", offline=True)
    repo.commit("revert resolved", offline=True)

    entries = get_status_files(repo)
    dirty_entry = find_status_entry(entries, "dirty_subdir/dirty_file.txt")
    assert dirty_entry is not None, (
        "dirty dirty_subdir/dirty_file.txt should survive the conflicted revert"
    )
    assert dirty_entry["flagDirty"] is True
    assert dirty_entry["flagStaged"] is False
    assert find_status_entry(entries, "target.txt") is None, (
        "target.txt should be clean after revert resolve + commit"
    )


@pytest.mark.smoke
def test_revert_abort_keeps_dirty_carry(new_lore_repo):
    """After `revert abort`, the dirty tracking captured at `revert start`
    is dropped — a subsequent unrelated commit does not re-apply it.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base\n")
    with repo.open_file("target.txt", "w+") as f:
        f.write("v1\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v1", offline=True)

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
        f.write("base locally edited\n")
    repo.dirty("base.txt", offline=True)

    revert_output = repo.revision_revert(rev_v2, offline=True)
    assert "conflicted" in revert_output, (
        f"expected revert to surface a conflict, got:\n{revert_output}"
    )
    repo.revision_revert_abort(offline=True)

    with repo.open_file("staged_post.txt", "w+") as f:
        f.write("post-abort\n")
    repo.stage("staged_post.txt", offline=True)
    repo.commit("post-abort commit", offline=True)

    entries = get_status_files(repo)
    assert find_status_entry(entries, "staged_post.txt") is None, (
        "staged_post.txt should be clean after commit"
    )
    carry = find_status_entry(entries, "base.txt")
    assert (
        carry is not None
        and carry["flagDirty"] is True
        and carry["flagStaged"] is False
    ), "revert abort preserves the pre-existing dirty-only carry"


def write_view_filter(repo: Lore, lines: list[str]) -> None:
    """Write the repository view filter (.lore/view) with the given lines."""
    view_path = os.path.join(repo.dot_path(), "view")
    with open(view_path, "w+") as view_file:
        view_file.write("\n".join(lines) + "\n")


def remove_view_filter(repo: Lore) -> None:
    """Remove the repository view filter so all paths are back in view."""
    view_path = os.path.join(repo.dot_path(), "view")
    if os.path.exists(view_path):
        os.remove(view_path)


@pytest.mark.smoke
def test_dirty_delete_respects_view_filter(new_lore_repo):
    """A dirty-delete of a directory must not mark view-filtered descendants.

    Regression guard for the carry-forward dirty recursion: a view pattern like
    `/Templates/*` excludes the *contents* of `Templates` but not the directory
    node itself, so `dirty Templates` would previously recurse the whole subtree
    and mark every (filtered) child as a dirty-delete in the staged anchor.

    The marking is hidden by the view (and a fresh scan would re-derive the
    on-disk deletion regardless), so it is observed with `--check-dirty` after
    removing the view: that only re-examines nodes already marked dirty in the
    staged anchor, so the dirty-delete count reflects exactly what the recursion
    wrote. With the fix only the `Templates` directory node is marked, so the
    summary reports a single delete; without it, every filtered descendant
    (`a.txt`, `sub`, `sub/b.txt`) would inflate the count.
    """
    repo: Lore = new_lore_repo()

    # Tracked subtree (with nesting) plus a normal file, committed.
    repo.make_dirs(os.path.join("Templates", "sub"))
    with repo.open_file(os.path.join("Templates", "a.txt"), "w+") as f:
        f.write("a\n")
    with repo.open_file(os.path.join("Templates", "sub", "b.txt"), "w+") as f:
        f.write("b\n")
    with repo.open_file("keep.txt", "w+") as f:
        f.write("keep\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # View excludes the contents of Templates but not the Templates node itself.
    write_view_filter(repo, ["/Templates/*"])

    # Remove the now-filtered subtree from disk and mark the directory dirty,
    # reproducing the phantom dirty-delete of a view-only directory. With it
    # gone from disk, `dirty` takes the not-on-disk + in-revision delete path.
    repo.rmtree("Templates")
    repo.dirty("Templates", offline=True)

    # Drop the view and re-examine the dirty markers without touching the
    # filesystem (--check-dirty does not walk disk; it only verifies nodes
    # already marked dirty in the staged anchor).
    remove_view_filter(repo)
    output = repo.status(check_dirty=True, json=True, offline=True)

    summary = parse_status_summary_json(output)
    assert summary is not None, "check-dirty must emit a repositoryStatusSummary event"
    assert summary["deletes"] == 1, (
        "only the Templates directory node should be marked dirty-delete; "
        f"filtered descendants must not be carried forward, got: {summary}"
    )

    # And none of the filtered descendants should appear as dirty entries.
    entries = parse_status_json(output)
    templates_children = [
        e
        for e in entries
        if to_posix(e.get("path", "")).startswith("Templates/")
        and e.get("flagDirty") is True
    ]
    assert not templates_children, (
        "view-filtered Templates children must not be marked dirty, "
        f"got: {[e['path'] for e in templates_children]}"
    )


@pytest.mark.smoke
def test_status_scan_summary_event(new_lore_repo):
    """`status --scan --json` emits a repositoryStatusSummary event whose
    per-action counts match the modify/add/delete made on disk."""
    repo: Lore = new_lore_repo()

    repo.make_dirs("src")
    with repo.open_file(os.path.join("src", "modify.txt"), "w+") as f:
        f.write("orig\n")
    with repo.open_file(os.path.join("src", "delete.txt"), "w+") as f:
        f.write("bye\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    with repo.open_file(os.path.join("src", "modify.txt"), "w+") as f:
        f.write("changed\n")
    os.remove(os.path.join(repo.path, "src", "delete.txt"))
    with repo.open_file(os.path.join("src", "add.txt"), "w+") as f:
        f.write("new\n")

    output = repo.status(scan=True, json=True, offline=True)
    summary = parse_status_summary_json(output)
    assert summary is not None, "scan must emit a repositoryStatusSummary event"
    assert summary["adds"] == 1, summary
    assert summary["modifies"] == 1, summary
    assert summary["deletes"] == 1, summary
    assert summary["moves"] == 0, summary
    assert summary["copies"] == 0, summary


@pytest.mark.smoke
def test_status_check_dirty_summary_excludes_reverted(new_lore_repo):
    """`status --check-dirty --json` summary counts only nodes that stay dirty
    after verification: a file marked dirty but restored to its committed
    content is cleared and excluded from the summary."""
    repo: Lore = new_lore_repo()

    with repo.open_file("real.txt", "w+") as f:
        f.write("v1\n")
    with repo.open_file("reverted.txt", "w+") as f:
        f.write("same\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # real.txt genuinely changes; reverted.txt is marked dirty but its on-disk
    # content still equals the committed content, so verify clears it.
    with repo.open_file("real.txt", "w+") as f:
        f.write("v2\n")
    repo.dirty(["real.txt", "reverted.txt"], offline=True)

    output = repo.status(check_dirty=True, json=True, offline=True)
    summary = parse_status_summary_json(output)
    assert summary is not None, "check-dirty must emit a repositoryStatusSummary event"
    assert summary["modifies"] == 1, summary
    assert summary["adds"] == 0, summary
    assert summary["deletes"] == 0, summary
    assert summary["moves"] == 0, summary
    assert summary["copies"] == 0, summary


@pytest.mark.smoke
def test_status_plain_has_no_summary_event(new_lore_repo):
    """A plain `status --json` (no scan, no check-dirty) does not emit the
    summary event — it is only produced by reconciling status runs."""
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w+") as f:
        f.write("data\n")
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    output = repo.status(json=True, offline=True)
    assert parse_status_summary_json(output) is None, (
        "plain status must not emit a repositoryStatusSummary event"
    )


# ===========================================================================
# Dirty-marked adds keep their add classification across a partial commit
# ===========================================================================


@pytest.mark.smoke
def test_dirty_partial_commit_keeps_uncommitted_adds(new_lore_repo):
    """New files flagged via the dirty API stay classified as adds across a
    partial commit, in every placement -- including a new subdirectory of an
    already-committed directory.

    A base revision commits a directory `existing/`. New files are then created
    in every placement a partial commit could disturb and flagged with the
    `dirty` API (not `status --scan`): two directly in the repository root, two
    in two brand-new top-level directories, and two in two brand-new
    subdirectories of the committed `existing/` directory. Status must report
    every one of them as a dirty add.

    One file of each kind is then staged and committed, leaving at least one
    uncommitted file of every kind -- a root file, a whole new top-level
    directory, and a whole new subdirectory of the existing directory. A plain
    `status` (no rescan) must still report each uncommitted file as an add: the
    partial commit must not reclassify them as modifies. A following
    `status --check-dirty` must report the same, since check-dirty only
    re-verifies the content of already-dirty files and never re-derives
    add/keep.
    """
    repo: Lore = new_lore_repo()

    # Base revision with an existing committed directory so the new
    # subdirectories below land inside a directory the base already knows.
    repo.write_files(
        {
            "existing/base_one.bin": os.urandom(64),
            "existing/base_two.bin": os.urandom(64),
        }
    )
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    # New files grouped so exactly one file of each kind is committed below and
    # at least one of each kind is left uncommitted.
    committed = [
        "root_committed.bin",
        "top_committed/one.bin",
        "top_committed/two.bin",
        "existing/sub_committed/one.bin",
        "existing/sub_committed/two.bin",
    ]
    pending = [
        "root_pending.bin",
        "top_pending/one.bin",
        "top_pending/two.bin",
        "existing/sub_pending/one.bin",
        "existing/sub_pending/two.bin",
    ]
    repo.write_files({name: os.urandom(64) for name in committed + pending})

    # Flag every new file dirty through the dirty API (not status --scan).
    repo.dirty(committed + pending, offline=True)

    def dirty_files(**kwargs) -> dict[str, dict]:
        return {
            to_posix(e["path"]): e
            for e in get_status_files(repo, **kwargs)
            if e.get("type") == "file"
        }

    # Status classifies every dirtied new file as an add, in every placement --
    # including the new subdirectories of the already-committed directory.
    dirtied = dirty_files()
    all_new = {to_posix(p) for p in committed + pending}
    assert set(dirtied) == all_new, (
        f"dirty + status should report exactly the new files as adds, "
        f"got {sorted(dirtied)}"
    )
    for p in committed + pending:
        entry = dirtied[to_posix(p)]
        assert entry["action"] == "add", f"{p} should be a dirty add: {entry}"
        assert entry["flagDirty"] is True, f"{p} should be dirty: {entry}"

    # Partial commit: stage and commit one file of each kind, leaving the rest.
    repo.stage(committed, offline=True)
    repo.commit(offline=True)

    # A plain status (no rescan) must still report each uncommitted file as an
    # add -- the partial commit must not demote them to modifies.
    after_commit = dirty_files()
    pending_posix = {to_posix(p) for p in pending}
    assert set(after_commit) == pending_posix, (
        f"after a partial commit, plain status should report exactly the "
        f"uncommitted new files, got {sorted(after_commit)}"
    )
    for p in pending:
        entry = after_commit[to_posix(p)]
        assert entry["action"] == "add", (
            f"partial commit demoted uncommitted new file {p} from add to "
            f"{entry['action']!r}: {entry}"
        )
        assert entry["flagDirty"] is True, f"{p} should remain dirty: {entry}"

    # --check-dirty re-verifies dirty content but never re-derives add/keep, so
    # it must report the uncommitted files exactly as before the commit.
    rechecked = dirty_files(check_dirty=True)
    assert set(rechecked) == pending_posix, (
        f"--check-dirty should report exactly the uncommitted new files, got "
        f"{sorted(rechecked)}"
    )
    for p in pending:
        entry = rechecked[to_posix(p)]
        assert entry["action"] == "add", (
            f"--check-dirty did not restore the add classification for {p}: {entry}"
        )
        assert entry["flagDirty"] is True, (
            f"{p} should remain dirty after --check-dirty: {entry}"
        )


@pytest.mark.smoke
def test_dirty_add_repeated_is_idempotent(new_lore_repo):
    """Marking the same added files dirty twice reports the same nodes both
    times, in plain status and under --check-dirty.

    A base revision commits a directory `existing/`. Three new files are then
    created -- one in the repository root, one in a brand-new top-level
    subdirectory, and one in a brand-new subdirectory of the committed
    `existing/` directory -- and flagged with the dirty API. Plain `status` (no
    scan, no check-dirty) and `status --check-dirty` must each report exactly
    those three as unstaged dirty adds, with no duplicated node. Flagging the
    same files dirty a second time must not change either report.
    """
    repo: Lore = new_lore_repo()

    repo.write_files({"existing/base.bin": os.urandom(64)})
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    added = [
        "root_add.bin",
        "new_dir/added.bin",
        "existing/new_sub/added.bin",
    ]
    repo.write_files({name: os.urandom(64) for name in added})
    expected = {to_posix(p) for p in added}

    def check(label: str, **kwargs) -> None:
        entries = get_status_files(repo, **kwargs)
        files = [e for e in entries if e.get("type") == "file"]
        by_path = {to_posix(e["path"]): e for e in files}
        assert len(files) == len(by_path), (
            f"{label}: duplicate file nodes reported: "
            f"{sorted(to_posix(e['path']) for e in files)}"
        )
        assert set(by_path) == expected, (
            f"{label}: wrong file set, got {sorted(by_path)}"
        )
        for p in expected:
            entry = by_path[p]
            assert entry["action"] == "add", f"{label}: {p} should be add: {entry}"
            assert entry["flagDirty"] is True, f"{label}: {p} should be dirty: {entry}"
            assert entry["flagStaged"] is False, (
                f"{label}: {p} should be unstaged: {entry}"
            )
        all_paths = [to_posix(e["path"]) for e in entries]
        assert len(all_paths) == len(set(all_paths)), (
            f"{label}: duplicate nodes reported: {sorted(all_paths)}"
        )

    repo.dirty(added, offline=True)
    check("first dirty, plain status")
    check("first dirty, --check-dirty", check_dirty=True)

    repo.dirty(added, offline=True)
    check("second dirty, plain status")
    check("second dirty, --check-dirty", check_dirty=True)
