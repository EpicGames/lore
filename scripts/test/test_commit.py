# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from error_types import UnknownLoreError
from lore_parsers import (
    parse_commit_stats_json,
    parse_jsonl,
    parse_status_json,
    parse_status_summary_json,
)

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

    repo.commit("Stats commit", stats=2, offline=True)

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


@pytest.mark.smoke
def test_failed_commit_records_no_modified_times(new_lore_repo):
    """A commit reads every file it commits and records the modified time it read each at,
    but those times describe the revision it is building, not the one the working copy is
    on. A commit that fails partway leaves the working copy on the previous revision, so
    none of the times it took may answer for any file.

    Both files are edited without changing size, so only a content comparison tells the
    edits from the committed bytes. Recording per file as it is read would leave the file
    that was fragmented before the failure answering from a time no revision backs, and the
    edit disappears from status and from every later commit.
    """
    repo: Lore = new_lore_repo()

    size = 4096
    committed = "committed-first.bin"
    removed = "removed-before-commit.bin"
    removed_content = os.urandom(size)
    with repo.open_file(committed, "w+b") as f:
        f.write(os.urandom(size))
    with repo.open_file(removed, "w+b") as f:
        f.write(removed_content)
    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)

    revision_before = parse_jsonl(
        repo.status(json=True, offline=True), "repositoryStatusRevision"
    )[-1]["revisionNumber"]

    # Same sizes, new content.
    for name in (committed, removed):
        with repo.open_file(name, "w+b") as f:
            f.write(os.urandom(size))
    repo.stage(scan=True, offline=True)

    # Removing a staged file fails the commit where it reads that file's metadata, after
    # the other one has been fragmented. Unlike a killed process, this exits cleanly, so
    # anything written to the mutable store along the way is flushed and survives.
    os.remove(os.path.join(repo.path, removed))
    with pytest.raises(UnknownLoreError):
        repo.commit(offline=True)

    # Put the committed bytes back. `reset` cannot do it while the file is staged, and
    # unstaging would drop it from the retry altogether, so the content is restored
    # directly: the file stays staged but holds what the current revision addresses, and
    # the retry has to recognise from its content that it is not a change.
    with repo.open_file(removed, "w+b") as f:
        f.write(removed_content)

    revision_after = parse_jsonl(
        repo.status(json=True, offline=True), "repositoryStatusRevision"
    )[-1]["revisionNumber"]
    assert revision_after == revision_before, (
        "the failed commit must not have produced a revision, "
        f"was {revision_before}, now {revision_after}"
    )

    summary = parse_status_summary_json(
        repo.status(scan=True, json=True, offline=True)
    )
    assert summary is not None, "scan must emit a repositoryStatusSummary event"
    assert summary["mtimeMatches"] == 0, (
        "no file may be answered by a modified time the failed commit took, as the working "
        f"copy is still on the revision before it, got {summary}"
    )

    output = repo.commit(json=True, offline=True, stats=1)
    commit_end = parse_jsonl(output, "revisionCommitEnd")
    assert commit_end, "commit must emit a revisionCommitEnd event"
    count = commit_end[-1]["count"]
    assert count["fileTotal"] == 2, (
        f"the retry must still carry both staged files, got {count}"
    )
    assert count["fileModifyCount"] == 1, (
        f"only {committed} still differs; the restored file matches the revision it is "
        f"committed against and is not a modification, got {count}"
    )

    # The commit read both staged files and committed the content of only one, so
    # this is where read and committed have to be reported apart. Conflating them
    # would either overstate what the revision holds or understate what the
    # commit cost to produce.
    stats = parse_commit_stats_json(output)
    assert stats is not None, "commit must emit its statistics event"
    files = stats["files"]
    assert files["filesRead"] == 2, f"both staged files were read, got {files}"
    assert files["bytesTransferred"] == 2 * size, (
        f"both files' content was read off disk, got {files}"
    )
    assert files["files"] == 1, (
        f"one file was committed; the other matched what it is committed against, "
        f"got {files}"
    )
    assert files["fileBytes"] == size, (
        f"the committed content is the one file's {size} bytes, not the {2 * size} "
        f"that were read, got {files}"
    )

    revision_committed = parse_jsonl(
        repo.status(json=True, offline=True), "repositoryStatusRevision"
    )[-1]["revisionNumber"]
    assert revision_committed == revision_before + 1, (
        f"the retry must produce one revision, was {revision_before}, "
        f"now {revision_committed}"
    )

    assert not parse_status_json(repo.status(scan=True, json=True, offline=True)), (
        "the working copy must be clean once the retry has committed"
    )


