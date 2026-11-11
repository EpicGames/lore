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
from lore_parsers import parse_jsonl, parse_status_json, parse_status_summary_json
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


@pytest.mark.own_vfs("re-clones over the files an ordinary clone leaves on disk")
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
    clone = repo.clone(offline=True)
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
    clone = repo.clone(offline=True)
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


def _commit_move(repo: Lore, modified: bool):
    """Commits and pushes a move of `moved/from.txt` to `moved/to.txt`, with other content where
    `modified`."""
    repo.move("moved/from.txt", "moved/to.txt")
    repo.stage_move("moved/from.txt", "moved/to.txt")
    if modified:
        repo.write_files({"moved/to.txt": "incoming\n"})
        repo.stage("moved/to.txt")
    repo.commit("Move")
    repo.push()


@pytest.mark.smoke
@pytest.mark.parametrize("modified", [False, True], ids=["move", "move and modify"])
def test_sync_refuses_a_move_of_a_locally_edited_file(new_lore_repo, modified):
    """An edit at the path an incoming move carries a file from is local work, refused as an
    edit at the path it moves to is, whether or not the move changes the content."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("Base", {"moved/from.txt": "base\n", "other.txt": "o\n"})
    clone: Lore = repo.clone()
    _commit_move(repo, modified)
    clone.write_files({"moved/from.txt": "locally edited\n"})

    with pytest.raises(LocalChanges):
        clone.sync()

    with clone.open_file("moved/from.txt") as f:
        assert f.read() == "locally edited\n", "the refused sync should keep the edit"
    assert not clone.file_exists("moved/to.txt")


@pytest.mark.smoke
@pytest.mark.skipif(
    sys.platform != "linux", reason="Requires case-sensitive filesystem"
)
def test_sync_refuses_a_directory_rename_in_case_onto_a_locally_edited_file(
    new_lore_repo,
):
    """An edit to a file in a directory an incoming move renames in case, which changes the file
    too, is local work the sync refuses, and keeps. The file's change is no move of its own, so
    the sync finds the edit at the path the file stood at below the renamed directory."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("Base", {"dir/a.txt": "base\n", "dir/b.txt": "b\n"})
    clone: Lore = repo.clone()
    repo.move("dir", "Dir")
    repo.stage_move("dir", "Dir")
    repo.write_files({"Dir/a.txt": "incoming\n"})
    repo.stage("Dir/a.txt")
    repo.commit("Rename the directory in case")
    repo.push()
    clone.write_files({"dir/a.txt": "locally edited\n"})

    with pytest.raises(LocalChanges):
        clone.sync()

    with clone.open_file("dir/a.txt") as f:
        assert f.read() == "locally edited\n", "the refused sync should keep the edit"
    assert not clone.path_exists("Dir")


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


