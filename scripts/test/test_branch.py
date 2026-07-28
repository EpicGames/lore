# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os
import uuid

import pytest
from error_types import (
    BranchAlreadyExistsError,
    BranchDivergedError,
    DeleteCurrentError,
    DeleteDefaultError,
    DeleteProtectedError,
    LocalChanges,
    NotFound,
    UnknownLoreError,
    ZeroRevisionError,
)
from lore_parsers import parse_branch_list_json, parse_jsonl

from lore import Lore, verify_signatures


def generate_branch_id() -> str:
    """Generate a hex-encoded 16-byte branch ID from a UUIDv4."""
    return uuid.uuid4().hex


@pytest.mark.smoke
def test_branch(new_lore_repo):
    repo: Lore = new_lore_repo()

    with pytest.raises(ZeroRevisionError):
        repo.branch_create("zero-branch")

    text_file = "text-File.txt"
    bin_file = "some_other.uasset"

    repo.write_commit_push(
        None,
        {
            text_file: ["One line\n", "Another line\n", "Third line\n"],
            bin_file: os.urandom(97901),
        },
    )

    revisions = repo.revision_history(remote=True)
    verify_signatures(revisions, 1)

    repo.branch_create("test-branch")

    # Ensure branch switch can switch to the branch point even though it was created on a different branch
    repo.branch_switch("main")
    repo.branch_switch(name="test-branch", revision=revisions[0].signature)

    repo.write_commit_push(
        None, {text_file: ["One line\n", "Another line\n", "Third line\nFourth line\n"]}
    )

    repo.remove_file(text_file)

    with repo.open_file(bin_file, "w+b") as file:
        file.write(os.urandom(100))

    repo.stage(scan=True, offline=True)

    # Ensure branches can be created even with staged state
    repo.branch_create("test-branch-staged")
    repo.unstage(offline=True)
    repo.branch_switch("test-branch")
    repo.branch_archive("test-branch-staged")
    repo.stage(scan=True, offline=True)

    repo.commit()
    repo.push()
    repo.repository_verify()

    urc_clone = repo.clone()
    repo.branch_switch("main")

    assert repo.compare_file(urc_clone, text_file), "Files in cloned repo should match"
    assert repo.compare_file(urc_clone, bin_file), "Files in cloned repo should match"

    repo.branch_switch("test-branch")
    urc_clone.branch_switch("test-branch")

    assert repo.compare_file(urc_clone, bin_file), "Files in cloned repo should match"

    assert not repo.file_exists(text_file), "Deleted file still exists"
    assert not urc_clone.file_exists(text_file), "Deleted file still exists"

    # Verify we cannot delete current branch
    with pytest.raises(DeleteCurrentError):
        repo.branch_archive("test-branch")
    # Verify we cannot delete default branch
    with pytest.raises(DeleteDefaultError):
        repo.branch_archive("main")

    # Verify protected branches can't be deleted
    repo.branch_switch("main")
    repo.repository_verify()
    repo.branch_protect("test-branch")
    with pytest.raises(DeleteProtectedError):
        repo.branch_archive("test-branch")

    # Unprotect and verify we can delete the branch
    repo.branch_unprotect("test-branch")
    repo.branch_archive("test-branch")

    # Verify branch is deleted
    assert not repo.has_branch("test-branch")

    # Ensure branch is deleted
    repo.branch_create("no-history")
    repo.push()
    branch_description = repo.branch_info()

    assert (
        branch_description.local_latest != ""
        and branch_description.local_latest == branch_description.remote_latest
    ), "Branch with no additional history not pushed correctly"

    repo.branch_switch("main")

    # Create another revision
    repo.write_commit_push(
        None,
        {text_file: ["One line\n", "Another line\n", "Third line\n", "Fourth line\n"]},
    )

    revisions = repo.revision_history()
    verify_signatures(revisions, 2)

    # Clone the repository at revision one
    urc_divergent = repo.clone(revision=revisions[0].signature)

    # Create a divegent second revision
    with urc_divergent.open_file(text_file, "w+") as file:
        file.writelines(
            ["One line\n", "Another line\n", "Third line\n", "Divergent line\n"]
        )

    urc_divergent.stage(scan=True)
    urc_divergent.commit("Test commit 3 (divergent)")

    with pytest.raises(BranchDivergedError):
        urc_divergent.push()

    urc_divergent.push(force=True)

    # Clone the repository again
    urc_verify = repo.clone()
    revisions = urc_verify.revision_history()
    verify_signatures(revisions, 2)

    assert urc_divergent.compare_file(urc_verify, bin_file), (
        "File contents should match in cloned repositories"
    )

    urc_divergent.sync(revisions[0].signature)

    with pytest.raises(BranchDivergedError):
        urc_divergent.push()

    urc_divergent.push(force=True)

    revisions = urc_verify.revision_history(remote=True)
    verify_signatures(revisions, 1)

    # Force sync the pushed revision
    urc_verify.sync(revisions[0].signature, force=True)

    # Recreate the branch
    repo.branch_switch("main")
    repo.sync(force=True)
    repo.branch_create("recreate-branch")

    repo.write_commit_push("New rev on recreate-branch", {bin_file: os.urandom(100)})

    assert "On branch recreate-branch" in repo.status(), (
        "Not on expected branch after create"
    )

    # Branch info on an active branch reports archived=false.
    active_info = parse_jsonl(repo.branch_info(json=True), "branchInfo")
    assert len(active_info) == 1, (
        f"Expected one branchInfo entry for active branch, got {active_info}"
    )
    assert not active_info[0]["archived"], (
        "Active branch should report archived=false in branch info"
    )
    recreate_branch_id = active_info[0]["id"]

    revisions = repo.revision_history()
    verify_signatures(revisions, 2)

    # Switch to main, create a revision, and delete the branch
    repo.branch_switch("main")

    repo.write_commit_push("New rev on main", {bin_file: os.urandom(100)})

    assert "On branch main" in repo.status(), "Not on expected branch after switch"

    revisions = repo.revision_history()
    verify_signatures(revisions, 2)
    expected_signatures = [rev.signature for rev in revisions]

    # Ensure a branch cannot be created when it exists and that we remain on main and can delete it
    with pytest.raises(BranchAlreadyExistsError):
        repo.branch_create("recreate-branch")

    assert "On branch main" in repo.status(), "Not on expected branch after switch"

    repo.branch_archive("recreate-branch", local=True)
    repo.branch_list()

    # Branch info queried by ID for a locally-archived branch reports archived=true.
    archived_info = parse_jsonl(
        repo.branch_info(recreate_branch_id, json=True, offline=True), "branchInfo"
    )
    assert len(archived_info) == 1, (
        f"Expected one branchInfo entry for archived branch, got {archived_info}"
    )
    assert archived_info[0]["archived"], (
        "Locally-archived branch should report archived=true in branch info"
    )

    # Ensure a branch cannot be created when it exists on remote, even when deleted locally,
    # and that we remain on main and can delete it fully
    with pytest.raises(BranchAlreadyExistsError):
        repo.branch_create("recreate-branch")

    assert "On branch main" in repo.status(), "Not on expected branch after switch"

    repo.branch_archive("recreate-branch")

    repo.branch_create("recreate-branch", offline=True)
    repo.branch_list()
    repo.branch_switch("main")

    with pytest.raises(BranchAlreadyExistsError):
        repo.branch_create("recreate-branch")

    assert "On branch main" in repo.status(), "Not on expected branch after switch"

    repo.branch_archive("recreate-branch")

    assert "On branch main" in repo.status(), (
        "Not on expected branch after switch and delete"
    )

    assert not repo.has_branch("recreate-branch"), "Deleted branch still in branch list"

    # Create branch and verify it was created at the current main LATEST revision
    repo.branch_create("recreate-branch")

    assert "On branch recreate-branch" in repo.status(), (
        "Not on expected branch after create"
    )

    revisions = repo.revision_history()
    verify_signatures(revisions, 2)
    current_signatures = [rev.signature for rev in revisions]
    assert current_signatures[1] == expected_signatures[1], (
        "New recreated branch not on expected revision"
    )

    # Switch back to main and verify branch cannot be created when it exists
    repo.branch_switch("main")
    with pytest.raises(BranchAlreadyExistsError):
        repo.branch_create("recreate-branch")

    # Ensure it can be created with a force flag
    repo.sync(revision="@1")
    repo.branch_create("recreate-branch", force=True)

    revisions = repo.revision_history()
    verify_signatures(revisions, 1)
    current_signatures = [rev.signature for rev in revisions]
    assert current_signatures[0] == expected_signatures[0], (
        "New recreated branch not on expected revision"
    )

    # Branch switch sync to latest or local
    repo.branch_switch("main")
    repo.sync()

    urc_verify.branch_switch("main")
    urc_verify.sync(force=True)
    urc_verify.sync()

    new_file = "new-file-in-latest.uasset"
    with urc_verify.open_file(new_file, "w+b") as file:
        file.write(os.urandom(128))

    urc_verify.stage(new_file)
    urc_verify.commit("Another rev on main")
    urc_verify.push()

    repo.branch_switch("recreate-branch")

    assert not repo.file_exists(new_file), "Branch switch did not sync files as expected"

    repo.branch_switch("main", local=True)

    assert not repo.file_exists(new_file), (
        "Branch switch with --local did not keep revision as expected"
    )

    repo.branch_switch("recreate-branch")

    assert not repo.file_exists(new_file), "Branch switch did not sync files as expected"

    repo.branch_switch("main")

    assert repo.file_exists(new_file), (
        "Branch switch with sync to latest remote LATEST did not sync files as expected"
    )

    # Branch switch with and without --force
    forced_main_content = os.urandom(1000)
    forced_file = "new-forced-file.uasset"
    repo.write_commit_push("New forced file", {forced_file: forced_main_content})

    repo.branch_create("force-branch")
    branch_forced_main_content = os.urandom(1000)
    repo.write_commit_push("New forced file", {forced_file: branch_forced_main_content})

    test_files = ["subdir/hierarchy/A.bin", "subdir/B.bin", "otherdir/C.bin"]

    for file in test_files:
        repo.make_dirs(os.path.dirname(file))

        with repo.open_file(file, "w+b") as open_file:
            open_file.write(os.urandom(1901))

    repo.branch_switch("main")

    for file in test_files:
        assert repo.file_exists(file), "Branch switch deleted filesystem file"
        assert os.path.exists(os.path.dirname(os.path.join(repo.path, file))), (
            "Branch switch deleted filesystem directory"
        )

    verify_forced_file = "verify-new-forced-file.uasset"
    with repo.open_file(verify_forced_file, "w+b") as file:
        file.write(forced_main_content)

    assert repo.compare_file(repo, forced_file, verify_forced_file), (
        "Files should contain same contents"
    )

    repo.remove_file(verify_forced_file)

    repo.branch_switch("force-branch")

    with repo.open_file(verify_forced_file, "w+b") as file:
        file.write(branch_forced_main_content)

    assert repo.compare_file(repo, forced_file, verify_forced_file), (
        "Files should contain same contents"
    )

    repo.remove_file(verify_forced_file)

    repo.branch_switch("main", force=True, reset=True)

    with repo.open_file(verify_forced_file, "w+b") as file:
        file.write(forced_main_content)

    assert repo.compare_file(repo, forced_file, verify_forced_file), (
        "Files should contain same contents"
    )

    repo.remove_file(verify_forced_file)

    for file in test_files:
        assert not repo.file_exists(file), (
            "Branch switch with --force --reset did not delete filesystem file"
        )
        assert not os.path.exists(os.path.dirname(os.path.join(repo.path, file))), (
            "Branch switch with --force --reset did not delete filesystem directory"
        )


