# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import shutil
import stat
import sys
import time

import pytest

from error_types import (
    FileAlreadyExist,
    LocalChanges,
    BranchDivergedError,
    ProtectedError,
    UnknownLoreError,
)
from lore_parsers import parse_status_json, parse_status_summary_json
from test_utils import to_posix

from lore import Lore

logger = logging.getLogger(__name__)


def find_status_entry(entries: list[dict], path: str) -> dict | None:
    """Find a status entry by path (posix-normalized)."""
    target = to_posix(path)
    for entry in entries:
        if to_posix(entry.get("path", "")) == target:
            return entry
    return None


@pytest.mark.smoke
def test_sync(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"
    unicode_file = os.path.join("奇怪的路徑", "کاراکترهای یونیکد")
    todelete_file = os.path.join("path", "to", "delete")
    long_path_first_dir = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    long_path_file = os.path.join(
        long_path_first_dir,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "0000000000000000000000000000000000000000000000000",
        "1111111111111111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222222222222",
        "dddddddddddddddddddddddddddddddddddddddddddddd",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddd",
        "0000000000000000000000000000000000000000000000000",
        "1111111111111111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222222222222",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddd",
    )

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    repo.make_dirs(os.path.dirname(unicode_file))
    with repo.open_file(unicode_file, "w+", encoding="utf-8") as output_file:
        output_file.writelines(["只需將一些文本寫入文件即可\n"])

    repo.make_dirs(os.path.dirname(todelete_file))
    with repo.open_file(todelete_file, "w+b") as output_file:
        output_file.write(os.urandom(678901))

    repo.make_dirs(os.path.dirname(long_path_file))
    with repo.open_file(long_path_file, "w+b") as output_file:
        output_file.write(os.urandom(345678901))

    # Stage the files
    repo.stage(scan=True)

    # Commit the files
    repo.commit("Test commit", local=True)

    # Delete a file
    repo.remove_file(todelete_file)

    # Modify a file
    with repo.open_file(long_path_file, "w+b") as output_file:
        output_file.write(os.urandom(100))

    # Stage the files offline
    repo.stage(scan=True, offline=True)

    # Commit the files offline
    repo.commit(offline=True)

    # Push to remote
    repo.push()
    repo.repository_verify()

    clone = repo.clone()

    # Verify files contents, mode and last modified timestamp

    assert repo.compare_file(clone, text_file)
    assert repo.compare_file(clone, unicode_file)
    assert repo.compare_file(clone, long_path_file)

    assert not clone.file_exists(todelete_file), (
        "File not deleted as expected in cloned repo: "
    )

    # Do some modifications (add, modify write, delete, change mode)
    added_file = os.path.join("added", "a", "added")
    clone.make_dirs(os.path.dirname(added_file))
    with clone.open_file(added_file, "w+b") as output_file:
        output_file.write(os.urandom(1234567))

    with clone.open_file(unicode_file, "w+b") as modify_file:
        modify_file.write(os.urandom(32))

    long_path_to_remove = os.path.join(clone.path, long_path_first_dir)
    if sys.platform == "win32":
        long_path_to_remove = "\\\\?\\" + os.path.abspath(long_path_to_remove)
    shutil.rmtree(long_path_to_remove)

    os.chmod(os.path.join(clone.path, text_file), 0o755)

    # Status
    clone.status()

    # Stage
    clone.stage(scan=True)

    # Status
    clone.status()

    # Commit
    clone.commit("Update", local=True)

    # Push
    clone.push()
    clone.repository_verify()

    # Sync
    repo.sync()

    # Verify files contents, mode and last modified timestamp

    assert clone.compare_file(repo, text_file)
    assert clone.compare_file(repo, unicode_file)
    assert clone.compare_file(repo, added_file)

    if sys.platform != "win32":
        synced_mode = os.stat(os.path.join(repo.path, text_file)).st_mode
        assert synced_mode & stat.S_IXUSR, (
            f"the mode change must reach the synced repository: {synced_mode:o}"
        )

    assert not repo.path_exists(long_path_first_dir), (
        "Directory not deleted as expected in source repo: " + long_path_first_dir
    )

    shutil.rmtree(clone.dot_path(), ignore_errors=True)
    # If cloning in a directory with existing files they will be reused as long as
    # content is identical to expected files. Without the --force flag the clone will
    # fail if files don't match
    clone = repo.clone(clone.path, clone.name)

    # Verify files contents, mode and last modified timestamp

    assert repo.compare_file(clone, text_file)
    assert repo.compare_file(clone, unicode_file)
    assert repo.compare_file(clone, added_file)

    time.sleep(1)
    shutil.rmtree(clone.dot_path(), ignore_errors=True)
    shutil.rmtree(clone.dot_path(), ignore_errors=True)
    time.sleep(1)
    # Modify one of the files to ensure the clone fails
    with clone.open_file(unicode_file, "w+b") as modify_file:
        modify_file.write(os.urandom(32))
    with pytest.raises(FileAlreadyExist):
        clone = repo.clone(clone.path, clone.name)

    time.sleep(1)
    shutil.rmtree(clone.dot_path(), ignore_errors=True)
    shutil.rmtree(clone.dot_path(), ignore_errors=True)
    time.sleep(1)
    clone = repo.clone(clone.path, clone.name, force=True)

    # Verify files contents, mode and last modified timestamp

    assert repo.compare_file(clone, text_file)
    assert repo.compare_file(clone, unicode_file)
    assert repo.compare_file(clone, added_file)

    # Do some modifications
    another_added_file = os.path.join("added-too", "b", "added")
    repo.make_dirs(os.path.dirname(another_added_file))
    with repo.open_file(another_added_file, "w+b") as output_file:
        output_file.write(os.urandom(323567))
    with repo.open_file(added_file, "w+b") as output_file:
        output_file.write(os.urandom(223569))
    with repo.open_file(unicode_file, "w+b") as modify_file:
        modify_file.write(os.urandom(64))

    # Stage the files
    repo.stage(scan=True)

    # Commit the files
    repo.commit("Another test commit", local=True)

    # Push to remote
    repo.push()

    # Modify a file and ensure sync fails
    with clone.open_file(added_file, "w+b") as output_file:
        output_file.write(os.urandom(223569))
    with pytest.raises(LocalChanges):
        clone.sync()

    # Copy over source file and ensure sync succeeds
    shutil.copyfile(
        os.path.join(repo.path, added_file), os.path.join(clone.path, added_file)
    )
    clone.sync()

    # Verify files contents, mode and last modified timestamp
    assert repo.compare_file(clone, text_file)
    assert repo.compare_file(clone, unicode_file)
    assert repo.compare_file(clone, added_file)
    assert repo.compare_file(clone, another_added_file)

    # Modify files and verify that force sync works
    clone.remove_file(text_file)
    with clone.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(123))
    with clone.open_file(unicode_file, "w+b") as output_file:
        output_file.write(os.urandom(456))
    with clone.open_file(added_file, "w+b") as output_file:
        output_file.write(os.urandom(789))

    clone.sync(reset=True)

    assert repo.compare_file(clone, text_file)
    assert repo.compare_file(clone, unicode_file)
    assert repo.compare_file(clone, added_file)
    assert repo.compare_file(clone, another_added_file)

    # Verify files contents, mode and last modified timestamp
    dry_run = repo.clone(dry_run=True)

    assert not os.path.exists(dry_run.path), "Dry run clone created directory"

    # Modify files and commit/push a new change in source repo
    repo.remove_file(text_file)
    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(123))

    # Stage the files
    repo.stage(scan=True)

    # Commit the files
    repo.commit("Diverge source")

    # Push to remote
    repo.push()

    # Modify files and commit a new change in destination repo
    clone.remove_file(unicode_file)
    with clone.open_file(unicode_file, "w+b") as output_file:
        output_file.write(os.urandom(1234))

    # Stage the files
    clone.stage(scan=True)

    # Commit the files
    clone.commit("Diverge destination")

    # Push to remote and verify it fails
    # TODO(UCS-11886) Once we support server side fast-forward this should succeed
    with pytest.raises(BranchDivergedError):
        clone.push()

    # Protect the branch and verify a push fails
    repo.branch_protect("main")

    # Modify files and commit/push a new change in source repo
    repo.remove_file(text_file)
    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(1234))

    # Stage the files
    repo.stage(scan=True)

    # Commit the files
    repo.commit("Diverge source")

    # Push to remote
    with pytest.raises(ProtectedError):
        repo.push()

    # Unrotect the branch and verify a push now succeeds
    repo.branch_unprotect("main")

    # Force push to remote and verify it succeeds since now unprotected
    repo.push()
    repo.repository_verify()

    # Create a directory in source repo
    test_dir = "test_dir"
    os.mkdir(os.path.join(repo.path, test_dir))

    subdir_file = os.path.join(test_dir, "a-file.png")
    with repo.open_file(subdir_file, "w+b") as output_file:
        output_file.write(os.urandom(87654))

    repo.stage(scan=True)
    repo.commit("Add subdirectory file", offline=True)
    repo.push()
    repo.repository_verify()

    # Force sync destination repo
    clone.sync(force=True)

    assert repo.compare_file(clone, text_file)
    assert repo.compare_file(clone, unicode_file)
    assert repo.compare_file(clone, added_file)
    assert repo.compare_file(clone, another_added_file)
    assert repo.compare_file(clone, subdir_file)

    # Delete the directory in source repo
    shutil.rmtree(os.path.join(repo.path, test_dir))

    repo.stage(scan=True)
    repo.commit("Delete subdirectory")
    repo.push()
    repo.repository_verify()

    # Create a local file in the destination repo subdirectory
    another_subdir_file = os.path.join(test_dir, "another-subdir.file")
    with clone.open_file(another_subdir_file, "w+b") as output_file:
        output_file.write(os.urandom(17654))

    # Ensure we can still sync destination repo and keep the local file
    clone.sync()

    # Verify the local file still exist
    assert clone.path_exists(another_subdir_file), (
        "Local subdirectory file not retained when syncing a directory delete"
    )

    # Verify the deleted file was actually deleted
    assert not clone.path_exists(subdir_file), (
        "Deleted subdirectory file not deleted when syncing a directory delete over local modifications"
    )

    # Re-clone the repository
    clone.clear_local_files()
    clone = repo.clone(clone.path, clone.name)

    # Commit a new file in source and destination repositories

    diverge1_file = "divergent-1.file"
    with repo.open_file(diverge1_file, "w+b") as output_file:
        output_file.write(os.urandom(17653))

    diverge2_file = "divergent-2.file"
    with clone.open_file(diverge2_file, "w+b") as output_file:
        output_file.write(os.urandom(17653))

    repo.stage(scan=True)
    repo.commit("Add source file", local=True)
    repo.push()
    repo.repository_verify()

    clone.stage(scan=True)
    clone.commit("Add destination file")
    # Push should fail since we're divergent
    with pytest.raises(BranchDivergedError):
        clone.push()
    clone.repository_verify()

    # Sync and merge destination repository
    clone.sync()
    # Push should now succeed since we merged
    clone.push()
    clone.repository_verify()

    # Sync source repository
    repo.sync()
    repo.repository_verify()

    assert repo.compare_file(clone, diverge1_file)

    assert repo.compare_file(clone, diverge2_file)

    # Create a conflict

    with repo.open_file(diverge1_file, "w+b") as output_file:
        output_file.write(os.urandom(17653))

    with clone.open_file(diverge1_file, "w+b") as output_file:
        output_file.write(os.urandom(17653))

    repo.stage(scan=True)
    repo.commit("Modify source file", local=True)
    repo.push()
    repo.repository_verify()

    clone.stage(scan=True)
    clone.commit("Modify destination file")
    # Push should fail since we're divergent
    with pytest.raises(BranchDivergedError):
        clone.push()
    clone.repository_verify()

    # Sync and merge destination repository
    clone.sync()
    # Commit should now fail since we're in conflict
    output = clone.commit("Merge conflict", check=False)
    assert (diverge1_file + " is still in conflict") in output, (
        "Conflict not detected as expected"
    )

    shutil.rmtree(clone.path, ignore_errors=True)
    clone = repo.clone(clone.path, clone.name, revision="main@2")

    output = clone.revision_info()
    assert output.revision == "2"

    clone.sync("main@3")
    output = clone.revision_info()
    assert output.revision == "3"

    with repo.open_file(diverge1_file, "w+b") as output_file:
        output_file.write(os.urandom(17653))

    repo.stage(scan=True)
    repo.commit("Modify source file", local=True)
    repo.push()
    repo.repository_verify()

    output = repo.revision_info()
    source_revision = output.revision

    clone.sync()
    output = clone.revision_info()
    destination_revision = output.revision
    assert destination_revision == source_revision, (
        f"Sync did not use the correct revision, got {destination_revision} expected {source_revision}"
    )

    clone.sync("main@3")
    output = clone.revision_info()
    destination_revision = output.revision
    assert destination_revision == "3", (
        f"Sync did not use the correct revision, got {destination_revision} expected 3"
    )

    with repo.open_file(diverge1_file, "w+b") as output_file:
        output_file.write(os.urandom(17653))

    repo.stage(scan=True)
    repo.commit("Modify source file again", local=True)
    repo.push()
    repo.repository_verify()

    output = repo.revision_info()
    source_revision = output.revision

    clone.sync()
    output = clone.revision_info()
    destination_revision = output.revision
    assert destination_revision == source_revision, (
        f"Sync did not use the correct revision, got {destination_revision} expected {source_revision}"
    )

    with clone.open_file(diverge1_file, "w+b") as output_file:
        output_file.write(os.urandom(17653))

    clone.stage(scan=True)
    clone.commit("Modify source file again", local=True)
    clone.repository_verify()

    output = clone.revision_info()
    expected_revision = output.revision

    clone.sync("main@2")
    output = clone.revision_info()
    destination_revision = output.revision
    assert destination_revision == "2", (
        f"Sync did not use the correct revision, got {destination_revision} expected 2"
    )

    clone.sync()
    output = clone.revision_info()
    destination_revision = output.revision
    assert destination_revision == expected_revision, (
        f"Sync did not use the correct revision, got {destination_revision} expected {expected_revision}"
    )

    clone.push()

    repo.sync()
    output = repo.revision_info()
    source_revision = output.revision
    assert source_revision == expected_revision, (
        f"Sync did not use the correct revision, got {source_revision} expected {expected_revision}"
    )

    with repo.open_file(diverge1_file, "w+b") as output_file:
        output_file.write(os.urandom(17653))

    repo.stage(scan=True)
    repo.commit("Modify source file again", local=True)
    repo.push()
    repo.repository_verify()

    with clone.open_file(diverge2_file, "w+b") as output_file:
        output_file.write(os.urandom(1765))

    clone.stage(scan=True)
    clone.commit("Modify destination file to diverge", local=True)
    clone.repository_verify()

    output = clone.revision_info()
    expected_revision = output.signature

    clone.sync("main@3")
    output = clone.revision_info()
    destination_revision = output.revision
    assert destination_revision == "3", (
        f"Sync did not use the correct revision, got {destination_revision} expected 3"
    )

    clone.sync()
    output = clone.revision_info()
    destination_revision = output.signature
    assert destination_revision == expected_revision, (
        f"Sync did not use the correct revision, got {destination_revision} expected {expected_revision}"
    )

    clone.sync()
    output = clone.status()
    assert "On branch main revision 16" in output, (
        "Sync from local divergent latest did not initiate an expected merge"
    )


