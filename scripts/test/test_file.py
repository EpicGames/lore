# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import random
import re

import pytest

from error_types import FileNotFound, LoreException, NotSupportedError
from lore import Lore
from lore_ffi import DISCONNECTED, NOT_CONNECTED
from lore_parsers import (
    parse_complete_json,
    parse_file_audit_json,
    parse_jsonl,
    parse_revision_list_json,
)
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)

logger = logging.getLogger(__name__)


def _revision_metadata(repo: Lore, revision: str) -> dict:
    """The metadata keys `revision info` reports for one revision, which is where
    a caller of `lore file audit` reads a range's attribution from."""
    revisions = parse_revision_list_json(repo.revision_info(revision, json=True))
    assert len(revisions) == 1, f"revision info reported {revisions} for {revision}"
    return revisions[0]["metadata"]


@pytest.mark.smoke
def test_file(new_lore_repo):
    repo: Lore = new_lore_repo("File")

    # Verify the repository is created
    assert os.path.isdir(repo.dot_path()), "Repository was not created"

    # Generate some files
    text_file = "text-file.txt"
    another_file = "another-file.txt"

    repo.write_commit_push(
        "Test commit",
        {
            text_file: ["One line\n", "Another line in text file\n", "Third line\n"],
            another_file: [
                "One line\n",
                "Another line in another file\n",
                "Third line\n",
            ],
        },
        offline=True,
    )

    # Modify some files, stage and commit
    repo.write_commit_push(
        "Test commit 2",
        {
            text_file: "Modified on second revision\n",
            another_file: "Reduced to one line\n",
        },
        offline=True,
    )

    # Modify some files, stage and commit
    repo.write_commit_push(
        "Test commit 3",
        {
            text_file: "Modified on third revision\n",
            another_file: "Final modification\n",
        },
        offline=True,
    )

    # Modify a file without committing and use file reset to get old state
    with repo.open_file(another_file, "w+") as output_file:
        output_file.writelines(["Something we want to reset\n"])

    repo.file_reset(another_file, offline=True)
    repo.file_reset(text_file, revision="@1", offline=True)

    with repo.open_file(text_file, "r") as input_file:
        lines = input_file.readlines()
        assert lines == ["One line\n", "Another line in text file\n", "Third line\n"], (
            f"Text file not reset to revision 1, got {lines}"
        )

    with repo.open_file(another_file, "r") as input_file:
        lines = input_file.readlines()
        assert lines == ["Final modification\n"], (
            f"Text file not reset to last revision, got {lines}"
        )

    repo.file_reset(offline=True)

    # Set up some merges and modifications to test reset to last merged

    repo.branch_create("test-branch", offline=True)
    repo.branch_switch("main", offline=True)

    repo.write_commit_push(
        "Modified on main",
        {
            text_file: [
                "Modified line\n",
                "Another line in text file\n",
                "Third line was also modified\n",
            ],
        },
        offline=True,
    )

    repo.branch_switch("test-branch", offline=True)

    repo.write_commit_push(
        "Modified another file on branch",
        {
            another_file: ["A change on the branch\n"],
        },
        offline=True,
    )

    repo.write_commit_push(
        "Modified text file on branch",
        {
            text_file: [
                "Modified line on branch\n",
                "Another line in text file also modified on branch\n",
                "Third line was also modified\n",
            ]
        },
        offline=True,
    )

    repo.file_reset(text_file, last_merged_from="main", offline=True)

    with repo.open_file(text_file, "r") as input_file:
        lines = input_file.readlines()
        assert lines == ["Modified on third revision\n"], (
            f"Text file not reset to last merged revision, got {lines}"
        )

    repo.branch_merge("main", offline=True)

    repo.write_commit_push(
        "Modified again on branch",
        {
            text_file: [
                "Modified line on branch again\n",
                "Another line in text file also modified on branch\n",
                "Third line was also modified\n",
            ]
        },
        offline=True,
    )

    third_file = "third-file.txt"

    repo.write_commit_push(
        "Modified again on branch",
        {third_file: ["An added file on the branch that should be deleted\n"]},
        offline=True,
    )

    repo.file_reset(
        [text_file, third_file], last_merged_from="main", offline=True, debug=True
    )

    with repo.open_file(text_file, "r") as input_file:
        lines = input_file.readlines()
        assert lines == [
            "Modified line\n",
            "Another line in text file\n",
            "Third line was also modified\n",
        ], f"Text file not reset to last merged revision, got {lines}"

    assert not repo.file_exists(third_file), (
        "Third file not deleted by merge to last merged revision"
    )

    # Check that we can reset to last merged when branch has no revisions
    repo.branch_switch("main", offline=True, force=True)
    repo.file_reset(offline=True)

    repo.branch_create("test-dummy-reset", offline=True)
    repo.file_reset(
        [text_file, third_file], last_merged_from="main", offline=True, debug=True
    )

    move_file = "move-file.txt"

    repo.write_commit_push(
        "Test commit for move",
        {move_file: ["One line\n", "Another line in text file\n", "Third line\n"]},
        offline=True,
    )

    move_file_moved = "move-file-moved.txt"

    repo.move(move_file, move_file_moved)

    result = repo.stage_move(move_file, move_file_moved, offline=True)

    assert "Staging 1 files (0 modified, 0 added, 0 deleted, 1 moved)" in result, (
        "Failed to stage move file"
    )

    move_dir = "dir"
    repo.make_dirs(move_dir)

    move_file_in_dir = os.path.join(move_dir, "move-file.txt")

    repo.write_commit_push(
        "Test commit for move in directory",
        {
            move_file_in_dir: [
                "One line\n",
                "Another line in text file\n",
                "Third line\n",
            ]
        },
        offline=True,
    )

    move_dir2 = "dir2"

    repo.move(move_dir, move_dir2)

    result = repo.stage_move(move_dir, move_dir2, offline=True)

    assert "Staging 1 directories (0 added, 0 deleted, 1 moved)" in result, (
        "Failed to move stage directory"
    )