@pytest.mark.own_vfs("counts the hash checks and modified-time matches a scan of an ordinary working tree makes, where a virtual clone's status diffs by address")
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
def test_sync_prefetches_the_tree_only_when_the_instance_asks(new_lore_repo):
    """Prefetching fetches the target's whole tree whatever the view, so a sync does it only
    under `[sync] prefetch_tree = true`."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("Base", {"root.txt": b"base"})
    clone: Lore = repo.clone()

    repo.write_commit_push("Second", {"root.txt": b"second"})
    output = clone.sync(level="debug")
    assert "Prefetched" not in output, output

    with open(
        os.path.join(clone.dot_path(), "config.toml"), "a", encoding="utf-8"
    ) as config:
        config.write("\n[sync]\nprefetch_tree = true\n")
    repo.write_commit_push("Third", {"root.txt": b"third"})
    output = clone.sync(level="debug")
    assert "Prefetched" in output, output
    assert _read_bytes(clone, "root.txt") == b"third"


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
@pytest.mark.own_vfs("denies reads through file permissions, where a virtual clone refuses a local file it cannot compare as a local change")
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


def _edit_then_dirty(repo: Lore) -> None:
    with repo.open_file("other.txt", "w") as f:
        f.write("edited during the merge")
    repo.dirty("other.txt")


def _edit_then_stage(repo: Lore) -> None:
    with repo.open_file("other.txt", "w") as f:
        f.write("edited during the merge")
    repo.stage("other.txt")


def _move_then_dirty(repo: Lore) -> None:
    repo.move("other.txt", "moved.txt")
    repo.dirty_move("other.txt", "moved.txt")


def _move_then_stage(repo: Lore) -> None:
    repo.move("other.txt", "moved.txt")
    repo.file_stage_move("other.txt", "moved.txt")


def _set_revision_metadata(repo: Lore) -> None:
    repo.revision_metadata_set(["co-author", "Co Author <co.author@example.com>"])


def _add_file_dependency(repo: Lore) -> None:
    repo.file_dependency_add("other.txt", "conflict.txt")


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("edit", "force"),
    [
        pytest.param(None, False, id="resolve-only"),
        pytest.param(_edit_then_dirty, False, id="dirty"),
        pytest.param(_edit_then_stage, False, id="stage"),
        pytest.param(_move_then_dirty, False, id="dirty-move"),
        pytest.param(_move_then_stage, False, id="stage-move"),
        pytest.param(_set_revision_metadata, False, id="revision-metadata"),
        pytest.param(_add_file_dependency, False, id="file-dependency"),
        pytest.param(None, True, id="forced-commit"),
    ],
)
def test_sync_merge_of_divergent_history_keeps_the_remote_latest_first(
    new_lore_repo, edit, force
):
    """
    A sync that merges divergent history takes the remote latest as the merge's first
    parent and the local latest as its second. Whatever is changed on top of the merge
    before it is committed keeps both, so the merge revision descends from the remote
    latest and pushes.
    """
    repo = new_lore_repo()

    for name in ("conflict.txt", "other.txt"):
        with repo.open_file(name, "w+") as f:
            f.write("base")
    repo.stage(["conflict.txt", "other.txt"])
    repo.commit("Base")
    repo.push()

    clone = repo.clone()

    with repo.open_file("conflict.txt", "w") as f:
        f.write("remote")
    repo.stage("conflict.txt")
    repo.commit("Remote change")
    repo.push()
    remote_latest = repo.branch_info("main").local_latest

    with clone.open_file("conflict.txt", "w") as f:
        f.write("local")
    clone.stage("conflict.txt")
    clone.commit("Local change", local=True)
    local_latest = clone.branch_info("main").local_latest

    # The conflict stops the sync before it commits the merge, which leaves the merge
    # staged for the change below.
    clone.sync()
    with clone.open_file("conflict.txt", "w") as f:
        f.write("resolved")
    clone.branch_merge_resolve("conflict.txt")

    if edit:
        edit(clone)
    clone.commit("Merge", force=force)

    merge_revision = clone.branch_info("main").local_latest
    merge_parents = clone.revision_info(merge_revision).merge.split()
    assert merge_parents == [remote_latest, local_latest]

    clone.push()


@pytest.mark.smoke
@pytest.mark.parametrize(
    "conflict", [False, True], ids=["sync-commit", "resolved-conflict"]
)
def test_sync_merge_of_divergent_history_leaves_the_branch_ahead_of_the_remote(
    new_lore_repo, conflict
):
    """
    A committed merge of divergent history holds the remote latest, so until it is pushed the
    branch is ahead of the remote and not behind it.
    """
    repo = new_lore_repo()

    for name in ("conflict.txt", "other.txt"):
        with repo.open_file(name, "w+") as f:
            f.write("base")
    repo.stage(["conflict.txt", "other.txt"])
    repo.commit("Base")
    repo.push()

    clone = repo.clone()

    with repo.open_file("conflict.txt", "w") as f:
        f.write("remote")
    repo.stage("conflict.txt")
    repo.commit("Remote change")
    repo.push()

    local_file = "conflict.txt" if conflict else "local.txt"
    with clone.open_file(local_file, "w") as f:
        f.write("local")
    clone.stage(local_file)
    clone.commit("Local change", local=True)

    clone.sync()
    if conflict:
        with clone.open_file("conflict.txt", "w") as f:
            f.write("resolved")
        clone.branch_merge_resolve("conflict.txt")
        clone.commit("Merge")

    [status] = parse_jsonl(clone.status(json=True), "repositoryStatusRevision")
    assert status["isLocalAhead"] == 1
    assert status["isRemoteAhead"] == 0


@pytest.mark.own_vfs("counts the hash checks and modified-time matches a scan of an ordinary working tree makes, where a virtual clone's status diffs by address")
@pytest.mark.smoke
def test_sync_records_the_time_of_a_file_holding_the_incoming_content_only_before_its_clock(
    new_lore_repo,
):
    """A sync that finds a file already holding the incoming content records its time only where
    that time is older than the filesystem clock the sync read before reading the file: a write
    after the read may share a later one. The scan after the sync answers for the older file by
    its recorded time and reads the other."""
    repo: Lore = new_lore_repo()
    names = ("older.bin", "ahead.bin")
    for name in names:
        with repo.open_file(name, "w+b") as output_file:
            output_file.write(os.urandom(4096))
    repo.stage(scan=True)
    repo.commit("First content")
    repo.push()
    first = repo.revision_info().signature

    # The same size, so only the recorded time or the content can tell the files apart.
    incoming = {name: os.urandom(4096) for name in names}
    for name, content in incoming.items():
        with repo.open_file(name, "w+b") as output_file:
            output_file.write(content)
    repo.stage(scan=True)
    repo.commit("Second content")
    repo.push()

    repo.sync(first)
    for name, content in incoming.items():
        with repo.open_file(name, "w+b") as output_file:
            output_file.write(content)
    now = time.time()
    os.utime(os.path.join(repo.path, "older.bin"), (now - 3600, now - 3600))
    os.utime(os.path.join(repo.path, "ahead.bin"), (now + 3600, now + 3600))
    repo.sync()

    output = repo.status(scan=True, json=True)
    assert [
        entry for entry in parse_status_json(output) if to_posix(entry["path"]) in names
    ] == [], "the files hold the synced content"
    summary = parse_status_summary_json(output)
    assert summary is not None, "scan must emit a repositoryStatusSummary event"
    assert (summary["hashChecks"], summary["mtimeMatches"]) == (1, 1), (
        f"the sync must record the older time and not the one ahead, got {summary}"
    )


def _file_history(repo: Lore, path: str) -> list[dict]:
    """The entries of the history of `path`, newest first."""
    return parse_jsonl(repo.file_history(path, json=True), "fileHistory")


@pytest.mark.smoke
def test_sync_merge_of_divergent_history_keeps_the_remote_history_off_a_local_file(
    new_lore_repo,
):
    """
    A sync that merges divergent history builds the merge from the local latest. A file
    the remote latest added and a file the local latest added can have the same node ID
    in the two, and the remote's addition is not recorded on the local file, whose history
    is its own addition.
    """
    repo = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base")
    repo.stage("base.txt")
    repo.commit("Base")
    repo.push()

    clone = repo.clone()

    with repo.open_file("remote.txt", "w+") as f:
        f.write("remote")
    repo.stage("remote.txt")
    repo.commit("Add a remote file")
    repo.push()
    remote_latest = repo.branch_info("main").local_latest

    with clone.open_file("local.txt", "w+") as f:
        f.write("local")
    clone.stage("local.txt")
    clone.commit("Add a local file", local=True)
    local_latest = clone.branch_info("main").local_latest

    # Neither side changed what the other did, so the sync commits the merge.
    clone.sync()
    merge_revision = clone.branch_info("main").local_latest
    assert clone.revision_info(merge_revision).merge.split() == [
        remote_latest,
        local_latest,
    ]

    history = _file_history(clone, "local.txt")
    assert [
        (entry["revision"], entry["path"], entry["action"]) for entry in history
    ] == [(local_latest, "local.txt", "add")]


@pytest.mark.smoke
def test_sync_merge_of_divergent_history_records_the_remote_add_of_a_file_both_sides_add(
    new_lore_repo,
):
    """
    Both sides add the same path after they diverge, and the remote latest gives the file
    another node ID than the local latest does, having added a file before it. The merge
    records the remote's addition on the file, so its history reaches the remote latest
    at the path the remote added it at.
    """
    repo = new_lore_repo()

    with repo.open_file("base.txt", "w+") as f:
        f.write("base")
    repo.stage("base.txt")
    repo.commit("Base")
    repo.push()

    clone = repo.clone()

    with repo.open_file("earlier.txt", "w+") as f:
        f.write("earlier")
    repo.stage("earlier.txt")
    repo.commit("Add an earlier remote file")
    repo.push()
    with repo.open_file("shared.txt", "w+") as f:
        f.write("remote")
    repo.stage("shared.txt")
    repo.commit("Add the shared file remotely")
    repo.push()
    remote_latest = repo.branch_info("main").local_latest

    with clone.open_file("shared.txt", "w+") as f:
        f.write("local")
    clone.stage("shared.txt")
    clone.commit("Add the shared file locally", local=True)
    local_latest = clone.branch_info("main").local_latest

    # Both sides added the file, which stops the sync on a conflict.
    clone.sync()
    with clone.open_file("shared.txt", "w") as f:
        f.write("resolved")
    clone.branch_merge_resolve("shared.txt")
    clone.commit("Merge")
    merge_revision = clone.branch_info("main").local_latest
    assert clone.revision_info(merge_revision).merge.split() == [
        remote_latest,
        local_latest,
    ]

    history = _file_history(clone, "shared.txt")
    assert [(entry["revision"], entry["path"]) for entry in history] == [
        (merge_revision, "shared.txt"),
        (remote_latest, "shared.txt"),
    ]
    assert history[-1]["action"] == "add"


@pytest.mark.smoke
def test_sync_merge_of_divergent_history_keeps_the_remote_change_before_its_latest(
    new_lore_repo,
):
    """
    A sync merges divergent history where the remote changed a file in a revision before
    its latest. The history of the file names that revision, as it does on the remote.
    """
    repo = new_lore_repo()

    with repo.open_file("changed.txt", "w+") as f:
        f.write("base")
    repo.stage("changed.txt")
    repo.commit("Base")
    repo.push()
    base = repo.branch_info("main").local_latest

    clone = repo.clone()

    with repo.open_file("changed.txt", "w") as f:
        f.write("remote")
    repo.stage("changed.txt")
    repo.commit("Change the file remotely")
    repo.push()
    remote_change = repo.branch_info("main").local_latest
    with repo.open_file("added.txt", "w+") as f:
        f.write("remote")
    repo.stage("added.txt")
    repo.commit("Add a remote file")
    repo.push()

    with clone.open_file("local.txt", "w+") as f:
        f.write("local")
    clone.stage("local.txt")
    clone.commit("Add a local file", local=True)

    # Neither side changed what the other did, so the sync commits the merge.
    clone.sync()
    merge_revision = clone.branch_info("main").local_latest

    history = _file_history(clone, "changed.txt")
    assert [entry["revision"] for entry in history] == [
        merge_revision,
        remote_change,
        base,
    ]