@pytest.mark.smoke
def test_branch_switch_bare(new_lore_repo):
    repo: Lore = new_lore_repo()

    text_file = "text-file.txt"
    bin_file = "data.uasset"

    repo.write_commit_push(
        "Initial commit on main",
        {
            text_file: ["Line one\n", "Line two\n"],
            bin_file: os.urandom(1024),
        },
    )

    repo.branch_create("bare-target")
    repo.write_commit_push(
        "Commit on bare-target",
        {text_file: ["Line one\n", "Line two\n", "Line three\n"]},
    )

    repo.branch_switch("main")

    # Bare clone the repository (no working files)
    bare_clone = repo.clone(bare=True)

    # Verify bare clone has no working files
    working_files = [f for f in os.listdir(bare_clone.path) if not f.startswith(".urc") and not f.startswith(".lore")]
    assert working_files == [], (
        f"Bare clone should have no working files, found: {working_files}"
    )

    # Bare switch to the other branch and verify via JSON event
    output = bare_clone.branch_switch("bare-target", bare=True, json=True)
    events = parse_jsonl(output, "branchSwitchEnd")
    assert len(events) == 1, "Expected exactly one branchSwitchEnd event"
    assert events[0]["branch"]["name"] == "bare-target", (
        "branchSwitchEnd event should report bare-target branch"
    )

    # Verify branch list reports the correct current branch
    branch_list = bare_clone.branch_list()
    assert branch_list.current_branch == "bare-target", (
        f"Expected current branch bare-target, got: {branch_list.current_branch}"
    )

    # Verify no working files were created by the bare switch
    working_files = [f for f in os.listdir(bare_clone.path) if not f.startswith(".urc") and not f.startswith(".lore")]
    assert working_files == [], (
        f"Bare switch should not create working files, found: {working_files}"
    )

    # Bare switch back to main and verify via JSON event
    output = bare_clone.branch_switch("main", bare=True, json=True)
    events = parse_jsonl(output, "branchSwitchEnd")
    assert len(events) == 1, "Expected exactly one branchSwitchEnd event"
    assert events[0]["branch"]["name"] == "main", (
        "branchSwitchEnd event should report main branch"
    )

    branch_list = bare_clone.branch_list()
    assert branch_list.current_branch == "main", (
        f"Expected current branch main, got: {branch_list.current_branch}"
    )

    working_files = [f for f in os.listdir(bare_clone.path) if not f.startswith(".urc") and not f.startswith(".lore")]
    assert working_files == [], (
        f"Bare switch back should not create working files, found: {working_files}"
    )


