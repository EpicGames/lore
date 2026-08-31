# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_push(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"
    other_file = "some_other.uasset"
    large_file = "some_large_file.uasset"
    my_feature_file = "my_feature_file.uasset"
    my_mega_feature_file = "my_mega_feature_file.uasset"

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    with repo.open_file(other_file, "w+b") as output_file:
        output_file.write(os.urandom(1234))

    # Generate a larger file
    with repo.open_file(large_file, "w+b") as output_file:
        output_file.write(os.urandom(345678901))

    # Stage the files
    repo.stage(scan=True)

    # Commit offline
    repo.commit("Test commit", offline=True)

    # Create the feature branch without pushing the main branch
    repo.branch_create("my-feature", offline=True)

    # Generate another large file
    with repo.open_file(my_feature_file, "w+b") as output_file:
        output_file.write(os.urandom(345678901))

    # Stage the repository again
    repo.stage(scan=True)

    # Commit the files again
    repo.commit("My feature commit", offline=True)

    # Create the mega feature branch
    repo.branch_create("my-mega-feature", offline=True)

    # Generate another large file
    with repo.open_file(my_mega_feature_file, "w+b") as output_file:
        output_file.write(os.urandom(345678901))

    # Stage the repository again
    repo.stage(scan=True)

    # Commit the files again
    repo.commit("My mega feature commit", offline=True)

    # Push the feature branch
    repo.push()

    # Push the feature branch again
    output = repo.push()
    assert "Revision is already pushed" in output, (
        "Missing message that nothing needs to be pushed"
    )

    # Clone the repository
    clone = repo.clone()

    # Switch to my-mega-feature branch
    clone.branch_switch("my-mega-feature")

    # Verify files contents, mode and last modified timestamp
    assert repo.compare_file(clone, text_file)
    assert repo.compare_file(clone, other_file)
    assert repo.compare_file(clone, large_file)
    assert repo.compare_file(clone, my_feature_file)
    assert repo.compare_file(clone, my_mega_feature_file)

    # Try pushing the main branch while current branch is the feature branch
    output = repo.branch_info("main")
    local_latest = output.local_latest
    remote_latest = output.remote_latest
    assert local_latest != remote_latest, (
        "Local and remote LATEST are unexpectedly the same"
    )

    repo.push("main")

    output = repo.branch_info("main")
    local_latest = output.local_latest
    remote_latest = output.remote_latest
    assert local_latest == remote_latest, (
        "Local and remote LATEST are unexpectedly different"
    )


def _collect_repo_files(repo: Lore) -> set[str]:
    """Collect all file paths in a repo, relative to root, excluding .lore directory."""
    files = set()
    for dirpath, dirnames, filenames in os.walk(repo.path):
        dirnames[:] = [d for d in dirnames if d not in (".urc", ".lore")]
        for filename in filenames:
            rel = os.path.relpath(os.path.join(dirpath, filename), repo.path)
            files.add(rel.replace("\\", "/"))
    return files