# Must exceed MAX_DIVERGENT_HISTORY_LENGTH (500) in lore-revision/src/branch.rs
# so find_divergence_base hits its cap-fallback path.
_FAR_BEHIND_REMOTE_COMMITS = 520


def _assert_clean_fast_forward(sync_output: str) -> None:
    """Assert the sync went through as a fast-forward, not a merge."""
    assert "performing merge" not in sync_output, (
        "Sync on a far-behind main went into the merge flow instead of "
        "fast-forwarding. Output:\n" + sync_output
    )
    assert "maximum history search reached" not in sync_output, (
        "find_divergence_base hit the cap. Output:\n" + sync_output
    )


@pytest.mark.smoke
def test_sync_far_behind_through_local_merge_tip(new_lore_repo):
    """A merge commit on main, followed by > MAX_DIVERGENT_HISTORY_LENGTH
    linear commits from another clone, must still fast-forward when a
    far-behind repo syncs. find_divergence_base's parent_self walk must
    not give up at the merge commit and fall back to base==target.
    """
    repo: Lore = new_lore_repo()

    shared_file = "shared.txt"
    repo.write_commit_push("Shared base", {shared_file: ["base\n"]})

    # Put a merge commit on main: side branch with one commit, merged back.
    side_branch = "local-side"
    side_file = "local-side.txt"
    repo.branch_create(side_branch, offline=True)
    repo.branch_switch(side_branch, offline=True)
    with repo.open_file(side_file, "w+") as f:
        f.write("on local side branch\n")
    repo.stage(side_file, offline=True)
    repo.commit("Commit on local-side", offline=True)
    repo.push()

    repo.branch_switch("main", offline=True)
    repo.branch_merge_start(
        side_branch,
        offline=True,
        message="Merge local-side into main",
    )
    repo.push()

    # A second clone races ahead with > MAX_DIVERGENT_HISTORY_LENGTH
    # linear commits on main.
    clone = repo.clone()
    bulk_file = "bulk.txt"
    for i in range(_FAR_BEHIND_REMOTE_COMMITS):
        with clone.open_file(bulk_file, "w+") as f:
            f.write(f"rev {i}\n")
        clone.stage(bulk_file, offline=True)
        clone.commit(f"Clone rev {i}", offline=True)
    clone.push()

    sync_output = repo.sync()
    _assert_clean_fast_forward(sync_output)
    repo.repository_verify()


