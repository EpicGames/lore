# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from status_util import Operation, verify_operations_in_status

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_urcignore_committed_file(new_lore_repo):
    """
    Verify that .loreignore only affects outbound (local) operations, not
    inbound (committed-state) operations.

    Scenario:
    1. Commit a file (data.bin) normally.
    2. Add a .loreignore rule that matches that file.
    3. Verify outbound commands (status, stage) still handle the file: the
       ignore rules govern only what the tree does not track (ADR-00027).
    4. Verify inbound commands (sync, branch switch, revision diff between
       two revisions, restore) still handle the ignored-but-committed file.
    """
    repo: Lore = new_lore_repo()

    # -- Setup: commit a file before any ignore rules exist --
    committed_file = "data.bin"
    other_file = "other.txt"

    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(1024))
    with repo.open_file(other_file, "w+") as f:
        f.write("hello\n")

    repo.stage(scan=True)
    repo.commit("Initial commit with data.bin and other.txt")
    repo.push()

    # Modify the file so we have a second revision
    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(2048))
    with repo.open_file(other_file, "w+") as f:
        f.write("updated\n")

    repo.stage(scan=True)
    repo.commit("Second commit - modify data.bin and other.txt")
    repo.push()

    # Record revisions
    history = repo.revision_history()
    rev2 = history[0].signature  # latest (second commit)
    rev1 = history[1].signature  # first commit

    # -- Add ignore file that matches the committed file --
    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("data.bin\n")

    # Stage and commit the ignore file itself
    repo.stage(scan=True)
    repo.commit("Add ignore file excluding data.bin")
    repo.push()

    # -- Outbound: status reports a change to the tracked file --
    # Modify the ignored-but-committed file on disk
    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(512))

    output = repo.repository_status(unstaged=True)
    assert "data.bin" in output, (
        "status should report a change to ignored-but-committed file data.bin"
    )

    # -- Outbound: stage takes the change without --force --
    repo.stage(scan=True)
    output = repo.repository_status()
    assert "data.bin" in output, (
        "stage should have staged the change to ignored-but-committed file data.bin"
    )

    # Unstage and reset the local modification so it doesn't interfere with sync
    repo.unstage(".")
    repo.file_reset()

    # -- Inbound: revision diff between two committed revisions should include data.bin --
    diff_output = repo.revision_diff(rev1, target=rev2)
    assert "data.bin" in diff_output, (
        "revision diff between two committed revisions should include "
        "ignored-but-committed file data.bin"
    )

    # -- Inbound: sync to an older revision should materialize data.bin --
    repo.sync(rev1)
    assert repo.file_exists(committed_file), (
        "sync to older revision should materialize ignored-but-committed file data.bin"
    )

    # -- Inbound: restore should include data.bin --
    # We're at rev1, restore fast-forwards to head including data.bin changes
    repo.revision_restore("Restore to latest")

    assert repo.file_exists(committed_file), (
        "restore should materialize ignored-but-committed file data.bin"
    )


@pytest.mark.smoke
def test_urcignore_merge_committed_file(new_lore_repo):
    """
    Verify that branch merge still processes ignored-but-committed files.

    Scenario:
    1. Commit data.bin on main.
    2. Create a feature branch, modify data.bin, push.
    3. On main, add .loreignore for data.bin, commit.
    4. Merge feature branch into main - data.bin changes should come through.
    """
    repo: Lore = new_lore_repo()

    committed_file = "data.bin"

    # Initial commit with data.bin
    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(1024))

    repo.stage(scan=True)
    repo.commit("Initial commit with data.bin")
    repo.push()

    # Create feature branch and modify data.bin
    repo.branch_create("feature")
    repo.branch_switch("feature")

    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(2048))

    repo.stage(scan=True)
    repo.commit("Feature: modify data.bin")
    repo.push()

    # Switch back to main, add ignore file
    repo.branch_switch("main")

    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("data.bin\n")

    repo.stage(scan=True)
    repo.commit("Add ignore file excluding data.bin")
    repo.push()

    # Merge feature into main
    repo.branch_merge_start("feature")

    # data.bin should still exist after merge (committed state came through)
    assert repo.file_exists(committed_file), (
        "merge should bring in changes to ignored-but-committed file data.bin"
    )