@pytest.mark.smoke
def test_file_reset_view(new_lore_repo, scratch_dir):
    repo: Lore = new_lore_repo("FileResetView")

    # Mix of file-level and whole-subdir view filtering. Every directory that
    # survives the view filter retains at least one kept file alongside the
    # dropped one(s).
    keep_top = "top-keep.txt"
    drop_top = "top-drop.txt"
    sub_dir = "sub"
    sub_keep = os.path.join(sub_dir, "keep.txt")
    sub_drop = os.path.join(sub_dir, "drop.txt")
    nested_dir = os.path.join(sub_dir, "nested")
    nested_keep = os.path.join(nested_dir, "keep.txt")
    nested_drop = os.path.join(nested_dir, "drop.txt")
    drop_subdir = "drop_subdir"
    drop_subdir_files = [os.path.join(drop_subdir, f"{i}.txt") for i in range(3)]

    files = {
        keep_top: ["keep at top\n"],
        drop_top: ["drop at top\n"],
        sub_keep: ["keep in sub\n"],
        sub_drop: ["drop in sub\n"],
        nested_keep: ["keep in nested\n"],
        nested_drop: ["drop in nested\n"],
    }
    for path in drop_subdir_files:
        files[path] = [f"{path}\n"]
    repo.write_commit_push("Initial commit", files)

    view_dir = scratch_dir("file-reset-view", create=True)
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("/" + drop_top + "\n")
        view_file.write("/" + sub_drop.replace(os.sep, "/") + "\n")
        view_file.write("/" + nested_drop.replace(os.sep, "/") + "\n")
        view_file.write("/" + drop_subdir + "\n")

    clone = repo.clone(view=view_path)

    def snapshot_tree(root: str) -> set[str]:
        entries: set[str] = set()
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in (".lore", ".urc")]
            rel_dir = os.path.relpath(dirpath, root)
            for d in dirnames:
                entries.add(os.path.join(rel_dir, d) if rel_dir != "." else d)
            for f in filenames:
                entries.add(os.path.join(rel_dir, f) if rel_dir != "." else f)
        return entries

    before = snapshot_tree(clone.path)

    clone.file_reset(".")

    after = snapshot_tree(clone.path)

    extra = after - before
    assert not extra, (
        f"file reset materialized paths that the clone's view filter did not: "
        f"{sorted(extra)}"
    )
    missing = before - after
    assert not missing, (
        f"file reset removed paths that the clone had materialized: {sorted(missing)}"
    )


def _working_tree(root: str) -> dict[str, bytes | None]:
    """Every path under `root` but the control directory, with a file's content and
    `None` for a directory."""
    tree: dict[str, bytes | None] = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in (".lore", ".urc")]
        for d in dirnames:
            tree[os.path.relpath(os.path.join(dirpath, d), root)] = None
        for f in filenames:
            path = os.path.join(dirpath, f)
            with open(path, "rb") as file:
                tree[os.path.relpath(path, root)] = file.read()
    return tree


@pytest.mark.smoke
def test_file_reset_dry_run_leaves_the_working_tree(new_lore_repo):
    """`file reset --purge --dry-run` restores no modified or deleted file and purges
    no untracked file or directory, at the root or in a subdirectory, where the same
    reset without `--dry-run` does."""
    repo: Lore = new_lore_repo("FileResetDryRun")

    modified = "modified.txt"
    deleted = "deleted.txt"
    nested_modified = os.path.join("directory", "inner", "modified.txt")
    nested_deleted = os.path.join("directory", "inner", "deleted.txt")
    repo.write_commit_push(
        "Initial commit",
        {
            modified: b"committed\n",
            deleted: b"committed\n",
            nested_modified: b"committed\n",
            nested_deleted: b"committed\n",
        },
        offline=True,
    )

    repo.write_files(
        {
            modified: b"local\n",
            nested_modified: b"local\n",
            "untracked.txt": b"local\n",
            os.path.join("directory", "untracked", "file.txt"): b"local\n",
        }
    )
    os.remove(os.path.join(repo.path, deleted))
    os.remove(os.path.join(repo.path, nested_deleted))

    before = _working_tree(repo.path)
    repo.file_reset(purge=True, dry_run=True, offline=True)
    assert _working_tree(repo.path) == before, (
        "file reset --dry-run changed the working tree"
    )

    repo.file_reset(purge=True, offline=True)
    after = _working_tree(repo.path)
    for path in (modified, deleted, nested_modified, nested_deleted):
        assert after.get(path) == b"committed\n", f"file reset did not restore {path}"
    for path in ("untracked.txt", os.path.join("directory", "untracked")):
        assert path not in after, f"file reset --purge did not remove {path}"


@pytest.mark.smoke
def test_file_reset_last_merged_side(new_lore_repo):
    """`file reset --last-merged-from` restores the merge revision, and `--mine` and
    `--theirs` restore the sides the merge was made from."""
    repo: Lore = new_lore_repo("FileResetLastMergedSide")

    conflict_file = "conflict.txt"

    repo.write_commit_push("Base commit", {conflict_file: ["base\n"]}, offline=True)

    repo.branch_create("feature", offline=True)
    repo.write_commit_push(
        "Modified on feature", {conflict_file: ["feature\n"]}, offline=True
    )

    repo.branch_switch("main", offline=True)
    repo.write_commit_push(
        "Modified on main", {conflict_file: ["main\n"]}, offline=True
    )

    repo.branch_switch("feature", offline=True)

    merge_output = repo.branch_merge("main", offline=True)
    assert "conflicted" in merge_output, (
        f"Merging main into feature should conflict, got:\n{merge_output}"
    )

    # Resolve by hand with content matching neither side, so the bytes on disk say
    # which of the three versions a reset restored.
    with repo.open_file(conflict_file, "w+") as output_file:
        output_file.writelines(["resolved\n"])
    repo.branch_merge_resolve(conflict_file, offline=True)
    repo.commit("Merge main into feature", offline=True)

    def reset_and_read(mine: bool = False, theirs: bool = False) -> list[str]:
        repo.file_reset(
            conflict_file,
            last_merged_from="main",
            mine=mine,
            theirs=theirs,
            offline=True,
        )
        with repo.open_file(conflict_file, "r") as input_file:
            return input_file.readlines()

    assert reset_and_read() == ["resolved\n"], (
        "Reset to last merged should restore the merge revision"
    )
    assert reset_and_read(mine=True) == ["feature\n"], (
        "Reset to last merged with --mine should restore the feature side of the merge"
    )
    assert reset_and_read(theirs=True) == ["main\n"], (
        "Reset to last merged with --theirs should restore the main side of the merge"
    )

    # Either side names a side of a merge, so neither means anything on its own.
    output = repo.file_reset(conflict_file, theirs=True, offline=True, check=False)
    assert "last-merged-from" in output, (
        f"--theirs without --last-merged-from should be rejected, got:\n{output}"
    )