@pytest.mark.smoke
def test_commit_stats_accuracy(new_lore_repo):
    """The statistics a large commit reports must be arithmetically true: a caller
    decides how much data it moved from these numbers alone.

    The tree puts something in every bucket. Files past the fragment threshold
    give both data fragments and fragment lists; a file moved from the baseline
    revision is re-read at an address the store already holds in full, so the
    deduplicated count cannot be zero; random and highly compressible bytes
    together keep the compressed payload from being a fixed fraction of the
    content. Offline, so a copy or an upload counted here would mean the remote
    counters read the wrong thing.
    """
    repo: Lore = new_lore_repo()

    # The size a single fragment covers; content past it is chunked and gains a
    # fragment list. Kept in step with FRAGMENT_SIZE_THRESHOLD in lore-base.
    fragment_threshold = 256 * 1024
    large_size = 2 * fragment_threshold
    large_content = os.urandom(large_size)

    # Baseline revision, so the commit under test is a delta rather than an
    # initial import of the whole tree. The file committed here is moved below:
    # a move re-reads the content under the identity it already has, so every
    # fragment of it resolves to a full match and is deduplicated outright.
    with repo.open_file("baseline.txt", "w+") as output_file:
        output_file.writelines(["baseline\n"])
    repo.make_dirs("large")
    committed_path = os.path.join("large", "committed.bin")
    with repo.open_file(committed_path, "w+b") as output_file:
        output_file.write(large_content)
    repo.stage(scan=True, offline=True)
    repo.commit("Baseline commit", offline=True)

    expected_files = 0
    expected_bytes = 0

    # Small files across several directories: the bulk of the file count.
    small_size = 4096
    for directory in range(8):
        subpath = os.path.join("small", str(directory))
        repo.make_dirs(subpath)
        for index in range(16):
            with repo.open_file(
                os.path.join(subpath, f"{index}.bin"), "w+b"
            ) as output_file:
                output_file.write(os.urandom(small_size))
            expected_files += 1
            expected_bytes += small_size

    # Compressible files, so the payload total is meaningfully below the content
    # total rather than equal to it.
    text_size = 32 * 1024
    repo.make_dirs("compressible")
    for index in range(8):
        with repo.open_file(os.path.join("compressible", f"{index}.txt"), "w+b") as f:
            f.write(b"the same line over and over\n" * (text_size // 28))
        expected_files += 1
        expected_bytes += (text_size // 28) * 28

    # Files past the fragment threshold, which chunk and so produce fragment
    # lists as well as data fragments. Byte-identical to the committed file, so
    # their content is already in the store under another identity: the payload
    # is loaded back rather than compressed again, but each still needs an entry
    # of its own, so these are processed rather than deduplicated.
    duplicate_count = 3
    for index in range(duplicate_count):
        with repo.open_file(
            os.path.join("large", f"duplicate-{index}.bin"), "w+b"
        ) as output_file:
            output_file.write(large_content)
        expected_files += 1
        expected_bytes += large_size

    repo.stage(scan=True, offline=True)

    # Staged after the scan, which would otherwise see the rename as an add and a
    # delete. The moved file is read again, so it counts toward the content read.
    moved_path = os.path.join("large", "moved.bin")
    repo.move(committed_path, moved_path)
    repo.file_stage_move(committed_path, moved_path, offline=True)
    expected_bytes += large_size

    output = repo.commit("Stats accuracy commit", json=True, offline=True, stats=1)

    events = parse_jsonl(output, "revisionCommitStats")
    assert len(events) == 1, (
        f"the statistics event is emitted once, when the commit has drained its "
        f"writes, got {len(events)}"
    )

    stats = parse_commit_stats_json(output)
    assert stats is not None, "commit must emit a revisionCommitStats event"
    files = stats["files"]
    fragments = stats["fragments"]

    # --- Files, by action -------------------------------------------------
    assert files["added"] == expected_files, (
        f"every file in this commit is new, so all {expected_files} must be "
        f"counted as added, got {files}"
    )
    assert files["modified"] == 0, f"nothing was modified, got {files}"
    assert files["moved"] == 1, f"one file was staged as a move, got {files}"
    assert files["copied"] == 0, f"nothing was staged as a copy, got {files}"
    assert files["deleted"] == 0, f"nothing was deleted, got {files}"
    assert (
        files["files"]
        == files["added"] + files["modified"] + files["moved"] + files["copied"]
    ), f"the file total must be the sum of the per-action counts, got {files}"
    assert files["fileBytes"] == expected_bytes, (
        f"the reported content size must be the bytes written to disk, expected "
        f"{expected_bytes}, got {files['fileBytes']}"
    )
    assert files["filesRead"] == files["files"], (
        f"every file this commit committed, it also read; nothing was staged that "
        f"turned out to match, and no path is view-excluded, got {files}"
    )
    assert files["bytesTransferred"] == expected_bytes, (
        f"and the bytes read must agree with the bytes committed here, got {files}"
    )

    # --- Fragments: the offered/deduplicated/processed split --------------
    assert fragments["fragmentsProduced"] > 0, (
        f"fragments must be counted, got {fragments}"
    )
    assert (
        fragments["fragmentsProduced"]
        == fragments["fragmentsDeduplicated"] + fragments["fragmentsProcessed"]
    ), (
        "every fragment offered was either already stored or processed; the two "
        f"must account for the total, got {fragments}"
    )
    assert fragments["fragmentsDeduplicated"] > 0, (
        f"the moved {large_size} byte file is offered at the address the store "
        f"already holds in full, so its fragments must be deduplicated outright, "
        f"got {fragments}"
    )
    assert fragments["fragmentsProcessed"] > 0, (
        f"most of this commit is new content, got {fragments}"
    )
    assert (
        fragments["dataFragments"]
        + fragments["fragmentlists"]
        + fragments["noPayloadFragments"]
        == fragments["fragmentsProcessed"]
    ), (
        "every fragment that entered the pipeline either had a payload prepared or "
        f"had none; the three must account for the total, got {fragments}"
    )
    assert fragments["noPayloadFragments"] == 0, (
        "offline there is no remote to duplicate an association, so every fragment "
        "that entered the pipeline had to produce a payload; one counted here would "
        f"mean it produced nothing for a reason the report cannot name, got {fragments}"
    )
    assert (
        fragments["dataContentBytes"] + fragments["noPayloadContentBytes"]
        == fragments["processedContentBytes"]
    ), (
        "the content that entered the pipeline is the content of the fragments that "
        f"produced a payload plus that of those that produced none, got {fragments}"
    )

    # --- Fragments: what the stored payloads cost -------------------------
    assert fragments["dataFragments"] > 0, (
        f"content-bearing fragments must be counted, got {fragments}"
    )
    assert fragments["fragmentlists"] > 0, (
        f"a {large_size} byte file is past the {fragment_threshold} byte threshold a "
        f"single fragment covers, so it must chunk and produce a fragment list, got "
        f"{fragments}"
    )
    assert fragments["dataPayloadBytes"] <= fragments["dataContentBytes"], (
        "a stored payload is the content compressed, so it can never exceed the "
        f"content it stands for, got {fragments}"
    )
    assert fragments["dataPayloadBytes"] < fragments["dataContentBytes"], (
        "this tree includes highly compressible files, so compression must have "
        f"shrunk the payload total below the content total, got {fragments}"
    )
    assert fragments["fragmentlistPayloadBytes"] > 0, (
        f"a fragment list has a payload of its own, got {fragments}"
    )

    # --- Fragments: local store writes ------------------------------------
    assert (
        fragments["localWrites"]
        == fragments["localMetadataWrites"] + fragments["localPayloadWrites"]
    ), (
        "each local write either carried a payload or recorded only the header; "
        f"the two must account for the total, got {fragments}"
    )
    assert fragments["localPayloadWrites"] > 0, (
        f"an offline commit stores its payloads locally, got {fragments}"
    )
    assert (
        fragments["localPayloadBytes"]
        == fragments["dataPayloadBytes"] + fragments["fragmentlistPayloadBytes"]
    ), (
        "offline, every payload the commit prepared is written to the local "
        "store and nothing else is, so the payload bytes written must be exactly "
        f"the data and list payloads prepared, got {fragments}"
    )

    # --- Fragments: the remote, which this commit never touched -----------
    assert (
        fragments["remoteWrites"]
        == fragments["remoteCopyWrites"] + fragments["remotePutWrites"]
    ), (
        "each remote write was either a copy or an upload; the two must account "
        f"for the total, got {fragments}"
    )
    assert fragments["remoteWrites"] == 0, (
        f"an offline commit reaches no remote, got {fragments}"
    )
    assert fragments["remotePutBytes"] == 0, (
        f"an offline commit uploads nothing, got {fragments}"
    )

    repo.repository_verify(offline=True)


@pytest.mark.smoke
def test_commit_without_a_statistics_level_reports_nothing(new_lore_repo):
    """What level zero buys is the whole reporting path: no event, and no counters
    kept per fragment in the write pipeline. An event emitted anyway would mean the
    level had stopped being read, and the counters with it."""
    repo: Lore = new_lore_repo()

    with repo.open_file("unreported.bin", "w+b") as output_file:
        output_file.write(os.urandom(4096))
    repo.stage(scan=True, offline=True)

    output = repo.commit("Commit asking for no statistics", json=True, offline=True)

    assert parse_jsonl(output, "revisionCommitStats") == [], (
        "no statistics level was asked for, so no statistics event may be emitted"
    )


@pytest.mark.smoke
def test_commit_stats_report_deletes_and_moves(new_lore_repo):
    """The per-action split must name the action a file was staged with, not just
    that the file changed: a move reported as a modification would tell a caller
    content crossed the wire that never did."""
    repo: Lore = new_lore_repo()

    size = 2048
    for name in ("keep.bin", "to-move.bin", "to-delete.bin", "to-edit.bin"):
        with repo.open_file(name, "w+b") as output_file:
            output_file.write(os.urandom(size))
    repo.stage(scan=True, offline=True)
    repo.commit("Baseline commit", offline=True)

    # The delete and the edit are reconciled by a scan; the move is staged after
    # it, because a scan run over a renamed file would see the two halves as an
    # add and a delete and replace the move it is meant to report.
    repo.remove_file("to-delete.bin")
    with repo.open_file("to-edit.bin", "w+b") as output_file:
        output_file.write(os.urandom(size * 2))
    repo.stage(scan=True, offline=True)

    repo.move("to-move.bin", "moved.bin")
    repo.file_stage_move("to-move.bin", "moved.bin", offline=True)

    stats = parse_commit_stats_json(
        repo.commit("Action split commit", json=True, offline=True, stats=1)
    )
    assert stats is not None, "commit must emit its statistics event"
    files = stats["files"]

    assert files["moved"] == 1, f"one file was staged as a move, got {files}"
    assert files["deleted"] == 1, f"one file was deleted, got {files}"
    assert files["modified"] == 1, f"one file was edited in place, got {files}"
    assert files["added"] == 0, f"nothing was added, got {files}"
    assert files["copied"] == 0, f"nothing was staged as a copy, got {files}"
    assert files["files"] == 2, (
        f"the move and the edit wrote content; the delete wrote none, got {files}"
    )
    assert files["fileBytes"] == size + size * 2, (
        f"the moved file's {size} bytes and the edited file's {size * 2}; the deleted "
        f"file contributes nothing, got {files}"
    )
    # The point of a separate read count: a delete never opens the file. A read
    # count that included it would report content the commit never touched.
    assert files["filesRead"] == 2, (
        f"the move and the edit were read; the delete and the untouched file were "
        f"not, got {files}"
    )
    assert files["bytesTransferred"] == size + size * 2, (
        f"only the two files that were read contribute bytes, got {files}"
    )

    repo.repository_verify(offline=True)
