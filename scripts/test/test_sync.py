# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import shutil
import sys
import time

import pytest

from error_types import (
    FileAlreadyExist,
    LocalChanges,
    BranchDivergedError,
    ProtectedError,
)
from lore import Lore

logger = logging.getLogger(__name__)


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

    clone = repo.clone(direct_file_io=True)

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

    clone.compare_file(repo, text_file)
    clone.compare_file(repo, unicode_file)
    clone.compare_file(repo, added_file)

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


@pytest.mark.smoke
def test_sync_remote_explicit_revision(new_lore_repo):
    """
    Test that syncing to an explicit revision with --remote updates the
    local branch Latest pointer, preventing 'Local branch is behind remote'
    status.
    """
    repo = new_lore_repo()

    # Create some initial history
    with repo.open_file("file1.txt", "w+") as f:
        f.write("v1")
    repo.stage("file1.txt")
    repo.commit("Commit 1")

    # Create a side branch
    repo.branch_create("feature")
    repo.branch_switch("feature")
    with repo.open_file("file2.txt", "w+") as f:
        f.write("v1")
    repo.stage("file2.txt")
    repo.commit("Feature commit")
    repo.push()

    # Switch back to main, merge the feature branch
    repo.branch_switch("main")
    repo.branch_merge_start("feature", message="Merge feature into main")
    repo.push()

    # Get the client to a clean state on an older commit
    clone = repo.clone()
    clone.sync("@LATEST~1")
    # Get the latest revision hash from the remote by running branch_latest
    import re
    merge_revision_output = repo.run(["status"])
    merge_revision = re.search(r'revision \d+ -> ([a-f0-9]+)', merge_revision_output).group(1)
    # Now simulate the bug: sync explicitly to the merge revision with --remote
    clone.sync(merge_revision, remote=True)

    # Check that status does NOT complain about being behind remote
    status_output = clone.status()
    assert "Local branch is behind remote" not in status_output, "Branch Latest pointer was not updated"

    # Second sync should say Already on branch, without actually syncing anything
    sync_output = clone.sync()
    assert "Already on branch" in sync_output, "Sync did not realize we are already at latest"