@pytest.mark.smoke
def test_sync_far_behind_with_local_merge_tip_and_remote_merges(new_lore_repo):
    """Local main tip is a merge commit AND remote main has many merge
    commits. This is the closest topology to a shared Fortnite repo
    where side branches are merged on the server while a user has also
    merged something into main locally. The far-behind sync must still
    fast-forward.
    """
    repo: Lore = new_lore_repo()

    shared_file = "shared.txt"
    repo.write_commit_push("Shared base", {shared_file: ["base\n"]})

    # Local: merge a side branch into main.
    side_branch = "local-side"
    side_file = "local-side.txt"
    repo.branch_create(side_branch, offline=True)
    repo.branch_switch(side_branch, offline=True)
    with repo.open_file(side_file, "w+") as f:
        f.write("on local side branch\n")
    repo.stage(side_file, offline=True)
    repo.commit("Commit on local-side", offline=True)
    repo.push()

    repo.branch_switch("main", offline=True)
    repo.branch_merge_start(
        side_branch,
        offline=True,
        message="Merge local-side into main",
    )
    repo.push()

    # Clone races ahead with cycles of linear commits + merges.
    clone = repo.clone()
    bulk_file = "bulk.txt"
    remote_side_file = "remote-side.txt"

    cycles = _FAR_BEHIND_REMOTE_COMMITS // 5 + 1
    for cycle in range(cycles):
        for j in range(3):
            with clone.open_file(bulk_file, "w+") as f:
                f.write(f"main cycle {cycle} step {j}\n")
            clone.stage(bulk_file, offline=True)
            clone.commit(f"Main cycle {cycle} step {j}", offline=True)

        remote_branch = f"remote-side-{cycle}"
        clone.branch_create(remote_branch, offline=True)
        clone.branch_switch(remote_branch, offline=True)
        with clone.open_file(remote_side_file, "w+") as f:
            f.write(f"remote side cycle {cycle}\n")
        clone.stage(remote_side_file, offline=True)
        clone.commit(f"Remote side cycle {cycle}", offline=True)

        clone.branch_switch("main", offline=True)
        clone.branch_merge_start(
            remote_branch,
            offline=True,
            message=f"Merge remote-side-{cycle} into main",
        )

    clone.push()

    sync_output = repo.sync()
    _assert_clean_fast_forward(sync_output)
    repo.repository_verify()