@pytest.mark.smoke
def test_file_write_address_inside_and_outside_repository(new_lore_repo, scratch_dir):
    """`file write --address` puts the content an address names at a destination the working
    tree holds, through the tree, and at one outside the repository, through the host
    filesystem."""
    repo: Lore = new_lore_repo()

    payload = random.Random(2).randbytes(4096)
    with repo.open_file("source.bin", "wb") as output_file:
        output_file.write(payload)
    repo.stage("source.bin")
    repo.commit("Commit the source")

    info = repo.file_info("source.bin")[0]
    address = f"{info.hash}-{info.context}"

    inside = os.path.join(repo.path, "restored.bin")
    outside = str(scratch_dir("file-write-outside", create=True) / "restored.bin")
    for output in (inside, outside):
        repo.file_write(address=address, output=output)
        with open(output, "rb") as restored:
            assert restored.read() == payload, (
                f"{output} must hold the content {address} names"
            )


@pytest.mark.smoke
def test_file_hash_inside_and_outside_repository(new_lore_repo, scratch_dir):
    """`file hash` reads a file the working tree holds through the tree and one outside the
    repository from the host filesystem, answering alike for the same content."""
    repo: Lore = new_lore_repo()

    payload = random.Random(3).randbytes(4096)
    with repo.open_file("hashed.bin", "wb") as output_file:
        output_file.write(payload)
    outside = scratch_dir("file-hash-outside", create=True) / "hashed.bin"
    outside.write_bytes(payload)

    events = parse_jsonl(
        repo.file_hash(["hashed.bin", str(outside)], json=True), "fileHash"
    )
    assert len(events) == 2, f"Expected one hash event per path, got {events}"
    assert [event["size"] for event in events] == [len(payload)] * 2, (
        f"Both events must report the size of the payload.\nGot: {events}"
    )
    assert events[0]["hash"] == events[1]["hash"], (
        f"The same content must hash alike inside and outside the repository.\n"
        f"Got: {events}"
    )


@pytest.mark.smoke
def test_file_audit_names_the_revision_that_last_changed_each_line(new_lore_repo):
    """`lore file audit` attributes every line of a file to the revision that
    last changed it, and collapses consecutive lines sharing a revision into one
    range. A line untouched since the first revision stays with that revision
    however many revisions followed it."""
    repo: Lore = new_lore_repo("FileAudit")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    repo.write_commit_push(
        "Second commit",
        {audited: ["alpha\n", "BRAVO\n", "charlie\n"]},
        offline=True,
    )
    second = repo.revision_info().signature

    repo.write_commit_push(
        "Third commit",
        {audited: ["alpha\n", "BRAVO\n", "charlie\n", "delta\n"]},
        offline=True,
    )
    third = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"], entry["revision"])
        for entry in ranges
    ] == [
        (1, 1, "attributed", first),
        (2, 2, "attributed", second),
        (3, 3, "attributed", first),
        (4, 4, "attributed", third),
    ]

    # At the first revision the whole file is one range, since one revision
    # wrote all of it.
    at_first = parse_file_audit_json(
        repo.file_audit(audited, revision=first, json=True)
    )
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in at_first
    ] == [(1, 3, first)]


@pytest.mark.smoke
def test_file_audit_json_sends_the_most_recently_changed_range_first(new_lore_repo):
    """`--json` emits each range as soon as the walk resolves it, not in line
    order, so a range below can arrive before a range above it."""
    repo: Lore = new_lore_repo("FileAuditOrder")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    repo.write_commit_push(
        "Second commit",
        {audited: ["alpha\n", "bravo\n", "CHARLIE\n"]},
        offline=True,
    )
    second = repo.revision_info().signature

    emitted = parse_jsonl(repo.file_audit(audited, json=True), "fileAudit")
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in emitted
    ] == [(3, 3, second), (1, 2, first)]


@pytest.mark.smoke
def test_file_audit_reports_lines_past_the_budget_as_budget_exhausted(new_lore_repo):
    """A walk stopped by its revision budget reports the lines it never reached
    as budget-exhausted rather than attributing them to a revision it did not
    verify. The file needs a chain longer than the budget, so it takes three
    revisions: a budget of one reaches only the last of them."""
    repo: Lore = new_lore_repo("FileAuditBudget")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    repo.write_commit_push(
        "Second commit",
        {audited: ["alpha\n", "BRAVO\n", "charlie\n"]},
        offline=True,
    )
    repo.write_commit_push(
        "Third commit",
        {audited: ["alpha\n", "BRAVO\n", "charlie\n", "delta\n"]},
        offline=True,
    )
    third = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, max_revisions=1, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"]) for entry in ranges
    ] == [
        (1, 3, "budget-exhausted"),
        (4, 4, "attributed"),
    ]
    assert ranges[1]["revision"] == third

    # A budget of zero removes the bound, so the same chain resolves whole.
    unbounded = parse_file_audit_json(
        repo.file_audit(audited, max_revisions=0, json=True)
    )
    assert [entry["outcome"] for entry in unbounded] == ["attributed"] * len(unbounded)
    assert [(entry["lineFirst"], entry["lineLast"]) for entry in unbounded] == [
        (1, 1),
        (2, 2),
        (3, 3),
        (4, 4),
    ]


@pytest.mark.smoke
def test_file_audit_marks_lines_edited_on_disk_as_uncommitted(new_lore_repo):
    """A line differing on disk from the revision the audit read belongs to no
    revision, so it is reported as uncommitted with no revision and no author,
    and the lines around it still attribute normally. The numbers are the file on
    disk's, so an editor can align them against what it is showing."""
    repo: Lore = new_lore_repo("FileAuditUncommitted")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    # Edit line 2 and add a fourth line, staging and committing neither.
    repo.write_files({audited: ["alpha\n", "BRAVO\n", "charlie\n", "delta\n"]})

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"], entry["revision"])
        for entry in ranges
    ] == [
        (1, 1, "attributed", first),
        (2, 2, "uncommitted", "0" * 64),
        (3, 3, "attributed", first),
        (4, 4, "uncommitted", "0" * 64),
    ]

    # The summary says so in place of the revision, the author and the date.
    rows = [
        line.split() for line in repo.file_audit(audited).splitlines() if line.strip()
    ]
    assert [row[0] for row in rows] == ["1", "2", "3", "4"]
    assert rows[1] == ["2", "uncommitted"]
    assert rows[3] == ["4", "uncommitted"]

    # The file on disk answers for the revision the instance is synced to and
    # for no other, so an audit of a named revision reads history alone.
    at_first = parse_file_audit_json(
        repo.file_audit(audited, revision=first, json=True)
    )
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"]) for entry in at_first
    ] == [(1, 3, "attributed")]

    # A branch's latest revision is a named point in history too, even where it
    # resolves to the revision the instance is synced to.
    at_branch = parse_file_audit_json(
        repo.file_audit(audited, revision="main@LATEST", json=True)
    )
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"]) for entry in at_branch
    ] == [(1, 3, "attributed")]