@pytest.mark.smoke
def test_urcignore_merge_new_file_from_incoming_branch(new_lore_repo):
    """
    Verify that merge brings in a file in an ignored subdirectory committed
    only on the incoming branch.

    Scenario:
    1. On main, commit an initial file (no artifacts/, no .loreignore).
    2. Create a feature branch from that point.
    3. On main, add .loreignore for artifacts/, commit, push.
    4. Switch to feature branch, commit artifacts/data.bin (no .loreignore there), push.
    5. Switch back to main, merge feature branch.
    6. artifacts/data.bin should appear on main despite the ignore rule.
    """
    repo: Lore = new_lore_repo()

    committed_file = "artifacts/data.bin"

    # -- Main: initial commit without artifacts/ or ignore file --
    with repo.open_file("readme.txt", "w+") as f:
        f.write("initial\n")

    repo.stage(scan=True)
    repo.commit("Initial commit without artifacts/")
    repo.push()

    # -- Create feature branch before ignore file exists, then switch back to main --
    repo.branch_create("feature")
    repo.branch_switch("main")

    # -- Main: add ignore file that ignores the entire artifacts/ directory --
    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("artifacts\n")

    repo.stage(scan=True)
    repo.commit("Add ignore file excluding artifacts/")
    repo.push()

    # -- Feature branch: commit artifacts/data.bin (no ignore file on this branch) --
    repo.branch_switch("feature")
    repo.make_dirs("artifacts/")

    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(1024))

    repo.stage(scan=True)
    repo.commit("Feature: add artifacts/data.bin")
    repo.push()

    # -- Back to main: merge feature --
    repo.branch_switch("main")
    repo.branch_merge_start("feature", debug=True)

    # artifacts/data.bin should exist after merge even though main ignores artifacts/
    assert repo.file_exists(committed_file), (
        "merge should bring in artifacts/data.bin from incoming branch even when "
        "current branch has a .loreignore rule for the artifacts/ directory"
    )


@pytest.mark.own_vfs("resets local changes, which a virtual clone refuses: it holds whole revisions")
@pytest.mark.smoke
def test_urcignore_branch_switch_preserves_ignored_file(new_lore_repo):
    """
    Verify that branch switch with reset does not delete files excluded by
    .loreignore that were never committed.

    The branch switch sync uses FilterMode::View which only checks the
    .lore/view filter, not .loreignore. Files matched by .loreignore that
    exist on the filesystem but are not in the revision tree get incorrectly
    identified as "extra" files and deleted during the reset diff reversal.

    Scenario:
    1. Create a repo with .loreignore excluding *.code-workspace.
    2. Create a .code-workspace file on disk (never committed).
    3. Commit other tracked files, push.
    4. Create a feature branch, add a new file, commit, push.
    5. Merge feature branch back into main, push.
    6. Switch to main with --reset.
    7. Assert the .code-workspace file still exists on disk.
    """
    repo: Lore = new_lore_repo()

    ignored_file = "project.code-workspace"

    # -- Setup: create ignore file before any commits --
    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("*.code-workspace\n")

    # Create the ignored file on disk
    with repo.open_file(ignored_file, "w+") as f:
        f.write('{"folders": [{"path": "."}]}\n')

    # Create a tracked file
    with repo.open_file("readme.txt", "w+") as f:
        f.write("initial\n")

    repo.stage(scan=True)
    repo.commit("Initial commit with ignore file")
    repo.push()

    # Verify the ignored file is not tracked
    output = repo.repository_status(unstaged=True)
    assert "code-workspace" not in output, "ignored file should not appear in status"

    # -- Create feature branch and add a tracked file --
    repo.branch_create("feature")
    repo.branch_switch("feature")

    with repo.open_file("feature.txt", "w+") as f:
        f.write("feature work\n")

    repo.stage(scan=True)
    repo.commit("Feature: add feature.txt")
    repo.push()

    # -- Merge feature into main --
    repo.branch_switch("main")
    repo.branch_merge_start("feature")

    # -- Switch to main with reset --
    repo.branch_switch("main", reset=True, force=True)

    # The ignored file must survive the branch switch
    assert repo.file_exists(ignored_file), (
        "branch switch with --reset should not delete files excluded by "
        ".loreignore (FilterMode::View does not check .loreignore)"
    )