def _read_bytes(repo: Lore, name: str) -> bytes:
    with repo.open_file(name, "rb") as f:
        return f.read()


@pytest.mark.smoke
def test_sync_aborted_by_local_change_writes_nothing(new_lore_repo):
    """A sync verifies every incoming change against the file system before it writes any
    of them, so a local modification it refuses to overwrite stops the whole sync with the
    working copy untouched.

    Every file is the same size on both sides, so the refusal rests on comparing content
    rather than on sizes, and the edit must still be reported afterwards: a sync that
    aborts may not leave a local change looking as though it had been dealt with.
    """
    repo: Lore = new_lore_repo()

    size = 4096
    names = [f"synced{index}.bin" for index in range(4)]
    for name in names:
        with repo.open_file(name, "w+b") as f:
            f.write(os.urandom(size))
    repo.stage(scan=True)
    repo.commit("Base")
    repo.push()

    clone: Lore = repo.clone()

    # New content of the same size for every file, so sizes settle nothing on either side.
    for name in names:
        with repo.open_file(name, "w+b") as f:
            f.write(os.urandom(size))
    repo.stage(scan=True)
    repo.commit("Incoming")
    repo.push()

    edited = names[0]
    with clone.open_file(edited, "w+b") as f:
        f.write(os.urandom(size))

    before = {name: _read_bytes(clone, name) for name in names}

    with pytest.raises(LocalChanges):
        clone.sync()

    after = {name: _read_bytes(clone, name) for name in names}
    assert after == before, (
        "a sync that refuses a local change must not have written any file: "
        f"{[name for name in names if after[name] != before[name]]} changed"
    )

    entries = parse_status_json(clone.status(scan=True, json=True, offline=True))
    assert find_status_entry(entries, edited) is not None, (
        "the local edit must still be reported after the sync aborts"
    )