@pytest.mark.smoke
def test_branch_list_fresh_clone_lists_default_branch(new_lore_repo):
    repo: Lore = new_lore_repo()

    repo.write_commit_push(
        "Initial commit",
        {"file.txt": ["content\n"]},
    )

    fresh_clone = repo.clone()

    output = fresh_clone.branch_list(json=True)
    branch_list = parse_branch_list_json(output)

    assert branch_list.has_local_branch("main"), (
        f"Default branch 'main' should appear in local branches on a fresh clone, "
        f"got local={branch_list.local_branches}"
    )
    assert branch_list.has_remote_branch("main"), (
        f"Default branch 'main' should appear in remote branches on a fresh clone, "
        f"got remote={branch_list.remote_branches}"
    )
    assert branch_list.current_branch == "main", (
        f"Current branch should be 'main' on a fresh clone, "
        f"got current={branch_list.current_branch}"
    )


@pytest.mark.smoke
def test_branch_list_archived(new_lore_repo):
    repo: Lore = new_lore_repo()

    repo.write_commit_push(
        "Initial commit",
        {"file.txt": ["content\n"]},
    )

    def local_branch_sets() -> tuple[set[str], set[str]]:
        """Active-local and archived-local branch names from branch list --archived."""
        output = repo.branch_list(archived=True, json=True)
        local = [
            e
            for e in parse_jsonl(output, "branchListEntry")
            if e["location"] == "local"
        ]
        active = {e["name"] for e in local if not e["archived"]}
        archived = {e["name"] for e in local if e["archived"]}
        return active, archived

    # Before any archive: the default branch is active, nothing is archived, and
    # the active and archived lists are disjoint.
    active, archived = local_branch_sets()
    assert "main" in active, "Active branch 'main' should appear in the local list"
    assert not archived, f"No branch should be archived before any archive; got {archived}"
    assert active.isdisjoint(archived), (
        f"A branch must appear in either the local or archived list, never both; "
        f"overlap={active & archived}"
    )

    # A newly created branch joins the active list and stays out of the archived list.
    repo.branch_create("to-archive")
    repo.branch_switch("main")
    active, archived = local_branch_sets()
    assert "to-archive" in active, "Newly created branch should be in the active list"
    assert "to-archive" not in archived, "Newly created branch should not be archived"
    assert active.isdisjoint(archived), (
        f"A branch must appear in either the local or archived list, never both; "
        f"overlap={active & archived}"
    )

    # Without --archived, no entry is ever flagged archived.
    entries = parse_jsonl(repo.branch_list(json=True), "branchListEntry")
    assert not any(e["archived"] for e in entries), (
        "No branch should be flagged archived without the --archived flag"
    )

    # Archiving the branch locally removes it from the active list and moves it
    # into the archived list; unrelated active branches are untouched and the
    # lists stay disjoint.
    repo.branch_archive("to-archive", local=True)
    active, archived = local_branch_sets()
    assert "to-archive" not in active, (
        "Archived branch should be removed from the active local list"
    )
    assert "to-archive" in archived, "Archived branch should appear in the archived list"
    assert "main" in active, "Unrelated active branch 'main' must stay active"
    assert "main" not in archived, "Active branch 'main' must never appear as archived"
    assert active.isdisjoint(archived), (
        f"A branch must appear in either the local or archived list, never both; "
        f"overlap={active & archived}"
    )

    # The archived entry is reported at the local location.
    archived_entry = next(
        e
        for e in parse_jsonl(repo.branch_list(archived=True, json=True), "branchListEntry")
        if e["location"] == "local" and e["archived"] and e["name"] == "to-archive"
    )
    assert archived_entry["location"] == "local", (
        "Archived branch location should be local"
    )

    # A second archive adds to the archived list; lists remain disjoint throughout.
    repo.branch_create("to-archive-2")
    repo.branch_switch("main")
    repo.branch_archive("to-archive-2", local=True)
    active, archived = local_branch_sets()
    assert {"to-archive", "to-archive-2"} <= archived, (
        f"Both archived branches should appear in the archived list; got {archived}"
    )
    assert "main" in active, "Active branch 'main' should remain in the local list"
    assert active.isdisjoint(archived), (
        f"A branch must appear in either the local or archived list, never both; "
        f"overlap={active & archived}"
    )