@pytest.mark.smoke
def test_push_fast_forward_merge(new_lore_repo):
    """Fast-forward merge push succeeds when target branch head has moved with
    non-conflicting changes, and subsequent sync resolves cleanly."""
    repo: Lore = new_lore_repo()

    # Create initial content with a deep directory tree on main
    repo.make_dirs(os.path.join("src", "core", "utils"))
    with repo.open_file(os.path.join("src", "core", "utils", "helpers.txt"), "w+") as f:
        f.write("initial helpers content\n")
    with repo.open_file(os.path.join("src", "core", "engine.txt"), "w+") as f:
        f.write("initial engine content\n")
    with repo.open_file("readme.txt", "w+") as f:
        f.write("initial readme\n")
    repo.stage(scan=True)
    repo.commit("Initial commit with deep tree", offline=True)
    repo.push()

    # Create a feature branch with changes in the deep tree
    repo.branch_create("feature-branch")
    repo.make_dirs(os.path.join("src", "core", "utils", "extra"))
    with repo.open_file(os.path.join("src", "core", "utils", "extra", "new_util.txt"), "w+") as f:
        f.write("new utility from feature branch\n")
    with repo.open_file(os.path.join("src", "core", "utils", "helpers.txt"), "w+") as f:
        f.write("modified helpers from feature branch\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Feature branch changes in deep tree", offline=True)
    repo.push()

    # Switch back to main (no additional changes on main before the merge)
    repo.branch_switch("main")

    # Merge the feature branch into main locally
    repo.branch_merge_start("feature-branch")

    # At this point the merge commit's parent_self = main rev 1 (the initial push).
    # Now advance main from another clone to simulate a concurrent push.
    clone_b = repo.clone()
    clone_b.make_dirs(os.path.join("assets", "textures", "hdr"))
    with clone_b.open_file(os.path.join("assets", "textures", "hdr", "sky.bin"), "w+b") as f:
        f.write(os.urandom(4096))
    clone_b.make_dirs(os.path.join("docs", "api", "v2"))
    with clone_b.open_file(os.path.join("docs", "api", "v2", "reference.txt"), "w+") as f:
        f.write("API reference docs\n")
    clone_b.stage(scan=True, offline=True)
    clone_b.commit("Concurrent push from clone B", offline=True)
    clone_b.push()

    # Record the local revision number before the fast-forward merge push
    pre_push_revision = int(repo.revision_info().revision)

    # Push from the original repo with fast-forward merge — the server should
    # create a new merge revision combining the merge and clone_b's changes
    repo.push(fast_forward_merge=True)

    # Sync to pick up the server-created revision
    repo.sync()

    # The server creates a new revision for the fast-forward merge, so the
    # resulting revision number must be strictly greater than what we pushed
    post_sync_revision = int(repo.revision_info().revision)
    assert post_sync_revision > pre_push_revision, (
        f"Expected revision after fast-forward merge ({post_sync_revision}) "
        f"to be greater than the pushed revision ({pre_push_revision})"
    )

    # The expected set of files after fast-forward merge includes content from:
    # - initial commit (engine.txt, readme.txt)
    # - feature branch (helpers.txt modified, extra/new_util.txt added)
    # - concurrent push from clone_b (sky.bin, reference.txt)
    expected_files = {
        "src/core/utils/helpers.txt",
        "src/core/utils/extra/new_util.txt",
        "src/core/engine.txt",
        "readme.txt",
        "docs/api/v2/reference.txt",
        "assets/textures/hdr/sky.bin",
    }

    # Verify local repo after sync has exactly the expected files
    local_files = _collect_repo_files(repo)
    assert local_files == expected_files, (
        f"Local files after sync differ from expected.\n"
        f"  Extra: {local_files - expected_files}\n"
        f"  Missing: {expected_files - local_files}"
    )

    # Verify via a fresh clone that the server state is correct
    verify = repo.clone()
    clone_files = _collect_repo_files(verify)
    assert clone_files == expected_files, (
        f"Cloned files differ from expected.\n"
        f"  Extra: {clone_files - expected_files}\n"
        f"  Missing: {expected_files - clone_files}"
    )

    # Verify file contents match between synced repo and fresh clone
    for file_path in expected_files:
        assert repo.compare_file(verify, file_path), (
            f"File content mismatch: {file_path}"
        )

    # Verify the server-created revision carries the fast-forward-merge metadata
    ff_metadata = repo.revision_metadata_get("fast-forward-merge")
    assert ff_metadata.strip(), (
        "Expected 'fast-forward-merge' metadata to be set on the server-created revision"
    )

    # Verify merged-by is present (server fallback or preserved from incoming)
    merged_by = repo.revision_metadata_get("merged-by")
    assert merged_by.strip(), (
        "Expected 'merged-by' metadata to be set on the server-created revision"
    )

    # Also verify on a fresh clone
    ff_metadata_clone = verify.revision_metadata_get("fast-forward-merge")
    assert ff_metadata_clone.strip(), (
        "Expected 'fast-forward-merge' metadata on fresh clone's latest revision"
    )


@pytest.mark.smoke
def test_push_fast_forward_merge_conflict(new_lore_repo):
    """Fast-forward merge push fails when the concurrent push modifies a file
    that was also changed by the merge, producing a conflict in the server-side
    diff3."""
    repo: Lore = new_lore_repo()

    # Create initial content with deep tree
    repo.make_dirs(os.path.join("src", "core", "utils"))
    with repo.open_file(os.path.join("src", "core", "utils", "shared.txt"), "w+") as f:
        f.write("initial shared content\n")
    repo.stage(scan=True)
    repo.commit("Initial commit", offline=True)
    repo.push()

    # Create feature branch that modifies the shared file
    repo.branch_create("feature-conflict")
    with repo.open_file(os.path.join("src", "core", "utils", "shared.txt"), "w+") as f:
        f.write("feature branch modified shared content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Feature modifies shared file", offline=True)
    repo.push()

    # Switch to main (no local changes) and merge the feature branch cleanly
    repo.branch_switch("main")
    repo.branch_merge_start("feature-conflict")

    # Now advance main from another clone by modifying the same file — this
    # creates a conflict between the merge (which changed shared.txt via the
    # feature branch) and the concurrent push (which also changes shared.txt)
    clone_b = repo.clone()
    with clone_b.open_file(os.path.join("src", "core", "utils", "shared.txt"), "w+") as f:
        f.write("concurrent change to shared file on main\n")
    clone_b.stage(scan=True, offline=True)
    clone_b.commit("Concurrent conflicting push", offline=True)
    clone_b.push()

    # Fast-forward merge push should fail due to conflict in server-side diff3
    with pytest.raises(Exception):
        repo.push(fast_forward_merge=True)