@pytest.mark.smoke
def test_sync_records_modified_times_for_what_it_realizes(new_lore_repo):
    """A sync establishes what every file it touches holds: the ones it writes by writing
    them, and the ones that already hold the incoming content by measuring them. Recording
    the modified time in both cases is what leaves the next scan nothing to measure.

    One file here is written by the sync and one already holds the incoming bytes, so both
    routes are covered. All are the same size throughout, so a scan that reached for the
    content would have to measure it.
    """
    repo: Lore = new_lore_repo()

    size = 4096
    written = "written-by-sync.bin"
    already_held = "already-held.bin"
    for name in (written, already_held):
        with repo.open_file(name, "w+b") as f:
            f.write(os.urandom(size))
    repo.stage(scan=True)
    repo.commit("Base")
    repo.push()

    clone: Lore = repo.clone()

    for name in (written, already_held):
        with repo.open_file(name, "w+b") as f:
            f.write(os.urandom(size))
    repo.stage(scan=True)
    repo.commit("Incoming")
    repo.push()

    # Give the clone the incoming bytes for one file ahead of the sync, so that change is
    # verified against the target node and dropped rather than written.
    with clone.open_file(already_held, "w+b") as f:
        f.write(_read_bytes(repo, already_held))

    clone.sync()

    assert repo.compare_file(clone, written)
    assert repo.compare_file(clone, already_held)

    summary = parse_status_summary_json(
        clone.status(scan=True, json=True, offline=True)
    )
    assert summary is not None, "scan must emit a repositoryStatusSummary event"
    assert summary["hashChecks"] == 0, (
        f"a scan after a sync must measure no file it touched, got {summary}"
    )
    assert summary["mtimeMatches"] == 2, summary


@pytest.mark.smoke
def test_dry_run_sync_records_no_modified_times(new_lore_repo):
    """A dry run leaves the current revision where it was, so no modified time it takes
    describes the revision the working copy is on.

    The clone edits a file to exactly the incoming content, which is what makes the dry run
    reach the point of establishing that the file matches the node it would sync to. That
    node is not the one the clone is on, so recording the time would answer the next scan
    about the wrong node and hide the edit from status, from stage and from every commit.
    """
    repo: Lore = new_lore_repo()

    size = 4096
    name = "dry-run-probe.bin"
    with repo.open_file(name, "w+b") as f:
        f.write(os.urandom(size))
    repo.stage(scan=True)
    repo.commit("Base")
    repo.push()

    clone: Lore = repo.clone()

    incoming = os.urandom(size)
    with repo.open_file(name, "w+b") as f:
        f.write(incoming)
    repo.stage(scan=True)
    repo.commit("Incoming")
    repo.push()

    # Same size and the same content as the incoming revision, so the file differs from the
    # revision the clone is on while matching the one it would sync to.
    with clone.open_file(name, "w+b") as f:
        f.write(incoming)

    before = parse_status_json(clone.status(scan=True, json=True, offline=True))
    assert find_status_entry(before, name) is not None, (
        "the local edit must be reported before the dry run"
    )

    clone.sync(dry_run=True)

    after = clone.status(scan=True, json=True, offline=True)
    assert find_status_entry(parse_status_json(after), name) is not None, (
        "the local edit must still be reported after a dry run"
    )
    summary = parse_status_summary_json(after)
    assert summary is not None, "scan must emit a repositoryStatusSummary event"
    assert summary["mtimeMatches"] == 0, (
        f"a dry run may not leave a recorded time answering for the file, got {summary}"
    )