@pytest.mark.smoke
def test_branch_list_remote_only(new_lore_repo):
    repo: Lore = new_lore_repo()

    repo.write_commit_push(
        "Initial commit",
        {"file.txt": ["content\n"]},
    )

    # The default listing covers both the local and the remote.
    default = parse_jsonl(repo.branch_list(json=True), "branchListEntry")
    assert any(e["location"] == "local" for e in default), (
        "Default branch list should include local entries"
    )
    assert any(e["location"] == "remote" and e["name"] == "main" for e in default), (
        "Default branch list should include remote entries"
    )

    # --remote lists the remote only: every entry is remote, with no local section.
    remote = parse_jsonl(repo.branch_list(remote=True, json=True), "branchListEntry")
    assert remote, "--remote should list the remote branches"
    assert all(e["location"] == "remote" for e in remote), (
        "--remote must emit only remote entries, never a local section"
    )
    assert any(e["name"] == "main" for e in remote), (
        "main should be listed from the remote"
    )


def run_divergent_resolve_test(whose, new_lore_repo):
    original_repo: Lore = new_lore_repo()
    text_file = "file.txt"
    original_repo.write_commit_push(
        "add file", {text_file: ["One line\n", "Another line\n", "Third line\n"]}
    )

    cloned_repo = original_repo.clone()

    # Modify a file in the original repo and push it after cloning
    original_repo.write_commit_push(
        "original modify", {text_file: ["One line\n", "Changed line\n", "Third line\n"]}
    )

    # Modify the same file in the cloned repo
    with pytest.raises(BranchDivergedError):
        cloned_repo.write_commit_push(
            "cloned modify",
            {text_file: ["One line\n", "Very changed line\n", "Third line\n"]},
        )

    cloned_repo.sync()
    if whose == "mine":
        cloned_repo.branch_merge_resolve_mine(text_file)
    else:
        cloned_repo.branch_merge_resolve_theirs(text_file)
    cloned_repo.commit("resolve")
    cloned_repo.push()


@pytest.mark.smoke
def test_divergent_merge_resolve_mine(new_lore_repo):
    run_divergent_resolve_test("mine", new_lore_repo)


@pytest.mark.smoke
def test_divergent_merge_resolve_theirs(new_lore_repo):
    run_divergent_resolve_test("theirs", new_lore_repo)


def create_repo_state(repo):
    repo.make_dirs("folder/subfolder")
    repo.make_dirs("folder/subfolder2")
    repo.write_commit_push(
        "Initial commit",
        {
            "folder/subfolder/file.txt": ["A line in folder/subfolder/file.txt\n"],
            "folder/subfolder2/file.txt": ["A line in folder/subfolder2/file.txt\n"],
            "folder/subfolder2/file2.txt": ["A line in folder/subfolder2/file2/txt\n"],
        },
    )


@pytest.mark.smoke
def test_divergent_modified_deleted_with_siblings(new_lore_repo):
    repo: Lore = new_lore_repo("test_divergent_modified_deleted_with_siblings")
    create_repo_state(repo)

    clone = repo.clone()
    clone.rmtree("folder/subfolder2")
    clone.stage(".", scan=True)
    clone.commit("Remove subfolder2")
    clone.push()

    with pytest.raises(BranchDivergedError):
        repo.write_commit_push(
            "Edit file",
            {
                "folder/subfolder2/file.txt": ["An updated line\n"],
                "folder/subfolder2/file2.txt": ["Another updated line\n"],
            },
        )

    repo.sync()

    repo.branch_merge_resolve_mine("folder/subfolder2/file.txt")

    assert not repo.file_exists("folder/subfolder2/file.txt"), (
        "Deleted file has returned"
    )
    assert repo.path_exists("folder/subfolder2"), (
        "subfolder2 should not be deleted yet as other unresolved files exist"
    )

    repo.branch_merge_resolve_mine("folder/subfolder2/file2.txt")
    assert not repo.file_exists("folder/subfolder2/file2.txt"), (
        "Deleted file has returned"
    )
    assert not repo.path_exists("folder/subfolder2"), (
        "subfolder2 should be deleted as it is missing on both states"
    )

    repo.commit("Resolved conflict")
    repo.push()

    clone.sync()
    assert not clone.file_exists("folder/subfolder2/file.txt"), (
        "Deleted file has returned"
    )
    assert not clone.file_exists("folder/subfolder2/file1.txt"), (
        "Deleted file has returned"
    )
    assert not clone.path_exists("folder/subfolder2"), "Deleted folder has returned"