@pytest.mark.smoke
def test_push_fast_forward_merge_non_merge(new_lore_repo):
    """Fast-forward merge also works for non-merge (regular) revisions when the
    target branch head has moved with non-conflicting changes."""
    repo: Lore = new_lore_repo()

    # Create initial content with a deep tree
    repo.make_dirs(os.path.join("src", "core", "utils"))
    with repo.open_file(os.path.join("src", "core", "utils", "config.txt"), "w+") as f:
        f.write("initial config\n")
    with repo.open_file("readme.txt", "w+") as f:
        f.write("initial readme\n")
    repo.stage(scan=True)
    repo.commit("Initial", offline=True)
    repo.push()

    # Make a regular (non-merge) commit modifying a file
    with repo.open_file(os.path.join("src", "core", "utils", "config.txt"), "w+") as f:
        f.write("modified config\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Regular commit", offline=True)

    # Advance main from another clone with a non-conflicting change
    clone_b = repo.clone()
    repo_b_dir = os.path.join("docs", "guides", "setup")
    clone_b.make_dirs(repo_b_dir)
    with clone_b.open_file(os.path.join(repo_b_dir, "install.txt"), "w+") as f:
        f.write("installation guide\n")
    clone_b.stage(scan=True, offline=True)
    clone_b.commit("Concurrent push with docs", offline=True)
    clone_b.push()

    # Record the local revision number before the fast-forward merge push
    pre_push_revision = int(repo.revision_info().revision)

    # Push with --fast-forward-merge should succeed for non-merge revision too
    repo.push(fast_forward_merge=True)

    # Sync to pick up the server-created revision
    repo.sync()

    # The server creates a new revision for the fast-forward merge, so the
    # resulting revision number must be strictly greater than what we pushed
    post_sync_revision = int(repo.revision_info().revision)
    assert post_sync_revision > pre_push_revision, (
        f"Expected revision after fast-forward merge ({post_sync_revision}) "
        f"to be greater than the pushed revision ({pre_push_revision})"
    )

    # Verify all files present
    expected_files = {
        "src/core/utils/config.txt",
        "readme.txt",
        "docs/guides/setup/install.txt",
    }
    local_files = _collect_repo_files(repo)
    assert local_files == expected_files, (
        f"Local files after sync differ from expected.\n"
        f"  Extra: {local_files - expected_files}\n"
        f"  Missing: {expected_files - local_files}"
    )

    # Verify via fresh clone
    verify = repo.clone()
    clone_files = _collect_repo_files(verify)
    assert clone_files == expected_files, (
        f"Cloned files differ from expected.\n"
        f"  Extra: {clone_files - expected_files}\n"
        f"  Missing: {expected_files - clone_files}"
    )

    for file_path in expected_files:
        assert repo.compare_file(verify, file_path), (
            f"File content mismatch: {file_path}"
        )

    # Verify the server-created revision carries the fast-forward-merge metadata
    ff_metadata = repo.revision_metadata_get("fast-forward-merge")
    assert ff_metadata.strip(), (
        "Expected 'fast-forward-merge' metadata on the server-created revision"
    )


@pytest.mark.smoke
def test_push_non_current_branch_preserves_anchor(new_lore_repo):
    """Pushing a branch that is not the current branch must not corrupt the
    workspace anchor. Regression test for UCS-19529: a previous bug had
    ``branch::push`` write the pushed branch's latest revision into the
    workspace-wide ``ANCHOR_CURRENT`` slot, which then leaked into the next
    ``branch::create`` as the new branch's branch-point revision."""
    repo: Lore = new_lore_repo()

    # Initial commit on main, pushed so main exists on the remote.
    with repo.open_file("file.txt", "w+") as f:
        f.write("initial\n")
    repo.stage(scan=True)
    repo.commit("Initial commit")
    repo.push()
    main_head = repo.branch_info("main").local_latest

    # Create branch-a off main (this also switches to it).
    repo.branch_create("branch-a")

    # Local-only commit on branch-a. NOT pushed yet — this revision is
    # what the buggy anchor leak would later surface.
    with repo.open_file("file.txt", "w+") as f:
        f.write("on branch-a\n")
    repo.stage(scan=True)
    repo.commit("Branch-a local commit", offline=True)
    branch_a_head = repo.branch_info("branch-a").local_latest
    assert branch_a_head != main_head, (
        "branch-a should have advanced past main with the local commit"
    )

    # Switch back to main.
    repo.branch_switch("main")

    # Create branch-b off main (this also switches to it). Workspace anchor
    # should now describe branch-b at main_head.
    repo.branch_create("branch-b")

    # Push branch-a while checked out on branch-b. With the bug this writes
    # ANCHOR_CURRENT = branch_a_head even though the working directory is
    # branch-b at main_head.
    repo.push("branch-a")

    # Create branch-c off branch-b. Its first stack entry must record
    # branch-b at main_head, not branch-a's leaked head.
    repo.branch_create("branch-c")

    info = repo.branch_info("branch-c")
    assert info.parent.startswith("branch-b at "), (
        f"branch-c's parent should be branch-b, got {info.parent!r}"
    )
    branch_c_branch_point = info.parent.split(" at ", 1)[1].strip()
    assert branch_c_branch_point == main_head, (
        f"branch-c's branch-point should be main_head ({main_head}); "
        f"got {branch_c_branch_point} "
        f"(matches branch-a's leaked head: {branch_c_branch_point == branch_a_head})"
    )