@pytest.mark.smoke
@pytest.mark.skipif(
    sys.platform == "win32", reason="file permissions do not deny reads on Windows"
)
def test_sync_reports_a_file_it_cannot_read(new_lore_repo):
    """A file the sync cannot read compares as unmodified, so the incoming change is applied
    and the write is what fails. The failure must name the file rather than report local
    changes or quietly leave the file behind its revision.

    The file is edited to the same size as the incoming one first, so nothing about the
    outcome rests on the sizes differing.
    """
    repo: Lore = new_lore_repo()

    size = 4096
    name = "unreadable.bin"
    with repo.open_file(name, "w+b") as f:
        f.write(os.urandom(size))
    repo.stage(scan=True)
    repo.commit("Base")
    repo.push()

    clone: Lore = repo.clone()

    with repo.open_file(name, "w+b") as f:
        f.write(os.urandom(size))
    repo.stage(scan=True)
    repo.commit("Incoming")
    repo.push()

    clone_file_path = os.path.join(clone.path, name)
    with clone.open_file(name, "w+b") as f:
        f.write(os.urandom(size))
    os.chmod(clone_file_path, 0o000)
    try:
        with pytest.raises(UnknownLoreError) as failure:
            clone.sync()
        message = str(failure.value)
        assert f"Failed to sync file {name}" in message, (
            f"the failure must name the file it could not read, got {message}"
        )
    finally:
        os.chmod(clone_file_path, 0o644)


@pytest.mark.smoke
def test_sync_remote_explicit_revision(new_lore_repo):
    """
    Syncing to a revision named with --remote advances the local branch latest
    to it, leaving the branch standing at the remote revision it was synced to
    rather than behind it.
    """
    repo = new_lore_repo()

    with repo.open_file("file1.txt", "w+") as f:
        f.write("v1")
    repo.stage("file1.txt")
    repo.commit("Commit 1")
    repo.push()

    # Cloned ahead of the merge below, so the clone's branch latest stands behind the
    # remote's rather than at it.
    clone = repo.clone()
    base_revision = clone.branch_info("main").local_latest

    repo.branch_create("feature")
    with repo.open_file("file2.txt", "w+") as f:
        f.write("v1")
    repo.stage("file2.txt")
    repo.commit("Feature commit")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge_start("feature", message="Merge feature into main")
    repo.push()

    merge_revision = repo.branch_info("main").local_latest
    assert merge_revision != base_revision

    before = clone.branch_info("main")
    assert before.local_latest == base_revision
    assert before.remote_latest == merge_revision

    clone.sync(merge_revision, remote=True)

    after = clone.branch_info("main")
    assert after.local_latest == merge_revision
    assert after.local_latest == after.remote_latest


def _two_pushed_revisions(repo):
    with repo.open_file("file1.txt", "w+") as f:
        f.write("v1")
    repo.stage("file1.txt")
    repo.commit("Commit 1")
    repo.push()

    base_revision = repo.branch_info("main").local_latest

    with repo.open_file("file1.txt", "w") as f:
        f.write("v2")
    repo.stage("file1.txt")
    repo.commit("Commit 2")
    repo.push()

    tip_revision = repo.branch_info("main").local_latest
    assert tip_revision != base_revision
    return base_revision, tip_revision


@pytest.mark.smoke
def test_sync_remote_historical_revision_keeps_latest(new_lore_repo):
    """
    Syncing back to a revision the branch latest already stands ahead of keeps that
    latest: the revision is taken without the branch being recorded behind where it
    already reached.
    """
    repo = new_lore_repo()
    base_revision, tip_revision = _two_pushed_revisions(repo)

    # Cloned at the tip, so the latest stands ahead of the revision synced to
    # below and where the pointer ends up is visible.
    clone = repo.clone()
    assert clone.branch_info("main").local_latest == tip_revision

    clone.sync(base_revision, remote=True)

    after = clone.branch_info("main")
    assert after.local_latest == tip_revision
    assert after.remote_latest == tip_revision


def _clone_with_unpushed_commit(repo, tip_revision):
    clone = repo.clone()

    # Committed and not pushed, which is what stands the branch divergent.
    with clone.open_file("local.txt", "w+") as f:
        f.write("local")
    clone.stage("local.txt")
    clone.commit("Local commit")

    local_revision = clone.branch_info("main").local_latest
    assert local_revision != tip_revision
    return clone, local_revision


@pytest.mark.smoke
def test_sync_remote_historical_revision_keeps_divergent_latest(new_lore_repo):
    """
    A branch holding a revision the remote does not keeps its latest when syncing
    back to an earlier remote revision, that revision staying reachable.
    """
    repo = new_lore_repo()
    base_revision, tip_revision = _two_pushed_revisions(repo)
    clone, local_revision = _clone_with_unpushed_commit(repo, tip_revision)

    clone.sync(base_revision, remote=True)

    assert clone.branch_info("main").local_latest == local_revision