@pytest.mark.smoke
def test_file_audit_reports_a_binary_version_in_history_as_binary_in_history(
    new_lore_repo,
):
    """A version the walk cannot compare ends the walk, not the audit: the lines
    it never reached carry the reason and the lines it already resolved stand.
    The file is text now and held binary content two revisions back, which no
    line-by-line comparison can read."""
    repo: Lore = new_lore_repo("FileAuditBinaryHistory")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: bytes([0x00, 0x01, 0x02, 0xFF, 0xFE, 0x00]) + os.urandom(64)},
        offline=True,
    )
    repo.write_commit_push(
        "Second commit",
        {audited: ["alpha\n", "bravo\n"]},
        offline=True,
    )
    repo.write_commit_push(
        "Third commit",
        {audited: ["alpha\n", "BRAVO\n"]},
        offline=True,
    )
    third = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"]) for entry in ranges
    ] == [
        (1, 1, "binary-in-history"),
        (2, 2, "attributed"),
    ]
    assert ranges[1]["revision"] == third

    # The summary prints the reason next to the range it applies to.
    rows = [
        line.split() for line in repo.file_audit(audited).splitlines() if line.strip()
    ]
    assert rows[0] == ["1", "binary-in-history"]


@pytest.mark.smoke
def test_file_audit_reports_an_obliterated_version_as_obliterated(new_lore_repo):
    """Obliterating a payload leaves the address it was stored at behind and the
    content unreadable, so the walk stops where the removed version stood. The
    lines it had already resolved are answers in their own right and still come
    back, and the audit reports rather than fails."""
    repo: Lore = new_lore_repo("FileAuditObliterated")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    repo.write_commit_push(
        "Second commit",
        {audited: ["alpha\n", "BRAVO\n", "charlie\n"]},
        offline=True,
    )
    repo.write_commit_push(
        "Third commit",
        {audited: ["alpha\n", "BRAVO\n", "charlie\n", "delta\n"]},
        offline=True,
    )
    third = repo.revision_info().signature

    # The content of the first revision is held by no later one, so obliterating
    # its address removes a version the walk needs and nothing the audit reads
    # at the revision it starts from.
    obliterated = repo.file_info(audited, revision=first)[0]
    output = repo.file_obliterate(
        address=f"{obliterated.hash}-{obliterated.context}", offline=True
    )
    assert "removed 1 payloads" in output, (
        f"the first revision's payload was not obliterated: {output}"
    )

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"]) for entry in ranges
    ] == [
        (1, 3, "obliterated"),
        (4, 4, "attributed"),
    ]
    assert ranges[1]["revision"] == third


@pytest.mark.smoke
def test_file_audit_default_output_is_a_range_summary(new_lore_repo):
    """The default output is one row per range carrying the line span, the
    revision, the author and the date. Asserted field by field rather than
    column by column, so the layout can be adjusted without rewriting this."""
    repo: Lore = new_lore_repo("FileAuditSummary")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    repo.write_commit_push(
        "Second commit",
        {audited: ["alpha\n", "bravo\n", "CHARLIE\n"]},
        offline=True,
    )
    second = repo.revision_info().signature

    rows = [
        line.split() for line in repo.file_audit(audited).splitlines() if line.strip()
    ]

    # Lines 1 and 2 are untouched since the first revision, so they collapse
    # into one range; line 3 stands alone. The library sends line 3 first, and
    # the summary still reads from the top.
    assert [row[0] for row in rows] == ["1-2", "3"]
    assert [row[1] for row in rows] == [first, second]
    for row in rows:
        assert len(row) >= 4, f"row is missing the author or the date: {row}"
        assert re.fullmatch(r"\d{4}-\d{2}-\d{2}", row[-1]), (
            f"row does not end in a date: {row}"
        )


@pytest.mark.smoke
def test_file_audit_refuses_a_path_the_revision_does_not_hold(new_lore_repo):
    """An absent path is a not-found path, and no walk runs for it, so no range
    is reported. A path is absent whether it was never added, was deleted before
    the revision read, or had not yet been added at the revision read."""
    repo: Lore = new_lore_repo("FileAuditAbsent")

    kept = "kept.txt"
    removed = "removed.txt"
    added_later = "added-later.txt"

    repo.write_commit_push(
        "First commit",
        {kept: ["alpha\n"], removed: ["gone\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    repo.remove_file(removed)
    repo.write_files({added_later: ["new\n"]})
    repo.stage(scan=True, offline=True)
    repo.commit("Second commit", offline=True)

    # Never added at any revision.
    with pytest.raises(FileNotFound):
        repo.file_audit("never-existed.txt")

    # Deleted before the revision read, but present at an earlier one.
    with pytest.raises(FileNotFound):
        repo.file_audit(removed)
    assert (
        len(parse_file_audit_json(repo.file_audit(removed, revision=first, json=True)))
        == 1
    )

    # Added after the revision read.
    with pytest.raises(FileNotFound):
        repo.file_audit(added_later, revision=first)

    # No walk runs for an absent path, so it reports no range at all.
    assert parse_file_audit_json(repo.file_audit(removed, json=True, check=False)) == []


@pytest.mark.smoke
def test_file_audit_refuses_a_binary_path(new_lore_repo):
    """Attribution compares versions line by line, which binary content has no
    answer for, so the query reports an error rather than nonsense ranges. The
    file on disk is the one being attributed where the audit accounts for it, so
    binary content there is refused on the same grounds as binary content in the
    revision."""
    repo: Lore = new_lore_repo("FileAuditBinary")

    binary = "payload.bin"
    text = "audited.txt"
    repo.write_commit_push(
        "First commit",
        {
            binary: bytes([0x00, 0x01, 0x02, 0xFF, 0xFE, 0x00]) + os.urandom(64),
            text: ["alpha\n", "bravo\n"],
        },
        offline=True,
    )
    first = repo.revision_info().signature

    with pytest.raises(NotSupportedError):
        repo.file_audit(binary)

    assert parse_file_audit_json(repo.file_audit(binary, json=True, check=False)) == []

    # Text at the revision read, binary on disk and neither staged nor committed.
    repo.write_files(
        {text: bytes([0x00, 0x01, 0x02, 0xFF, 0xFE, 0x00]) + os.urandom(64)}
    )

    with pytest.raises(NotSupportedError):
        repo.file_audit(text)

    # The revision itself still answers, the file on disk not being read for it.
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"])
        for entry in parse_file_audit_json(
            repo.file_audit(text, revision=first, json=True)
        )
    ] == [(1, 2, "attributed")]


@pytest.mark.smoke
def test_file_audit_survives_whitespace_churn(new_lore_repo):
    """Attribution asks who wrote a line, and trailing-space or line-ending churn
    does not answer that, so a line that gained trailing spaces stays with the
    revision that wrote it. Reindentation is a change and counts, unless
    `--ignore-space-change` asks otherwise."""
    repo: Lore = new_lore_repo("FileAuditWhitespace")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "  charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    # Line 1 is rewritten, so the revision carries a change no whitespace rule
    # can ignore; line 2 gains trailing spaces and line 3 is reindented.
    repo.write_commit_push(
        "Second commit",
        {audited: ["ALPHA\n", "bravo   \n", "      charlie\n"]},
        offline=True,
    )
    second = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in ranges
    ] == [
        (1, 1, second),
        (2, 2, first),
        (3, 3, second),
    ]

    # Turning the default off counts the trailing space, so the second revision
    # takes the line it only added spaces to.
    literal = parse_file_audit_json(
        repo.file_audit(audited, no_ignore_space_at_eol=True, json=True)
    )
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in literal
    ] == [(1, 3, second)]

    # Asking for internal whitespace to be ignored as well leaves the
    # reindented line with the revision that wrote its text.
    collapsed = parse_file_audit_json(
        repo.file_audit(audited, ignore_space_change=True, json=True)
    )
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"])
        for entry in collapsed
    ] == [
        (1, 1, second),
        (2, 3, first),
    ]