@pytest.mark.smoke
def test_divergent_deleted_modified_with_siblings(new_lore_repo):
    repo: Lore = new_lore_repo("test_divergent_deleted_modified_with_siblings")
    create_repo_state(repo)

    clone = repo.clone()

    repo.write_commit_push(
        "Edit file",
        {
            "folder/subfolder2/file.txt": ["An updated line\n"],
            "folder/subfolder2/file2.txt": ["Another updated line\n"],
        },
    )

    clone.rmtree("folder/subfolder2")
    clone.stage(".", scan=True)
    clone.commit("Remove subfolder2")
    with pytest.raises(BranchDivergedError):
        clone.push()

    clone.sync()
    clone.repository_dump()

    clone.branch_merge_resolve_mine("folder/subfolder2/file.txt")
    assert clone.file_exists("folder/subfolder2/file.txt"), (
        "Deleted file should have returned after resolve"
    )

    clone.branch_merge_resolve_mine("folder/subfolder2/file2.txt")
    assert clone.file_exists("folder/subfolder2/file2.txt"), (
        "Deleted file should have returned after resolve"
    )

    clone.commit("Resolved conflict")
    clone.push()

    repo.sync()
    assert repo.file_exists("folder/subfolder2/file.txt"), (
        "Edited file should still remain"
    )
    assert repo.file_exists("folder/subfolder2/file2.txt"), (
        "Edited file should still remain"
    )


# Switching into a branch that deletes a directory must block the switch when
# a tracked file inside that directory has uncommitted local edits. The verifier
# returns a path-less SyncError::LocalModifications; per-path detail is delivered
# via error-level log events, which we verify via the JSON event stream.
@pytest.mark.smoke
def test_switch_to_branch_deleting_dir_with_local_modified_file(new_lore_repo):
    repo: Lore = new_lore_repo()

    repo.write_commit_push(
        "Initial commit",
        {
            "doomed_dir/file.txt": ["original\n"],
            "doomed_dir/file2.txt": ["original 2\n"],
            "keep_dir/keep.txt": ["keep\n"],
        },
    )

    repo.branch_create("delete-dir")
    repo.rmtree("doomed_dir")
    repo.stage(".", scan=True)
    repo.commit("Delete doomed_dir")
    repo.push()

    repo.branch_switch("main")
    assert repo.file_exists("doomed_dir/file.txt"), "Setup: dir restored on main"

    with repo.open_file("doomed_dir/file.txt", "w+") as f:
        f.write("locally modified, not committed\n")

    with pytest.raises(LocalChanges) as exc_info:
        repo.branch_switch("delete-dir", json=True)

    log_events = parse_jsonl(str(exc_info.value), "log")
    error_messages = [e["message"] for e in log_events if e.get("level") == "error"]
    assert any("doomed_dir/file.txt" in m for m in error_messages), (
        f"Expected error log to name modified file path; got: {error_messages}"
    )
    assert any("doomed_dir" in m for m in error_messages), (
        f"Expected error log to name affected directory; got: {error_messages}"
    )


@pytest.mark.smoke
def test_switch_to_branch_deleting_dir_with_local_added_file(new_lore_repo):
    """Locally-added (untracked) file in a doomed directory should not block the
    switch — the verifier is meant to keep locally-added files. Confirms the
    Add branch of the verifier in lore-revision/src/fs/realize.rs."""
    repo: Lore = new_lore_repo()

    repo.write_commit_push(
        "Initial commit",
        {"doomed_dir/file.txt": ["original\n"]},
    )

    repo.branch_create("delete-dir")
    repo.rmtree("doomed_dir")
    repo.stage(".", scan=True)
    repo.commit("Delete doomed_dir")
    repo.push()

    repo.branch_switch("main")

    with repo.open_file("doomed_dir/added.txt", "w+") as f:
        f.write("locally added, not staged\n")

    repo.branch_switch("delete-dir")

    assert repo.file_exists("doomed_dir/added.txt"), (
        "Locally added file inside doomed dir should be preserved across switch"
    )
    assert not repo.file_exists("doomed_dir/file.txt"), (
        "Tracked file in doomed dir should be removed by switch"
    )


@pytest.mark.smoke
def test_switch_to_branch_deleting_dir_with_missing_tracked_file(new_lore_repo):
    """Tracked files inside a doomed directory are missing on disk before the
    switch (manually deleted by the user without staging the deletion). The
    directory is going away on the destination branch, so the switch should
    succeed without tripping a sync verify error."""
    repo: Lore = new_lore_repo()

    repo.write_commit_push(
        "Initial commit",
        {
            "doomed_dir/file.txt": ["original\n"],
            "doomed_dir/file2.txt": ["original 2\n"],
        },
    )

    repo.branch_create("delete-dir")
    repo.rmtree("doomed_dir")
    repo.stage(".", scan=True)
    repo.commit("Delete doomed_dir")
    repo.push()

    repo.branch_switch("main")
    repo.remove_file("doomed_dir/file.txt")

    repo.branch_switch("delete-dir")

    assert not repo.path_exists("doomed_dir"), (
        "doomed_dir should be removed by switch to delete-dir"
    )