@pytest.mark.smoke
def test_sync_remote_tip_keeps_divergent_latest(new_lore_repo):
    """
    A branch holding a revision the remote does not keeps its latest when syncing
    to the remote's own tip: the revision is taken, and the branch is not recorded
    as convergent on a tip that does not carry it.
    """
    repo = new_lore_repo()
    _base_revision, tip_revision = _two_pushed_revisions(repo)
    clone, local_revision = _clone_with_unpushed_commit(repo, tip_revision)

    clone.sync(tip_revision, remote=True)

    assert clone.branch_info("main").local_latest == local_revision

    # Still divergent, so a sync given no revision stages a merge and leaves the latest
    # on the unpushed revision. A branch recorded convergent would advance onto the tip
    # instead, leaving that revision behind.
    clone.sync()
    assert clone.branch_info("main").local_latest == local_revision


@pytest.mark.smoke
def test_sync_layer_matched_revision_keeps_latest(new_lore_repo):
    """
    Where a layer carries the sync target back to the nearest main revision it matches,
    the branch latest is read off that revision. It stands ahead of the matched one, so
    it keeps where it is rather than being rewound onto it.
    """
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")

    # Both take the default commit message, which is what the layer matches on.
    repo.write_commit_push(None, {"main.txt": b"v1"})
    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {"lay/data.txt": b"initial"})
    repo.layer_add("lay", layer_repo, "lay/", metadata="message")

    matched_revision = repo.branch_info("main").local_latest

    # Main revisions the layer matches nothing of, so matching has to walk back.
    for i in (2, 3):
        with repo.open_file("main.txt", "wb") as f:
            f.write(f"v{i}".encode())
        repo.stage(scan=True)
        repo.commit(f"no-match-{i}", non_interactive=True)
    repo.push()

    latest_revision = repo.branch_info("main").local_latest
    assert latest_revision != matched_revision

    # Pushed from elsewhere, so the revision asked for below stands ahead of the
    # latest this instance holds and would advance it on its own.
    other = repo.clone()
    other.write_commit_push("no-match-4", {"main.txt": b"v4"})
    requested_revision = other.branch_info("main").local_latest
    assert requested_revision != latest_revision

    repo.sync(requested_revision, search_nearest=True)

    # The layer carried the target back, which is what puts the latest at risk.
    assert repo.revision_info().signature == matched_revision

    assert repo.branch_info("main").local_latest == latest_revision


@pytest.mark.smoke
def test_sync_revision_advances_latest_without_remote_flag(new_lore_repo):
    """
    Syncing to the remote's tip advances the branch latest without --remote being
    given: a search of both the remote and the local history reads the remote too.
    A sync under --local advances nothing, having no remote answer to stand on.
    """
    repo = new_lore_repo()
    base_revision, tip_revision = _two_pushed_revisions(repo)

    clone = repo.clone(revision=base_revision)
    assert clone.branch_info("main").local_latest == base_revision

    # Carries the working tree to the tip and leaves the latest behind, there being no
    # remote answer to record it against.
    clone.sync(tip_revision, local=True)
    assert clone.branch_info("main").local_latest == base_revision

    # The tree already stands at the revision, so the latest is all a sync has left to
    # record. A dry run records nothing.
    clone.sync(tip_revision, dry_run=True)
    assert clone.branch_info("main").local_latest == base_revision

    clone.sync(tip_revision)

    after = clone.branch_info("main")
    assert after.local_latest == tip_revision
    assert after.local_latest == after.remote_latest


@pytest.mark.smoke
def test_sync_remote_tip_behind_latest_keeps_latest(new_lore_repo):
    """
    A remote carried back to an earlier revision leaves a tip numbered below the branch
    latest. Syncing to that tip takes the revision without standing the latest back on
    it, the revisions the branch already tracks staying tracked.
    """
    repo = new_lore_repo()

    for i in (1, 2, 3):
        with repo.open_file("file1.txt", "w+") as f:
            f.write(f"v{i}")
        repo.stage("file1.txt")
        repo.commit(f"Commit {i}")
    repo.push()

    first_revision = repo.revision_info("main@1").signature
    latest_revision = repo.branch_info("main").local_latest

    # Cloned at the third revision, so the latest stands ahead of the tip left below.
    clone = repo.clone()
    assert clone.branch_info("main").local_latest == latest_revision

    repo.branch_reset(first_revision)
    with repo.open_file("file1.txt", "w") as f:
        f.write("v2 again")
    repo.stage("file1.txt")
    repo.commit("Commit 2 again")
    repo.push(force=True)

    replacement_tip = repo.branch_info("main").local_latest
    assert replacement_tip != latest_revision
    assert repo.revision_info(replacement_tip).revision == "2"

    clone.sync(replacement_tip, remote=True)

    assert clone.branch_info("main").local_latest == latest_revision


