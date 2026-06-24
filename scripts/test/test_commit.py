# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_commit(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"
    unicode_file = os.path.join("奇怪的路徑", "کاراکترهای یونیکد")
    long_path_file = os.path.join(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddd",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddd",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddd",
    )
    long_file_case_one = os.path.join(
        "dirone",
        "a-long-file-name-forcing-an-external-node-name-with-a-specific-case-variation-in-the-name",
    )
    long_file_case_two = os.path.join(
        "dirtwo",
        "a-long-file-name-forcing-an-external-node-name-with-a-specific-case-variation-in-the-NAME",
    )

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    repo.make_dirs(os.path.dirname(unicode_file))
    with repo.open_file(unicode_file, "w+", encoding="utf-8") as output_file:
        output_file.writelines(["只需將一些文本寫入文件即可\n"])

    repo.make_dirs(os.path.dirname(long_file_case_one))
    with repo.open_file(long_file_case_one, "w+b") as output_file:
        output_file.write(os.urandom(1234))

    repo.make_dirs(os.path.dirname(long_file_case_two))
    with repo.open_file(long_file_case_two, "w+b") as output_file:
        output_file.write(os.urandom(1234))

    _large_file_size = 345678901
    repo.make_dirs(os.path.dirname(long_path_file))
    with repo.open_file(long_path_file, "w+b") as output_file:
        output_file.write(os.urandom(345678901))

    # Stage the files
    repo.stage(scan=True, offline=True)

    # Commit the files
    repo.commit("Test commit", offline=True)

    # Verify the repository
    repo.repository_verify(offline=True)

    # Test case variations
    case_variation_support = True
    case_variation_one = os.path.join("some", "pathCaseVariation", "file.txt")
    case_variation_two = os.path.join("some", "PathCaseVariation", "other.txt")
    case_variation_three = os.path.join("some", "Pathcasevariation", "third.txt")
    case_variation_stage = os.path.join("some", "pathCasevariation", "third.txt")

    repo.make_dirs(os.path.dirname(case_variation_one))
    # noinspection PyBroadException
    try:
        repo.make_dirs(os.path.dirname(case_variation_two))
        repo.make_dirs(os.path.dirname(case_variation_three))
        with repo.open_file(case_variation_one, "w+b") as output_file:
            output_file.write(os.urandom(1234))
        with repo.open_file(case_variation_two, "w+b") as output_file:
            output_file.write(os.urandom(1234))
        with repo.open_file(case_variation_three, "w+b") as output_file:
            output_file.write(os.urandom(1234))

    except:
        # File system does not support case variations
        case_variation_support = False

    if case_variation_support:
        repo.stage(case_variation_stage, offline=True)
        repo.commit("Test case variation", offline=True)

        repo.stage(case_variation_one, case="keep", offline=True)
        repo.commit("Test case variation", offline=True)

        repo.stage(case_variation_two, case="keep", offline=True)
        repo.commit("Test case variation", offline=True)

    # Delete a file
    repo.remove_file(unicode_file)

    # Modify a file
    with repo.open_file(long_path_file, "w+b") as output_file:
        output_file.write(os.urandom(100))

    # Stage the files
    repo.stage(scan=True, offline=True)

    # Commit the files
    repo.commit("Test commit 2", offline=True)

    # Verify the repository
    repo.repository_verify(offline=True)

    print("*****************************************")
    print("* Status tests, unstaged")
    print("*****************************************")

    first_path_file = "first/path/file.txt"
    first_other_file = "first/other/file.foo"
    second_path_file = "second/path/file.txt"

    repo.make_dirs(os.path.dirname(first_path_file))
    repo.make_dirs(os.path.dirname(first_other_file))
    repo.make_dirs(os.path.dirname(second_path_file))

    with repo.open_file(first_path_file, "w+b") as output_file:
        output_file.write(os.urandom(100))
    with repo.open_file(first_other_file, "w+b") as output_file:
        output_file.write(os.urandom(100))
    with repo.open_file(second_path_file, "w+b") as output_file:
        output_file.write(os.urandom(100))

    # Check status
    output = repo.status(unstaged=True, offline=True)

    assert "A first" in output, "Missing path in status: first"
    assert "A second" in output, "Missing file in status: second"

    # Check partial status
    output = repo.status("first", unstaged=True, offline=True)

    assert "A first/path" in output, "Missing path in partial status: first"
    assert "A first/other" in output, "Missing path in partial status: first"
    assert "A second" not in output, "Unexpected file in partial status: second"

    output = repo.status(os.path.join("first", "path"), unstaged=True, offline=True)

    assert "A " + first_path_file in output, "Missing path in partial status: first"
    assert "A first/other" not in output, (
        "Unexpected path in partial status: first/other"
    )
    assert "A second" not in output, "Unexpected file in partial status: second"

    print("*****************************************")
    print("* Status tests, staged")
    print("*****************************************")

    # Stage changes
    _output = repo.stage("first", offline=True)

    # Check status. `second` stays reported as a dirty/untracked entry from the
    # earlier scan (status --unstaged is a scan alias that persists dirty state).
    output = repo.status(offline=True)

    assert "A " + first_path_file in output, (
        "Missing path in staged status: " + first_path_file
    )
    assert "A " + first_other_file in output, (
        "Missing path in staged status: " + first_other_file
    )
    assert "A second" in output, "Missing dirty file in status: second"

    # Check partial status
    output = repo.status(os.path.join("first", "path"), offline=True)

    assert "A " + first_path_file in output, (
        "Missing path in staged status: " + first_path_file
    )
    assert "A first/other" not in output, (
        "Unexpected path in staged status: first/other"
    )
    assert "A second" not in output, "Unexpected file in staged status: second"

    output = repo.status("second", offline=True)

    assert "A first" not in output, "Unexpected path in staged status: first"
    assert "A second" in output, "Missing dirty file in status: second"

    output = repo.status("second", offline=True, unstaged=True)

    assert "A first" not in output, "Unexpected path in staged status: first"
    assert "A second/path" in output, "Missing file in unstaged status: second"

    output = repo.status(["first", second_path_file], offline=True, unstaged=True)

    assert "A first/path" in output, "Missing path in staged status: first/path"
    assert "A second/path" in output, "Missing file in unstaged status: second"

    # Commit the files
    repo.stage(scan=True, offline=True)
    repo.commit("Test commit 3", offline=True)

    output = repo.status(["first", "second"], offline=True)

    assert " first" not in output, "Unexpected path in staged status: first"
    assert " second" not in output, "Unexpected path in staged status: second"

    output = repo.status([first_path_file, second_path_file], offline=True)

    assert " first" not in output, "Unexpected path in staged status: first"

    assert " second" not in output, "Unexpected path in staged status: second"

    output = repo.status(["first", "second"], unstaged=True, offline=True)

    assert " first" not in output, "Unexpected path in staged status: first"
    assert " second" not in output, "Unexpected path in staged status: second"

    output = repo.status(
        [first_other_file, second_path_file], unstaged=True, offline=True
    )

    assert " first" not in output, "Unexpected path in staged status: first"
    assert " second" not in output, "Unexpected path in staged status: second"

    # Revision history tests
    # List all revisions
    output = repo.history(offline=True)

    assert len(output) > 0, "No revision information in history"

    # List the latest two revisions
    output = repo.history("2", offline=True)

    assert len(output) > 0, "No revision information in history when listing latest two"

    # Get signatures of the latest two revisions
    latest_revision = output[-1].signature
    revision = output[-2].signature

    assert latest_revision != "" or revision != "", (
        "Signatures of latest two revisions not found in history"
    )

    # List all revisions starting from the second latest
    output = repo.history(revision=revision, offline=True)

    assert len(output) > 0, (
        "No revision information in history when listing starting from the second latest"
    )
    assert latest_revision not in [item.revision for item in output], (
        "Latest revision found in list supposed to start from second last"
    )

    # Amend tests
    def find_branch(command_output: str) -> str | None:
        for line in command_output.splitlines():
            if line.startswith("Branch"):
                return line.split(": ")[1].removesuffix("\n")
        return None

    # Crate file for the commit
    amend_file = "amend-file.txt"

    with repo.open_file(amend_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    original_commit_message = "Original commit message"
    repo.stage(amend_file, offline=True)
    output = repo.revision_commit(original_commit_message, offline=True)

    commit_branch = find_branch(output)
    assert commit_branch is not None, "Unable to find branch in commit output"

    new_commit_message = "New commit message"
    output = repo.revision_amend(new_commit_message, offline=True)

    amend_branch = find_branch(output)
    assert amend_branch is not None, "Unable to find branch in amend output"

    assert amend_branch == commit_branch, (
        f"Amend branch ({amend_branch}) didn't match commit branch ({commit_branch})"
    )
    assert new_commit_message in output, (
        f"Amend output didn't include new commit message"
    )


@pytest.mark.smoke
def test_commit_stats(new_lore_repo):
    # Commit with --stats finalizes the revision and clears staging.
    repo: Lore = new_lore_repo()

    seed_file = "seed.txt"
    with repo.open_file(seed_file, "w+") as output_file:
        output_file.writelines(["seed\n"])
    repo.stage(scan=True, offline=True)
    repo.commit("Seed commit", offline=True)
    before = int(repo.revision_info(offline=True).revision)

    stats_file = "stats-file.bin"
    with repo.open_file(stats_file, "w+b") as output_file:
        output_file.write(os.urandom(512 * 1024))
    repo.stage(scan=True, offline=True)

    repo.commit("Stats commit", stats=True, offline=True)

    after = int(repo.revision_info(offline=True).revision)
    assert after == before + 1, "Revision did not advance after --stats commit"

    output = repo.status(unstaged=True, offline=True)
    assert stats_file not in output, "Staging area not cleared after --stats commit"

    repo.repository_verify(offline=True)


@pytest.mark.smoke
def test_commit_dry_run(new_lore_repo):
    """`commit --dry-run` runs the full pipeline and reports the would-be
    revision, but performs no mutating writes; a subsequent real commit lands."""
    repo: Lore = new_lore_repo()

    # Baseline revision so history is non-empty.
    with repo.open_file("base.txt", "w+") as output_file:
        output_file.writelines(["base\n"])
    repo.stage(scan=True, offline=True)
    repo.commit("Baseline commit", offline=True)

    baseline_count = len(repo.history(offline=True))

    # Stage a new change.
    with repo.open_file("dry-run.txt", "w+") as output_file:
        output_file.writelines(["dry run content\n"])
    repo.stage(scan=True, offline=True)

    assert "dry-run.txt" in repo.status(offline=True), (
        "Expected dry-run.txt to be staged before the dry-run commit"
    )

    repo.commit("Dry run commit", dry_run=True, offline=True)

    assert len(repo.history(offline=True)) == baseline_count, (
        "Dry-run commit added a revision to history"
    )
    assert "dry-run.txt" in repo.status(offline=True), (
        "Dry-run commit consumed the staged change"
    )

    repo.commit("Real commit", offline=True)

    assert len(repo.history(offline=True)) == baseline_count + 1, (
        "Real commit after dry-run did not add exactly one revision"
    )
    assert "dry-run.txt" not in repo.status(offline=True), (
        "Real commit did not clear the staged change"
    )

    repo.repository_verify(offline=True)