@pytest.mark.own_vfs("resets local changes, which a virtual clone refuses: it holds whole revisions")
@pytest.mark.smoke
@pytest.mark.parametrize("command", ["sync", "branch switch"])
def test_urcignore_reset_restores_committed_file(new_lore_repo, command):
    """
    Verify that `sync --reset` and `branch switch --reset` carry the target
    revision's change to a committed file an ignore rule matches over a local
    edit, and its deletes of others, and leave untracked files alone, whether
    an ignore rule matches them or not.
    """
    repo: Lore = new_lore_repo()
    repo.write_files(
        {
            "Game/cache.tmp": "committed\n",
            "Game/gone.tmp": "committed\n",
            "Saved/Old/drop.ini": "committed\n",
        }
    )
    repo.stage(scan=True)
    repo.commit("Commit files before the ignore rules")
    repo.write_commit_push("Add ignore rules", {repo.ignore_file(): "*.tmp\nSaved/\n"})

    clone = repo.clone()
    if command == "branch switch":
        clone.branch_create("feature")
        clone.branch_switch("feature")

    repo.write_files({"Game/cache.tmp": "incoming\n"})
    repo.remove_file("Game/gone.tmp")
    repo.rmtree("Saved")
    repo.stage(["Game/cache.tmp", "Game/gone.tmp", "Saved/Old/drop.ini"], force=True)
    repo.commit("Change and delete ignored committed files")
    repo.push()

    untracked = {
        "Game/build.tmp": "untracked\n",
        "Game/notes.txt": "untracked\n",
        "Saved/Old/run.log": "untracked\n",
    }
    clone.write_files({"Game/cache.tmp": "local\n", **untracked})
    if command == "sync":
        clone.sync(reset=True)
    else:
        clone.branch_switch("main", reset=True)

    for path, content in {"Game/cache.tmp": "incoming\n", **untracked}.items():
        assert clone.file_exists(path), f"{command}: {path} is missing"
        with clone.open_file(path) as f:
            assert f.read() == content, f"{command}: {path} does not hold {content!r}"
    for path in ["Game/gone.tmp", "Saved/Old/drop.ini"]:
        assert not clone.file_exists(path), f"{command}: {path} survived the reset"