@pytest.mark.smoke
def test_sync_remote_dropped_revision_keeps_latest(new_lore_repo):
    """
    A revision the remote no longer holds at its number does not become the branch
    latest, however far ahead of that latest it is numbered. A whole hash is not
    looked up, so only the remote's own answer says it still stands there.
    """
    repo = new_lore_repo()

    with repo.open_file("file1.txt", "w+") as f:
        f.write("v1")
    repo.stage("file1.txt")
    repo.commit("Commit 1")
    repo.push()

    base_revision = repo.branch_info("main").local_latest

    with repo.open_file("file1.txt", "w") as f:
        f.write("v2")
    repo.stage("file1.txt")
    repo.commit("Commit 2")
    repo.push()

    dropped_revision = repo.branch_info("main").local_latest
    assert dropped_revision != base_revision

    # Pinned behind the revision dropped below, so that revision is numbered ahead of
    # the latest and would advance it were the remote still holding it.
    clone = repo.clone(revision=base_revision)
    assert clone.branch_info("main").local_latest == base_revision

    # The remote branch is carried back and taken forward again, so what it holds at
    # the dropped revision's number is a different revision.
    repo.branch_reset(base_revision)
    with repo.open_file("file1.txt", "w") as f:
        f.write("v2 again")
    repo.stage("file1.txt")
    repo.commit("Commit 2 again")
    repo.push(force=True)

    replacement_revision = repo.branch_info("main").local_latest
    assert replacement_revision != dropped_revision

    clone.sync(dropped_revision, remote=True)

    assert clone.branch_info("main").local_latest == base_revision


# Revisions between the branch latest and the revision synced to, enough of them that a
# decision scaling with the distance between the two would show it.
_DEEP_HISTORY_REVISIONS = 1100


@pytest.mark.slow
def test_sync_remote_revision_deep_behind_tip_advances_latest(new_lore_repo):
    """
    A revision ahead of the branch latest advances it however far behind the remote's
    tip it stands.
    """
    repo = new_lore_repo()

    with repo.open_file("file1.txt", "w+") as f:
        f.write("v1")
    repo.stage("file1.txt")
    repo.commit("Commit 1")
    repo.push()

    base_revision = repo.branch_info("main").local_latest

    with repo.open_file("bulk.txt", "w+") as f:
        f.write("rev 0\n")
    repo.stage("bulk.txt", offline=True)
    repo.commit("Bulk rev 0", offline=True)
    second_revision = repo.revision_info().signature

    for i in range(1, _DEEP_HISTORY_REVISIONS):
        with repo.open_file("bulk.txt", "w+") as f:
            f.write(f"rev {i}\n")
        repo.stage("bulk.txt", offline=True)
        repo.commit(f"Bulk rev {i}", offline=True)
    repo.push()

    tip_revision = repo.branch_info("main").local_latest
    assert tip_revision not in (base_revision, second_revision)

    # Pinned at the first revision, so the revision synced to below stands one ahead
    # of the latest and the whole bulk history behind the remote's tip.
    clone = repo.clone(revision=base_revision)
    assert clone.branch_info("main").local_latest == base_revision

    clone.sync(second_revision, remote=True)

    after = clone.branch_info("main")
    assert after.local_latest == second_revision
    assert after.remote_latest == tip_revision


@pytest.mark.smoke
def test_sync_remote_revision_merged_from_divergence_keeps_latest(new_lore_repo):
    """
    A revision a merge carried onto the branch stands behind the latest, and shares its
    number with the revision the remote holds at that number, the divergence the merge
    resolved having numbered the two alike. The latest keeps where it is on either
    count.
    """
    repo = new_lore_repo()

    with repo.open_file("file1.txt", "w+") as f:
        f.write("v1")
    repo.stage("file1.txt")
    repo.commit("Commit 1")
    repo.push()

    clone = repo.clone()

    with repo.open_file("file1.txt", "w") as f:
        f.write("v2")
    repo.stage("file1.txt")
    repo.commit("Commit 2")
    repo.push()

    # A commit on the same branch that the remote has moved past, so the sync
    # below merges rather than advancing, and the merge carries this revision as
    # a parent other than its first.
    with clone.open_file("file2.txt", "w+") as f:
        f.write("local")
    clone.stage("file2.txt")
    clone.commit("Divergent commit", local=True)
    merged_revision = clone.branch_info("main").local_latest

    clone.sync()
    clone.push()

    merge_revision = clone.branch_info("main").local_latest
    assert merge_revision != merged_revision

    # Reachable past the first parent alone, which is what a first-parent walk
    # misses and this test exists for. A merge reports both its parents on one
    # line, the first of them the one such a walk follows.
    merge_parents = clone.revision_info(merge_revision).merge.split()
    assert len(merge_parents) == 2
    assert merge_parents[0] != merged_revision
    assert merge_parents[1] == merged_revision

    # Cloned at the merge, so the latest stands ahead of the revision synced to.
    other = repo.clone()
    assert other.branch_info("main").local_latest == merge_revision

    other.sync(merged_revision, remote=True)

    after = other.branch_info("main")
    assert after.local_latest == merge_revision
    assert after.remote_latest == merge_revision