@pytest.mark.smoke
def test_file_audit_attributes_line_ending_changes(new_lore_repo):
    """A final newline is part of the line it ends, so adding one changes the
    last line with content. Switching to CRLF is trailing whitespace, ignored by
    default and counted under `--no-ignore-space-at-eol`. Bytes are written so
    the platform does not translate the line endings."""
    repo: Lore = new_lore_repo("FileAuditLineEndings")

    audited = "audited.txt"

    repo.write_commit_push("First commit", {audited: b"alpha\nbravo"}, offline=True)
    first = repo.revision_info().signature

    repo.write_commit_push("Second commit", {audited: b"alpha\nbravo\n"}, offline=True)
    second = repo.revision_info().signature

    repo.write_commit_push(
        "Third commit", {audited: b"alpha\r\nbravo\r\n"}, offline=True
    )
    third = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in ranges
    ] == [
        (1, 1, first),
        (2, 2, second),
    ]

    literal = parse_file_audit_json(
        repo.file_audit(audited, no_ignore_space_at_eol=True, json=True)
    )
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in literal
    ] == [(1, 2, third)]


@pytest.mark.smoke
def test_file_audit_follows_a_recorded_rename(new_lore_repo):
    """A rename is recorded, so the walk continues into the path the file had
    before it and attribution survives the move. A move recorded as a deletion
    and an addition of an identical file is two unrelated operations, and the
    added lines belong to the revision that added them."""
    repo: Lore = new_lore_repo("FileAuditRename")

    original = "original.txt"
    renamed = "renamed.txt"

    repo.write_commit_push(
        "First commit",
        {original: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    repo.move(original, renamed)
    repo.stage_move(original, renamed, offline=True)
    repo.commit("Rename the file", offline=True)

    repo.write_commit_push(
        "Third commit",
        {renamed: ["alpha\n", "BRAVO\n", "charlie\n"]},
        offline=True,
    )
    third = repo.revision_info().signature

    # The rename revision changed no line, so it owns none: the two untouched
    # lines are still the first revision's, from before the file was renamed.
    ranges = parse_file_audit_json(repo.file_audit(renamed, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in ranges
    ] == [
        (1, 1, first),
        (2, 2, third),
        (3, 3, first),
    ]

    # An unrecorded move is a deletion and an addition of unrelated files, and
    # the added file's history starts where it was added.
    copied = "copied.txt"
    repo.remove_file(renamed)
    repo.write_files({copied: ["alpha\n", "BRAVO\n", "charlie\n"]})
    repo.stage(scan=True, offline=True)
    repo.commit("Move the file without recording it", offline=True)
    fourth = repo.revision_info().signature

    unrecorded = parse_file_audit_json(repo.file_audit(copied, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"])
        for entry in unrecorded
    ] == [(1, 3, fourth)]


@pytest.mark.smoke
def test_file_audit_follows_a_rename_that_also_changed_the_file(new_lore_repo):
    """A revision may rename a file and edit it at once, which is the case a
    walk that followed paths rather than the file itself would get wrong: the
    lines that revision changed are its own, and the lines it left alone still
    belong to the revisions that wrote them under the old path. Renaming twice,
    with edits before, between and after, changes none of that."""
    repo: Lore = new_lore_repo("FileAuditRenameWithEdit")

    original = "original.txt"
    renamed = "renamed.txt"

    repo.write_commit_push(
        "First commit",
        {original: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    # One revision carrying both the move and a change to the moved file.
    repo.move(original, renamed)
    repo.stage_move(original, renamed, offline=True)
    repo.write_files({renamed: ["alpha\n", "BRAVO\n", "charlie\n"]})
    repo.stage(renamed, offline=True)
    repo.commit("Rename the file and change a line", offline=True)
    moved = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(renamed, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in ranges
    ] == [
        (1, 1, first),
        (2, 2, moved),
        (3, 3, first),
    ]

    # A second move, with a change before it and a change after it, so the walk
    # crosses two renames and answers from either side of both.
    once_more = "moved-again.txt"

    repo.write_commit_push(
        "Change a line between the two renames",
        {renamed: ["alpha\n", "BRAVO\n", "CHARLIE\n"]},
        offline=True,
    )
    between = repo.revision_info().signature

    repo.move(renamed, once_more)
    repo.stage_move(renamed, once_more, offline=True)
    repo.commit("Rename the file again", offline=True)

    repo.write_commit_push(
        "Change a line after the second rename",
        {once_more: ["ALPHA\n", "BRAVO\n", "CHARLIE\n"]},
        offline=True,
    )
    after = repo.revision_info().signature

    crossed = parse_file_audit_json(repo.file_audit(once_more, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in crossed
    ] == [
        (1, 1, after),
        (2, 2, moved),
        (3, 3, between),
    ]


@pytest.mark.smoke
def test_file_audit_summary_reads_each_revision_s_author(new_lore_repo):
    """A range names its revision and nothing more of it, so the summary reads
    the author and the date from each revision itself. The author is the
    revision's `merged-by`, and its `committed-by` where it records no merger,
    printed as the one string the revision holds rather than split into a list.
    The JSON record carries the range alone, so a caller reads the same
    attribution through `lore revision info`."""
    repo: Lore = new_lore_repo("FileAuditSummaryAuthor")

    audited = "audited.txt"
    first_merger = "ada@example.com,grace@example.com"
    second_merger = "carol@example.com"

    repo.write_files({audited: ["alpha\n", "bravo\n", "charlie\n", "delta\n"]})
    repo.stage(audited, offline=True)
    repo.revision_metadata_set(
        ["created-by", "creator@example.com", "merged-by", first_merger],
        offline=True,
    )
    repo.commit("First commit", offline=True)
    first = repo.revision_info().signature

    repo.write_files({audited: ["alpha\n", "BRAVO\n", "charlie\n", "delta\n"]})
    repo.stage(audited, offline=True)
    repo.revision_metadata_set(["merged-by", second_merger], offline=True)
    repo.commit("Second commit", offline=True)
    second = repo.revision_info().signature

    # A revision no merge created records no merger.
    repo.write_files({audited: ["alpha\n", "BRAVO\n", "charlie\n", "DELTA\n"]})
    repo.stage(audited, offline=True)
    repo.revision_metadata_set(["created-by", "creator@example.com"], offline=True)
    repo.commit("Third commit", offline=True)
    third = repo.revision_info().signature
    third_metadata = _revision_metadata(repo, third)
    assert "merged-by" not in third_metadata, third_metadata

    rows = [
        line.split() for line in repo.file_audit(audited).splitlines() if line.strip()
    ]
    assert [row[:2] for row in rows] == [
        ["1", first],
        ["2", second],
        ["3", first],
        ["4", third],
    ], f"each row names its own revision: {rows}"
    assert [row[2] for row in rows[:3]] == [
        first_merger,
        second_merger,
        first_merger,
    ], f"a revision's merger is its author: {rows}"
    assert rows[3][2] != "creator@example.com", f"the creator is not the author: {rows}"
    assert rows[3][2] != "-", f"the committer is the author: {rows}"

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert _revision_metadata(repo, ranges[0]["revision"])["merged-by"] == (
        first_merger
    )


@pytest.mark.smoke
def test_file_audit_follows_a_path_into_a_linked_repository(new_lore_repo):
    """A path resolving into a linked repository carries that repository's node
    ids and its own chain, so the walk continues there. Every range names the
    repository its revision came from rather than the one the audit was run in,
    so a consumer never takes one repository for the whole answer."""
    outer: Lore = new_lore_repo("FileAuditLinkOuter")
    linked: Lore = new_lore_repo("FileAuditLinkInner")

    source = "shared/audited.txt"

    linked.write_commit_push(
        "First commit in the linked repository",
        {source: ["alpha\n", "bravo\n", "charlie\n"]},
    )
    first = linked.revision_info().signature

    linked.write_commit_push(
        "Second commit in the linked repository",
        {source: ["alpha\n", "BRAVO\n", "charlie\n"]},
    )
    second = linked.revision_info().signature

    outer.link_add("vendor", linked.get_id(), "shared")
    outer.commit("Link the shared repository")
    outer.push()

    audited = "vendor/audited.txt"
    ranges = parse_file_audit_json(outer.file_audit(audited, json=True))

    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"], entry["revision"])
        for entry in ranges
    ] == [
        (1, 1, "attributed", first),
        (2, 2, "attributed", second),
        (3, 3, "attributed", first),
    ], f"the walk continues in the linked repository's own chain: {ranges}"

    assert {entry["repository"] for entry in ranges} == {linked.get_id()}, (
        f"every range names the repository it came from: {ranges}"
    )
    assert outer.get_id() != linked.get_id(), "the two repositories must differ"

    # The summary keeps each range's revision, and marks the author and date it
    # could not read as unresolved.
    rows = [
        line.split() for line in outer.file_audit(audited).splitlines() if line.strip()
    ]
    assert rows == [
        ["1", first, "unresolved", "unresolved"],
        ["2", second, "unresolved", "unresolved"],
        ["3", first, "unresolved", "unresolved"],
    ], f"a linked revision's attribution is not read here: {rows}"


@pytest.mark.smoke
def test_file_audit_offline_reports_a_link_it_cannot_reach(new_lore_repo):
    """A linked repository out of reach is reported against the link, for the
    ranges it affects alone. The lines the walk resolved against content it does
    hold are answers in their own right and still come back."""
    outer: Lore = new_lore_repo("FileAuditLinkUnreachableOuter")
    linked: Lore = new_lore_repo("FileAuditLinkUnreachableInner")

    source = "shared/audited.txt"

    linked.write_commit_push(
        "First commit in the linked repository",
        {source: ["alpha\n", "bravo\n", "charlie\n"]},
    )

    outer.link_add("vendor", linked.get_id(), "shared")
    outer.commit("Link the shared repository")
    outer.push()

    audited = "vendor/audited.txt"

    # A second revision in the linked repository, so the walk has a step to take
    # before it runs out of content.
    outer.write_commit_push(
        "Second commit through the link",
        {audited: ["alpha\n", "BRAVO\n", "charlie\n"]},
    )

    # `--cache` keeps the payloads the clone fetches, which for a clone is the
    # revision it is synced to. The revision behind it is on the server alone,
    # which is the sparse instance the rule is about.
    clone = outer.clone(cache=True)

    # One line edited on disk, so the file carries an answer that needs no walk
    # at all and must survive the walk stopping.
    clone.write_files({audited: ["alpha\n", "BRAVO\n", "CHARLIE\n"]})

    offline = parse_file_audit_json(clone.file_audit(audited, json=True, offline=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"]) for entry in offline
    ] == [
        (1, 2, "link-unavailable"),
        (3, 3, "uncommitted"),
    ], f"the link out of reach affects its own ranges alone: {offline}"
    assert {entry["repository"] for entry in offline} == {linked.get_id()}, (
        f"every range still names the repository the file lives in: {offline}"
    )

    # With the remote reachable the same walk resolves the lines the working tree
    # did not claim.
    ranges = parse_file_audit_json(clone.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"]) for entry in ranges
    ] == [
        (1, 1, "attributed"),
        (2, 2, "attributed"),
        (3, 3, "uncommitted"),
    ], f"a reachable link resolves every line the walk was given: {ranges}"


@pytest.mark.smoke
def test_file_audit_offline_reports_content_it_cannot_reach(new_lore_repo):
    """A sparse instance holds the version it is synced to and not the ones
    behind it. By default the walk fetches what it lacks through the existing
    read path and resolves the whole file. Under `--offline` it reads local
    content only and reports what is left as `content-unavailable` rather than as
    a removal, the lines it did resolve standing."""
    repo: Lore = new_lore_repo("FileAuditOffline")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit", {audited: ["alpha\n", "bravo\n", "charlie\n"]}
    )
    first = repo.revision_info().signature

    # A fresh clone keeps the states it read but not the payloads it fetched, so
    # the first revision's content is on the server alone. The two revisions
    # committed here are the clone's own, so their content is local.
    clone = repo.clone()

    clone.write_files({audited: ["alpha\n", "BRAVO\n", "charlie\n"]})
    clone.stage(audited, offline=True)
    clone.commit("Second commit", offline=True)
    second = clone.revision_info().signature

    clone.write_files({audited: ["alpha\n", "BRAVO\n", "CHARLIE\n"]})
    clone.stage(audited, offline=True)
    clone.commit("Third commit", offline=True)
    third = clone.revision_info().signature

    # Offline before online: the walk below fetches the missing version, and
    # reading offline first leaves no question of what was in the local store.
    offline = parse_file_audit_json(clone.file_audit(audited, json=True, offline=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"]) for entry in offline
    ] == [
        (1, 2, "content-unavailable"),
        (3, 3, "attributed"),
    ], f"offline, the version the clone does not hold is out of reach: {offline}"
    assert offline[1]["revision"] == third, (
        f"the line resolved against local content keeps its revision: {offline}"
    )

    # With a remote to read through, the same walk resolves the whole file.
    ranges = parse_file_audit_json(clone.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"], entry["revision"])
        for entry in ranges
    ] == [
        (1, 1, "attributed", first),
        (2, 2, "attributed", second),
        (3, 3, "attributed", third),
    ], f"a version absent locally is fetched by default: {ranges}"


@pytest.mark.smoke
def test_file_audit_ends_with_the_error_when_the_remote_is_unreachable(
    request,
    tmp_path_factory,
    lore_server_executable_path,
    new_lore_repo,
):
    """A sparse instance whose remote cannot be reached is not in a configured
    state: the walk cannot fetch what it lacks, and the audit ends with the
    connection error on the completion event rather than reporting the lines it
    could not read as unavailable. Every line still comes back: those resolved
    before the failure keep their outcomes, and the rest are `unresolved`.

    The test runs its own server so that stopping it leaves every other test's
    server alone."""
    # The client reaches gRPC and QUIC through one URL, so they share a port.
    shared_port = allocate_free_port()
    server_ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(
        request, tmp_path_factory, server_ports
    )
    server_proc, server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    try:
        repo: Lore = new_lore_repo(
            "FileAuditUnreachable",
            remote_url=f"lore://127.0.0.1:{server_ports['quic']}/",
        )

        audited = "audited.txt"
        repo.write_commit_push(
            "First commit", {audited: ["alpha\n", "bravo\n", "charlie\n"]}
        )

        # A fresh clone does not keep the payloads it fetched, so the first
        # revision's content is on the server alone. The two revisions committed
        # here are the clone's own, so their content is local.
        clone = repo.clone()
        clone.write_files({audited: ["alpha\n", "bravo\n", "CHARLIE\n"]})
        clone.stage(audited, offline=True)
        clone.commit("Second commit", offline=True)
        clone.write_files({audited: ["alpha\n", "BRAVO\n", "CHARLIE\n"]})
        clone.stage(audited, offline=True)
        clone.commit("Third commit", offline=True)
    finally:
        _kill_server_by_pid(server_proc.pid, server_log_path, label="audit server")
        server_log_fd.close()

    # Line 1 edited on disk needs no walk, and the first step claims line 2 for
    # the third commit. The second step needs the first revision's content,
    # which only the stopped server holds, so line 3 is reported as unresolved.
    clone.write_files({audited: ["ALPHA\n", "BRAVO\n", "CHARLIE\n"]})

    output = clone.file_audit(audited, json=True, check=False)

    complete = parse_complete_json(output)
    assert complete is not None and complete["status"] in (
        DISCONNECTED,
        NOT_CONNECTED,
    ), f"the audit ends with the connection error: {output}"
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["outcome"])
        for entry in parse_file_audit_json(output)
    ] == [(1, 1, "uncommitted"), (2, 2, "attributed"), (3, 3, "unresolved")], (
        f"the lines resolved before the failure stand, and the line the walk did "
        f"not reach is reported as unresolved: {output}"
    )

    # The summary prints the same ranges, and the command still fails.
    with pytest.raises(LoreException) as failure:
        clone.file_audit(audited)
    rows = [line.split() for line in str(failure.value).splitlines() if line.strip()]
    assert ["1", "uncommitted"] in rows, failure.value
    assert any(row[0] == "2" for row in rows), failure.value
    assert ["3", "unresolved"] in rows, failure.value


@pytest.mark.smoke
def test_file_audit_attributes_restored_lines_to_the_restoring_revision(new_lore_repo):
    """A restore is where the restored content re-entered this history, so the
    restoring revision owns the lines it changed back. The revision it took them
    from is recorded on the restoring revision, for a consumer that wants to
    follow it."""
    repo: Lore = new_lore_repo("FileAuditRestore")

    audited = "audited.txt"

    # A restore reads the branch's remote head, so both revisions are pushed.
    repo.write_commit_push("First commit", {audited: ["alpha\n", "bravo\n"]})
    first = repo.revision_info().signature

    repo.write_commit_push("Second commit", {audited: ["alpha\n", "BRAVO\n"]})

    repo.sync(first)
    repo.revision_restore("Restore the first commit")
    restored = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in ranges
    ] == [
        (1, 1, first),
        (2, 2, restored),
    ]
    assert _revision_metadata(repo, restored)["restored-from"] == first


@pytest.mark.smoke
def test_file_audit_json_carries_the_range_alone(new_lore_repo):
    """`--json` emits one line-delimited event per range carrying the repository,
    the revision, the line span and the outcome, and nothing the caller already
    knows or reads from the revision itself."""
    repo: Lore = new_lore_repo("FileAuditJson")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    repo.write_commit_push(
        "Second commit",
        {audited: ["alpha\n", "BRAVO\n"]},
        offline=True,
    )
    second = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert ranges == [
        {
            "repository": repo.get_id(),
            "revision": first,
            "lineFirst": 1,
            "lineLast": 1,
            "outcome": "attributed",
        },
        {
            "repository": repo.get_id(),
            "revision": second,
            "lineFirst": 2,
            "lineLast": 2,
            "outcome": "attributed",
        },
    ]


@pytest.mark.smoke
def test_file_audit_names_a_client_merge_as_the_author_of_what_it_merged(
    new_lore_repo,
):
    """A line a merge changed belongs to the merge revision, and a caller reads
    that revision's own metadata. `lore branch merge` inherits nothing unless
    asked, so the merge names whoever ran it; the revision records `merged-by`
    for a reader to see why, and `--inherit-metadata created-by` carries the real
    author across."""
    repo: Lore = new_lore_repo("FileAuditClientMerge")

    audited = "audited.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    first = repo.revision_info().signature

    repo.branch_create("feature", offline=True)
    repo.write_files({audited: ["ALPHA\n", "bravo\n", "charlie\n"]})
    repo.stage(audited, offline=True)
    repo.revision_metadata_set(
        ["created-by", "feature.author@example.com"], offline=True
    )
    repo.commit("Change the first line on the branch", offline=True)

    repo.branch_switch("main", offline=True)
    repo.write_commit_push(
        "Change the last line on main",
        {audited: ["alpha\n", "bravo\n", "CHARLIE\n"]},
        offline=True,
    )
    on_main = repo.revision_info().signature

    repo.branch_merge("feature", message="Merge feature into main", offline=True)
    merge = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert [
        (entry["lineFirst"], entry["lineLast"], entry["revision"]) for entry in ranges
    ] == [
        (1, 1, merge),
        (2, 2, first),
        (3, 3, on_main),
    ]

    merged = _revision_metadata(repo, ranges[0]["revision"])
    assert merged.get("merged-by"), f"a merge records who made it: {merged}"
    assert not merged.get("fast-forward-merge"), merged
    # Nothing was inherited, so the merge names whoever ran it rather than the
    # author of the work it brought in. A commit fills `created-by` with the
    # committing user where it is unset, so the runner is what `committed-by`
    # holds: the field is pinned to that rather than only to "not the branch
    # author", which an empty value would satisfy.
    assert merged.get("committed-by"), f"a merge records who committed it: {merged}"
    assert merged.get("created-by") == merged["committed-by"], (
        f"an uninherited merge names the actor that committed it: {merged}"
    )
    assert merged["created-by"] != "feature.author@example.com", (
        f"the author of the merged work was not asked for: {merged}"
    )

    inheriting: Lore = new_lore_repo("FileAuditClientMergeInherit")
    inheriting.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"]},
        offline=True,
    )
    inheriting.branch_create("feature", offline=True)
    inheriting.write_files({audited: ["ALPHA\n", "bravo\n", "charlie\n"]})
    inheriting.stage(audited, offline=True)
    inheriting.revision_metadata_set(
        ["created-by", "feature.author@example.com"], offline=True
    )
    inheriting.commit("Change the first line on the branch", offline=True)
    inheriting.branch_switch("main", offline=True)
    inheriting.write_commit_push(
        "Change the last line on main",
        {audited: ["alpha\n", "bravo\n", "CHARLIE\n"]},
        offline=True,
    )
    inheriting.branch_merge(
        "feature",
        message="Merge feature into main",
        inherit_metadata=["created-by"],
        offline=True,
    )

    inherited = parse_file_audit_json(inheriting.file_audit(audited, json=True))
    inherited_metadata = _revision_metadata(inheriting, inherited[0]["revision"])
    assert inherited_metadata["created-by"] == "feature.author@example.com", (
        f"the named key carries the author of the merged work across: "
        f"{inherited_metadata}"
    )


@pytest.mark.smoke
def test_file_audit_keeps_the_author_a_server_merge_preserved(new_lore_repo):
    """The server's fast-forward merge copies the incoming revision's metadata
    wholesale, so the author of the work survives onto the revision it creates,
    which owns the line it merged. That revision records the fast-forward flag,
    which is what says a merge the client never made stands behind it."""
    repo: Lore = new_lore_repo("FileAuditServerMerge")

    audited = "audited.txt"
    other = "other.txt"

    repo.write_commit_push(
        "First commit",
        {audited: ["alpha\n", "bravo\n", "charlie\n"], other: ["one\n"]},
    )

    repo.branch_create("feature")
    repo.write_commit_push(
        "Change the first line on the branch",
        {audited: ["ALPHA\n", "bravo\n", "charlie\n"]},
    )

    repo.branch_switch("main")
    repo.branch_merge_start("feature", no_commit=True)
    repo.revision_metadata_set(["created-by", "feature.author@example.com"])
    repo.commit("Merge feature into main")

    # Advance main from another instance, so the merge no longer descends from
    # the branch tip and the server has to merge the push itself.
    clone = repo.clone()
    clone.write_commit_push("Concurrent change on main", {other: ["two\n"]})

    repo.push(fast_forward_merge=True)
    repo.sync()
    server_merge = repo.revision_info().signature

    ranges = parse_file_audit_json(repo.file_audit(audited, json=True))
    assert ranges[0]["lineFirst"] == 1 and ranges[0]["lineLast"] == 1, (
        f"the merged change stands alone on the first line: {ranges}"
    )
    assert ranges[0]["revision"] == server_merge, (
        f"the revision the server created owns the line it merged: {ranges}"
    )
    merged = _revision_metadata(repo, server_merge)
    assert merged.get("fast-forward-merge"), merged
    assert merged.get("created-by") == "feature.author@example.com", (
        f"the author survives the server's merge: {merged}"
    )
    assert merged.get("merged-by"), f"a merge records who made it: {merged}"


@pytest.mark.smoke
def test_file_write_dry_run_writes_nothing(new_lore_repo, scratch_dir):
    """`file write --dry-run` writes neither an output outside the repository nor one inside
    it, where `file write` without it does."""
    repo: Lore = new_lore_repo("FileWriteDryRun")
    repo.write_commit_push("Initial", {"source.txt": b"source\n"}, offline=True)
    outside = scratch_dir("file-write-dry-run", create=True) / "outside.txt"
    inside = os.path.join(repo.path, "inside.txt")

    for output in (str(outside), inside):
        written = repo.file_write(
            path="source.txt", output=output, dry_run=True, offline=True
        )
        assert "Would write" in written, written
        assert not os.path.exists(output), f"file write --dry-run wrote {output}"

    for output in (str(outside), inside):
        repo.file_write(path="source.txt", output=output, offline=True)
        with open(output, "rb") as written_file:
            assert written_file.read() == b"source\n"