# =============================================================================
# Branch existence smoke tests
#
# These tests verify the branch create/query/delete consistency model.
# A branch is considered to exist when it has both a name->ID mapping AND valid
# metadata for that ID, with the metadata name matching the mapped name.
#
# Create decision matrix:
#
# | # | METADATA? | name->ID? | Extra check                       | Outcome              | Test                                            |
# |---|-----------|-----------|-----------------------------------|----------------------|-------------------------------------------------|
# | 1 | No        | No        | -                                 | Create               | test_branch, test_delete_and_create_*            |
# | 2 | No        | Yes=self  | -                                 | Create               | (stale mapping, not testable via smoke)          |
# | 3 | No        | Yes!=self | mapped ID has no metadata         | Create               | (stale mapping, not testable via smoke)          |
# | 4 | No        | Yes!=self | mapped ID has metadata            | AlreadyExist (name)  | test_name_conflict_from_two_clones               |
# | 5 | Yes       | No        | metadata.name == given name       | Restore (same name)  | test_push_restores_deleted_branch*               |
# | 6 | Yes       | No        | name differs, old name mapped     | AlreadyExist (id)    | test_duplicate_id_different_name                 |
# | 7 | Yes       | No        | name differs, old name gone       | Restore+rename       | test_delete_and_create_different_name_same_id    |
# | 8 | Yes       | Yes       | -                                 | AlreadyExist (id)    | test_branch, test_local_delete_and_switch*       |
#
# Query matrix:
#
# | Query     | Condition                                   | Outcome       | Test                                         |
# |-----------|---------------------------------------------|---------------|----------------------------------------------|
# | By name   | name->ID not found                          | NOT_FOUND     | test_query_deleted_branch_by_name_not_found   |
# | By name   | name->ID found, metadata exists, name match | Exists        | (implicit in every branch_list call)          |
# | By ID     | metadata not found                          | NOT_FOUND     | (implicit in initial push queries)            |
# | By ID     | metadata found, name->ID matches            | Exists        | (implicit in push flow)                       |
# | By ID     | metadata found, name->ID missing/mismatch   | Exists+deleted| test_query_deleted_branch_by_id               |
#
# Additional behavioral tests:
# - test_local_delete_and_switch_restores_from_remote: local delete, create fails (remote), switch restores
# - test_push_restores_deleted_branch: push from clone restores deleted branch, name reuse blocked
# - test_push_restores_deleted_branch_no_new_commits: push with no new commits still restores
# =============================================================================


@pytest.mark.smoke
def test_push_restores_deleted_branch(new_lore_repo):
    """Pushing to a branch that was deleted on the server should recreate it."""
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    # Create a branch with a commit and push it
    repo.branch_create("feature")
    repo.write_commit_push("Feature commit", {text_file: ["Line one\nFeature line\n"]})

    # Clone so we have a second client with the branch
    clone = repo.clone(branch="feature")

    # Verify both clients see the branch remotely
    branch_list = repo.branch_list()
    assert branch_list.has_remote_branch("feature"), (
        "feature branch should exist remotely before delete"
    )

    # Delete the branch from the first client (deletes name→id mapping on server)
    repo.branch_switch("main")
    repo.branch_archive("feature")

    branch_list = repo.branch_list()
    assert not branch_list.has_remote_branch("feature"), (
        "feature branch should not exist remotely after delete"
    )

    # Make a new commit on the clone (which still has the branch locally)
    with clone.open_file(text_file, "w+") as f:
        f.writelines(["Line one\nFeature line\nAnother line\n"])
    clone.stage(text_file)
    clone.commit("Another feature commit")

    # Push from the clone — this should trigger branch recreation via create
    clone.push()

    # Verify the branch is back in the remote branch list
    branch_list = clone.branch_list()
    assert branch_list.has_remote_branch("feature"), (
        "feature branch should be restored remotely after push from clone"
    )

    # Verify from a fresh clone that the branch is visible
    verify_clone = repo.clone()
    branch_list = verify_clone.branch_list()
    assert branch_list.has_remote_branch("feature"), (
        "feature branch should be visible from a fresh clone"
    )

    # Now test that pushing to a deleted branch fails when the name has been reused
    # by a different branch (different ID).

    # Delete the restored feature branch again
    repo.sync()
    repo.branch_archive("feature")

    branch_list = repo.branch_list()
    assert not branch_list.has_remote_branch("feature"), (
        "feature branch should not exist remotely after second delete"
    )

    # Create a new branch with the same name "feature" from repo (gets a new branch ID)
    repo.branch_create("feature")
    repo.write_commit_push("New feature commit", {text_file: ["Reused name\n"]})

    branch_list = repo.branch_list()
    assert branch_list.has_remote_branch("feature"), (
        "reused feature branch should exist remotely"
    )

    # The clone still has the old branch ID for "feature". Pushing should fail because
    # the name→id mapping now points to a different branch.
    with clone.open_file(text_file, "w+") as f:
        f.writelines(["Line one\nFeature line\nYet another line\n"])
    clone.stage(text_file)
    clone.commit("Commit on stale branch")

    with pytest.raises(Exception):
        clone.push()


@pytest.mark.smoke
def test_push_restores_deleted_branch_no_new_commits(new_lore_repo):
    """Pushing the same revision to a deleted branch should recreate it without new commits."""
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    # Create a branch with a commit and push it
    repo.branch_create("restore-branch")
    repo.write_commit_push(
        "Branch commit", {text_file: ["Line one\nBranch line\n"]}
    )

    # Clone with the branch
    clone = repo.clone(branch="restore-branch")

    # Delete the branch from the original repo
    repo.branch_switch("main")
    repo.branch_archive("restore-branch")

    branch_list = repo.branch_list()
    assert not branch_list.has_remote_branch("restore-branch"), (
        "restore-branch should not exist remotely after delete"
    )

    # Push from the clone with no new commits — should recreate the branch
    clone.push()

    # Verify the branch is back
    branch_list = clone.branch_list()
    assert branch_list.has_remote_branch("restore-branch"), (
        "restore-branch should be restored remotely after push with no new commits"
    )

    # Verify from a fresh clone
    verify_clone = repo.clone()
    branch_list = verify_clone.branch_list()
    assert branch_list.has_remote_branch("restore-branch"), (
        "restore-branch should be visible from a fresh clone"
    )


def get_status_branch(repo: Lore) -> str:
    """Get the current branch name from status JSON output."""
    import json

    output = repo.status(json=True, offline=True)
    for line in output.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        branch_name = event.get("data", {}).get("branchName", "")
        if branch_name:
            return branch_name
    return ""