@pytest.mark.smoke
def test_urcignore_directory_rule_leaves_files_of_that_name(new_lore_repo):
    """A directory-only rule such as `Intermediate/` leaves out directories of
    that name and nothing else: a file `src/Intermediate` is reported, diffed,
    staged, committed and locked like any other file.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("Intermediate/\n")
    repo.make_dirs("src")
    with repo.open_file("src/Intermediate", "w+") as f:
        f.write("file\n")

    output = repo.status(scan=True)
    verify_operations_in_status(output, [], untracked_files=["src/Intermediate"])
    assert "+++ src/Intermediate" in repo.diff("src/Intermediate"), (
        "diff should report the file src/Intermediate"
    )

    repo.stage(scan=True)
    verify_operations_in_status(repo.status(), [(Operation.ADD, "src/Intermediate")])

    repo.commit("Add src/Intermediate")
    repo.push()

    assert repo.lock_acquire("src/Intermediate").acquired == ["src/Intermediate"]
    assert [lock.file for lock in repo.lock_status("src/Intermediate")] == [
        "src/Intermediate"
    ]
    assert [lock.file for lock in repo.lock_query("main", path="src/Intermediate")] == [
        "src/Intermediate"
    ]
    assert repo.lock_release("src/Intermediate").released == ["src/Intermediate"]


@pytest.mark.smoke
def test_urcignore_legacy_fallback(new_lore_repo):
    """A legacy `.urcignore` is honored as a fallback when no `.loreignore`
    exists, and `.loreignore` takes precedence when both are present.

    The repository uses the current `.lore/` format, whose primary ignore file
    is `.loreignore`. With only a legacy `.urcignore` on disk its rules must
    still apply to outbound operations. Once a `.loreignore` is added it becomes
    authoritative and the `.urcignore` is no longer consulted.
    """
    repo: Lore = new_lore_repo()

    # Only a legacy .urcignore present: its rule must be honored as a fallback.
    with repo.open_file(".urcignore", "w+") as f:
        f.write("legacy.tmp\n")

    with repo.open_file("legacy.tmp", "w+") as f:
        f.write("from urcignore\n")
    with repo.open_file("kept.txt", "w+") as f:
        f.write("tracked\n")

    output = repo.repository_status(unstaged=True)
    assert "legacy.tmp" not in output, (
        "legacy .urcignore should be honored as a fallback when no .loreignore exists"
    )
    assert "kept.txt" in output, "non-ignored file should still be reported"

    # Adding a .loreignore makes it authoritative: it no longer ignores
    # legacy.tmp (now reported), while its own rule excludes primary.tmp.
    with repo.open_file(".loreignore", "w+") as f:
        f.write("primary.tmp\n")

    with repo.open_file("primary.tmp", "w+") as f:
        f.write("from loreignore\n")

    output = repo.repository_status(unstaged=True)
    assert "primary.tmp" not in output, (
        ".loreignore should take precedence and exclude its own rules"
    )
    assert "legacy.tmp" in output, (
        ".urcignore must no longer be consulted once .loreignore exists"
    )


def _without_warnings(output: str) -> str:
    """`output` without the warnings Lore logs, which name paths of their own."""
    return "\n".join(
        line for line in output.splitlines() if not line.startswith("[Warn]")
    )


@pytest.mark.smoke
def test_instance_ignore_rules(new_lore_repo):
    """
    The `ignore` file in the instance's Lore directory excludes untracked
    entries from the working-directory operations as `.loreignore` does,
    without being committed: an untracked file under a rule drops out of
    `status --scan` and a scanning stage, an edit to a tracked file under it
    still flows through, and `--force stage` stages the untracked file. A `!`
    line is skipped with a warning naming it.
    """
    repo: Lore = new_lore_repo()
    repo.write_files(
        {
            "Binaries/Win64/tool.exe": "tracked\n",
            "notes.txt": "tracked\n",
        }
    )
    repo.stage(scan=True)
    repo.commit("Commit files before the instance ignore rules")

    with open(os.path.join(repo.dot_path(), "ignore"), "w") as f:
        f.write("# build outputs\n\nBinaries/\n!Binaries/Win64/\n")
    repo.write_files(
        {
            "Binaries/Mac/editor": "untracked\n",
            "Binaries/Win64/tool.exe": "edited\n",
            "notes.txt": "edited\n",
        }
    )

    output = repo.status(scan=True)
    # Windows paths print with either separator, so compare them with forward slashes
    ignore_path = os.path.join(repo.dot_path(), "ignore").replace("\\", "/")
    warning = f"Skipping line 4 of {ignore_path}, !Binaries/Win64/"
    assert warning in output.replace("\\", "/"), (
        f"no warning names the skipped re-inclusion:\n{output}"
    )
    entries = _without_warnings(output)
    assert "editor" not in entries, (
        f"status --scan reports an untracked file the instance ignore file excludes:\n{output}"
    )
    assert "notes.txt" in entries, "status --scan should report notes.txt"
    assert "tool.exe" in entries, (
        f"status --scan should report an edit to a tracked file under a rule:\n{output}"
    )

    diff = _without_warnings(repo.diff())
    assert "notes.txt" in diff, "diff should report notes.txt"
    assert "tool.exe" in diff, f"diff should report an edit to a tracked file:\n{diff}"

    repo.stage(scan=True)
    output = _without_warnings(repo.status())
    verify_operations_in_status(
        output,
        [
            (Operation.MODIFY, "Binaries/Win64/tool.exe"),
            (Operation.MODIFY, "notes.txt"),
        ],
    )
    assert "editor" not in output, (
        f"a scanning stage staged an untracked file the instance ignore file excludes:\n{output}"
    )
    assert ".lore" not in output, "the instance ignore file must never be staged"

    repo.stage("Binaries/Mac/editor", force=True)
    verify_operations_in_status(
        repo.status(),
        [
            (Operation.MODIFY, "Binaries/Win64/tool.exe"),
            (Operation.ADD, "Binaries/Mac/editor"),
            (Operation.MODIFY, "notes.txt"),
        ],
    )