@pytest.mark.smoke
def test_branch_point_preserves_branch(new_lore_repo):
    """Syncing or switching to a branch point revision should preserve the
    current branch, not switch to the parent branch."""

    repo: Lore = new_lore_repo()

    # Create initial commit on main
    with repo.open_file("file.txt", "w+") as f:
        f.write("initial\n")
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    revisions = repo.revision_history(remote=True)
    branch_point = revisions[0].signature

    # Create a branch (which sets current branch to test-branch)
    repo.branch_create("test-branch")
    assert get_status_branch(repo) == "test-branch"

    # Commit on test-branch
    with repo.open_file("file.txt", "w+") as f:
        f.write("on test-branch\n")
    repo.stage(scan=True)
    repo.commit("commit on test-branch", offline=True)

    # Sync back to the branch point — should stay on test-branch
    repo.sync(branch_point)
    assert get_status_branch(repo) == "test-branch", (
        "Sync to branch point should preserve the current branch"
    )

    # Branch switch to test-branch at the branch point — should be on test-branch
    repo.branch_switch("main")
    assert get_status_branch(repo) == "main"
    repo.branch_switch(name="test-branch", revision=branch_point)
    assert get_status_branch(repo) == "test-branch", (
        "Branch switch to branch point should be on the target branch"
    )


@pytest.mark.smoke
def test_local_delete_and_switch_restores_from_remote(new_lore_repo):
    """Deleting a branch locally then switching to it should restore from remote."""
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    # Create and push a feature branch
    repo.branch_create("restore-switch")
    repo.write_commit_push("Feature commit", {text_file: ["Line one\nFeature\n"]})
    repo.branch_switch("main")

    # Delete local only
    repo.branch_archive("restore-switch", local=True)
    branch_list = repo.branch_list()
    assert not branch_list.has_local_branch("restore-switch"), (
        "restore-switch should not exist locally after local delete"
    )
    assert branch_list.has_remote_branch("restore-switch"), (
        "restore-switch should still exist on remote"
    )

    # Creating with same name should fail (remote still has it)
    with pytest.raises(BranchAlreadyExistsError):
        repo.branch_create("restore-switch")

    # Switching should restore from remote
    repo.branch_switch("restore-switch")
    assert "On branch restore-switch" in repo.status(), (
        "Should be on restore-switch after switch"
    )
    branch_list = repo.branch_list()
    assert branch_list.has_local_branch("restore-switch"), (
        "restore-switch should exist locally after switch from remote"
    )


@pytest.mark.smoke
def test_delete_and_create_different_name_same_client(new_lore_repo):
    """After deleting a branch, creating a new branch that reuses the same slot should work."""
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    # Create, push, switch away, delete
    repo.branch_create("old-name")
    repo.write_commit_push("Branch commit", {text_file: ["Line one\nBranch\n"]})
    repo.branch_switch("main")
    repo.branch_archive("old-name")

    branch_list = repo.branch_list()
    assert not branch_list.has_remote_branch("old-name"), (
        "old-name should not exist after delete"
    )

    # Create a new branch — should succeed since old-name is fully deleted
    repo.branch_create("new-name")
    repo.write_commit_push("New branch commit", {text_file: ["Line one\nNew branch\n"]})

    branch_list = repo.branch_list()
    assert branch_list.has_remote_branch("new-name"), (
        "new-name should exist after push"
    )


@pytest.mark.smoke
def test_name_conflict_from_two_clones(new_lore_repo):
    """Two clones creating branches with the same name should result in one AlreadyExist."""
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    clone_a = repo.clone()
    clone_b = repo.clone()

    # Clone A creates and pushes branch "shared-name"
    clone_a.branch_create("shared-name")
    clone_a.write_commit_push(
        "Clone A commit", {text_file: ["Line one\nClone A\n"]}
    )

    # Clone B tries to create a branch with the same name — should fail
    with pytest.raises(BranchAlreadyExistsError):
        clone_b.branch_create("shared-name")


@pytest.mark.smoke
def test_query_deleted_branch_by_name_not_found(new_lore_repo):
    """Querying a deleted branch by name should return not-found."""
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    repo.branch_create("query-del")
    repo.write_commit_push("Branch commit", {text_file: ["Line one\nBranch\n"]})
    repo.branch_switch("main")
    repo.branch_archive("query-del")

    # Fresh clone should not see the branch by name
    clone = repo.clone()
    branch_list = clone.branch_list()
    assert not branch_list.has_remote_branch("query-del"), (
        "Deleted branch should not appear in remote branch list"
    )


@pytest.mark.smoke
def test_duplicate_id_different_name(new_lore_repo):
    """Creating two branches with the same explicit ID but different names should fail."""
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    branch_id = generate_branch_id()

    # Create first branch with explicit ID
    repo.branch_create("first-name", id=branch_id)
    repo.write_commit_push("First commit", {text_file: ["Line one\nFirst\n"]})
    repo.branch_switch("main")

    # Creating another branch with same ID but different name should fail
    with pytest.raises(BranchAlreadyExistsError):
        repo.branch_create("second-name", id=branch_id)


@pytest.mark.smoke
def test_delete_and_create_different_name_same_id(new_lore_repo):
    """After deleting a branch, creating with same ID but different name is a restore+rename.

    The branch history (latest pointer) should be preserved — it's the same branch
    under a new name. No force should be needed.
    """
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    branch_id = generate_branch_id()

    # Create, push, switch away, delete
    repo.branch_create("old-name", id=branch_id)
    repo.write_commit_push("Branch commit", {text_file: ["Line one\nBranch\n"]})
    repo.branch_switch("main")
    repo.branch_archive("old-name")

    branch_list = repo.branch_list()
    assert not branch_list.has_remote_branch("old-name"), (
        "old-name should not exist after delete"
    )

    # Create with same ID but different name — this is a restore+rename.
    # The branch keeps its history and latest pointer.
    repo.branch_create("new-name", id=branch_id)
    repo.branch_switch("new-name")

    # Push should succeed (latest already on server, just restores the name mapping)
    repo.push()

    branch_list = repo.branch_list()
    assert branch_list.has_remote_branch("new-name"), (
        "new-name should exist remotely after push"
    )
    assert not branch_list.has_remote_branch("old-name"), (
        "old-name should still not exist"
    )


@pytest.mark.smoke
def test_query_deleted_branch_by_id(new_lore_repo):
    """A deleted branch should appear in the deleted branch list but not the remote list."""
    repo: Lore = new_lore_repo()

    text_file = "file.txt"
    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})

    repo.branch_create("del-query-id")
    repo.write_commit_push("Branch commit", {text_file: ["Line one\nBranch\n"]})
    repo.branch_switch("main")
    repo.branch_archive("del-query-id")

    # Branch should not appear in remote list
    branch_list = repo.branch_list()
    assert not branch_list.has_remote_branch("del-query-id"), (
        "Deleted branch should not appear in remote branch list"
    )

    # Branch should appear in archived list (metadata still exists locally)
    branch_list = repo.branch_list(archived=True)
    assert branch_list.has_archived_branch("del-query-id"), (
        "Archived branch should appear in archived branch list"
    )


@pytest.mark.smoke
def test_branch_reset_relists_locally_archived_branch(new_lore_repo):
    """A deleted branch should re-appear in the local branch list after a reset."""
    repo: Lore = new_lore_repo()

    # Base revision on main.
    with repo.open_file("base.txt", "w+") as f:
        f.write("base v1\n")
    repo.stage(scan=True, offline=True)
    repo.commit("v1", offline=True)

    # Feature branch with a known id, carrying its own revision.
    feature_id = generate_branch_id()
    repo.branch_create("feature", id=feature_id, offline=True)
    repo.branch_switch("feature", offline=True)
    with repo.open_file("feature.txt", "w+") as f:
        f.write("feature v1\n")
    repo.stage(scan=True, offline=True)
    repo.commit("feature v1", offline=True)
    feature_rev = repo.revision_history(1, offline=True)[0].signature

    # Archive it locally while it is not the current branch — this zeroes
    # the local name->id mapping and drops it from the local branch list.
    repo.branch_switch("main", offline=True)
    repo.branch_archive("feature", local=True)
    assert "feature" not in repo.branch_list(offline=True).local_branches, (
        "local archive should remove the branch from the local branch list"
    )

    # Reset the archived branch by id (its name no longer resolves locally).
    repo.branch_reset(feature_rev, branch=feature_id, offline=True)

    # The reset must have restored the branch's local name->id mapping, so
    # it shows up in the local branch list again.
    assert "feature" in repo.branch_list(offline=True).local_branches, (
        "branch reset should restore the branch's local name->id mapping"
    )


ZERO_HASH = "0" * 64


@pytest.mark.smoke
def test_branch_info_remote_local_flags(new_lore_repo):
    """`branch info` with --local / --remote global flags."""
    repo: Lore = new_lore_repo()
    text_file = "file.txt"

    repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})
    repo.branch_create("flag-test")
    repo.write_commit_push("Branch commit", {text_file: ["Line one\nBranch\n"]})
    branch_id = parse_jsonl(repo.branch_info(json=True), "branchInfo")[0]["id"]
    repo.branch_switch("main")

    # Branch present locally and remotely, no flag.
    info = parse_jsonl(repo.branch_info("flag-test", json=True), "branchInfo")[0]
    assert info["archived"] is False
    assert info["latest"] != ZERO_HASH, "local latest must be populated by default"
    assert info["latestRemote"] != ZERO_HASH, (
        "remote latest must be populated by default"
    )

    # --local on the same branch.
    info = parse_jsonl(
        repo.branch_info("flag-test", json=True, local=True), "branchInfo"
    )[0]
    assert info["archived"] is False
    assert info["latest"] != ZERO_HASH, "--local must report the local latest"
    assert info["latestRemote"] == ZERO_HASH, (
        "--local must leave latestRemote zero"
    )

    # --remote on the same branch.
    info = parse_jsonl(
        repo.branch_info("flag-test", json=True, remote=True), "branchInfo"
    )[0]
    assert info["archived"] is False
    assert info["latestRemote"] != ZERO_HASH, (
        "--remote must report the remote latest"
    )

    repo.branch_archive("flag-test")

    # --remote against a branch archived on the remote.
    info = parse_jsonl(
        repo.branch_info(branch_id, json=True, remote=True), "branchInfo"
    )[0]
    assert info["archived"] is True, (
        "--remote must report archived=true for a branch archived on the remote"
    )

    # --local against a branch archived locally.
    info = parse_jsonl(
        repo.branch_info(branch_id, json=True, local=True, offline=True),
        "branchInfo",
    )[0]
    assert info["archived"] is True, (
        "--local must report archived=true for a branch archived locally"
    )

    # --local against a branch that exists only on the remote.
    repo.branch_create("remote-only")
    repo.write_commit_push("Remote-only commit", {text_file: ["Line one\nRO\n"]})
    repo.branch_switch("main")

    clone = repo.clone()
    with pytest.raises(NotFound):
        clone.branch_info("remote-only", local=True)

    # Two instances: branch deleted on remote, second instance still has it
    # locally.
    repo.branch_create("two-instance")
    repo.write_commit_push("Two-instance commit", {text_file: ["Line one\nTI\n"]})
    repo.branch_switch("main")

    other = repo.clone()
    other.branch_switch("two-instance")
    two_instance_id = parse_jsonl(
        other.branch_info("two-instance", json=True), "branchInfo"
    )[0]["id"]
    other.branch_switch("main")

    repo.branch_archive("two-instance")

    info = parse_jsonl(
        other.branch_info(two_instance_id, json=True, local=True, offline=True),
        "branchInfo",
    )[0]
    assert info["archived"] is False, (
        "--local in a second instance that still has the branch must report "
        "archived=false"
    )

    info = parse_jsonl(
        other.branch_info(two_instance_id, json=True, remote=True), "branchInfo"
    )[0]
    assert info["archived"] is True, (
        "--remote in a second instance must report archived=true once the "
        "branch is gone from the remote"
    )
