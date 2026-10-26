# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import logging
import os
import shutil

import pytest
from error_types import LoreException

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_obliterate(new_lore_repo, scratch_dir):
    repo: Lore = new_lore_repo()
    temp = scratch_dir("obliterate", create=True)

    # Generate some files
    multi_reference_file_one = "multi-ref-file-one.txt"
    multi_reference_file_two = os.path.join("subdir", "multi-ref-file-two.txt")
    multi_fragment_file = "multi-frag-file.txt"
    multi_fragment_file_backup = os.path.join(temp, "multi-frag-file.txt")

    multi_reference_content = ["First line\n", "Second line\n", "Third line\n"]

    with repo.open_file(multi_reference_file_one, "w+") as output_file:
        output_file.writelines(multi_reference_content)

    repo.make_dirs(os.path.dirname(multi_reference_file_two))
    with repo.open_file(multi_reference_file_two, "w+") as output_file:
        output_file.writelines(multi_reference_content)

    with repo.open_file(multi_fragment_file, "w+b") as output_file:
        output_file.write(os.urandom(3456789))

    # Store a backup of a file so we can try and re-add it later
    shutil.copy(
        os.path.join(repo.path, multi_fragment_file), multi_fragment_file_backup
    )

    # Stage the files
    repo.stage(scan=True, offline=True)

    # Commit the files
    repo.commit(offline=True)

    # Obliterate first multi-reference file
    output = repo.file_obliterate(path=multi_reference_file_one, offline=True)

    assert "Obliterated 1" in output, "Did not obliterate first multi-reference file"
    assert "0 payloads" in output, "Obliterated payload with an existing reference"

    # Check first multi-reference file. An obliterated address is absent rather than described: the
    # store reports no match instead of a tombstone carrying its flags and sizes.
    output = repo.file_dump(path=multi_reference_file_one, offline=True)

    assert "Store match : 0" in output, (
        "First multi-reference file still resolves after being obliterated"
    )
    # The whole line, not `": 100"`: a bare substring matches an address that
    # happens to begin `100`, which one dumped fragment in 4,096 does, and the
    # content is random.
    assert "Store match : 100" not in output, (
        "Obliterated fragment was described rather than reported absent"
    )

    # Obliterate second multi-reference file
    output = repo.file_obliterate(path=multi_reference_file_two, offline=True)

    assert "Obliterated 1" in output, "Did not obliterate first multi-reference file"
    assert "1 payloads" in output, "Obliterated payload with an existing reference"

    # Obliterate multi-fragment file
    output = repo.file_obliterate(path=multi_fragment_file, offline=True)

    assert not "Obliterated 0" in output, "Did not obliterate multi-fragment file"
    assert not "removed 0 payloads" in output, (
        "Did not obliterate payloads of multi-fragment file"
    )

    # Try to obliterate multi-fragment file again
    output = repo.file_obliterate(path=multi_fragment_file, offline=True)

    assert "already obliterated" in output, (
        "Fragment already obliterated, warning not shown"
    )

    # Check multi-fragment file
    output = repo.file_dump(path=multi_fragment_file, offline=True)

    assert "Store match : 0" in output, (
        "Multi-fragment file still resolves after being obliterated"
    )
    # The whole line, not `": 100"`: a bare substring matches an address that
    # happens to begin `100`, which one dumped fragment in 4,096 does, and the
    # content is random.
    assert "Store match : 100" not in output, (
        "Obliterated fragment was described rather than reported absent"
    )

    # Check whether there are files staged after obliteration
    output = repo.repository_status(offline=True)

    assert "staged for commit" in output, "No files staged after obliterate"

    # Restore the multi-fragment file and then try and re-add it
    shutil.copy(
        multi_fragment_file_backup, os.path.join(repo.path, multi_fragment_file)
    )

    # Stage the files
    repo.stage(scan=True, offline=True)

    # Commit the files
    repo.commit(offline=True)


@pytest.mark.smoke
@pytest.mark.parametrize(
    "by_path,superseded", [(False, False), (True, False), (False, True)]
)
def test_obliterate_warns_before_reporting_deletion_and_still_removes_content(
    new_lore_repo, by_path, superseded
):
    repo: Lore = new_lore_repo("ObliterateLatestWarning")
    repo.write_commit_push("First", {"probe.txt": "current content\n"}, offline=True)
    repo.push()
    info = repo.file_info("probe.txt")[0]
    if superseded:
        repo.write_commit_push(
            "Replace", {"probe.txt": "replacement content\n"}, offline=True
        )
        repo.push()

    if by_path:
        output = repo.file_obliterate(path="probe.txt", json=True)
    else:
        output = repo.file_obliterate(address=f"{info.hash}-{info.context}", json=True)
    events = [json.loads(line) for line in output.splitlines() if line.startswith("{")]
    warning_positions = [
        i
        for i, event in enumerate(events)
        if event["tagName"] == "log" and event["data"]["level"] == "warn"
    ]
    deleted_position = next(
        i for i, event in enumerate(events) if event["tagName"] == "fileObliterate"
    )
    assert warning_positions and max(warning_positions) < deleted_position
    warnings = [events[i]["data"]["message"] for i in warning_positions]
    assert any(
        "impact for omitted branches is unknown" in message for message in warnings
    )
    known_impact = [
        message for message in warnings if "removes content required by" in message
    ]
    assert bool(known_impact) == (not superseded)

    if superseded:
        cloned = repo.clone()
        with cloned.open_file("probe.txt", "r") as content:
            assert content.read() == "replacement content\n"
    else:
        with pytest.raises(LoreException):
            repo.clone()


@pytest.mark.smoke
def test_clone_and_sync_materialize_available_files_after_obliteration(
    new_lore_repo, scratch_dir
):
    repo: Lore = new_lore_repo("ObliterateContinue")
    repo.write_commit_push("Base", {"base.txt": "base content\n"}, offline=True)
    repo.push()
    older = repo.clone()
    available = {f"available-{i}.txt": f"available content {i}\n" for i in range(8)}
    removed = {"removed-one.txt": "removed one\n", "removed-two.txt": "removed two\n"}
    repo.write_commit_push("Add files", available | removed, offline=True)
    repo.push()
    for path in removed:
        info = repo.file_info(path)[0]
        repo.file_obliterate(address=f"{info.hash}-{info.context}")

    destination = scratch_dir("clone-after-obliterate")
    with pytest.raises(LoreException) as clone_failure:
        repo.clone(path=destination, json=True)
    with pytest.raises(LoreException) as sync_failure:
        older.sync("@2", json=True)

    for failure in (clone_failure, sync_failure):
        events = [
            json.loads(line)
            for line in str(failure.value).splitlines()
            if line.startswith("{")
        ]
        complete = [event for event in events if event["tagName"] == "complete"]
        assert len(complete) == 1
        assert complete[0]["data"]["status"] != 0
        assert not any(event["tagName"] == "error" for event in events)
        warnings = [
            event
            for event in events
            if event["tagName"] == "log" and event["data"]["level"] == "warn"
        ]
        for path in removed:
            assert any(path in json.dumps(event) for event in warnings)

    # A nonzero result is expected, but every available file must still be written.
    # The unit tests control scheduling so an early failure cannot be hidden by a
    # small fixture that happened to enqueue all its work before seeing the error.
    for root in (destination, older.path):
        for path, content in available.items():
            with open(os.path.join(root, path)) as materialized:
                assert materialized.read() == content
        for path in removed:
            assert not os.path.exists(os.path.join(root, path))


@pytest.mark.smoke
def test_obliterate_dry_run_removes_nothing(new_lore_repo, scratch_dir):
    """`file obliterate --dry-run` keeps the content in the local store and on the remote,
    where `file obliterate` removes it."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("First", {"secret.txt": b"secret content\n"})

    output = repo.file_obliterate(path="secret.txt", dry_run=True)

    assert "Would obliterate file" in output, output
    assert "Store match : 0" not in repo.file_dump(path="secret.txt"), (
        "a dry run removed the content from the local store"
    )
    clone = scratch_dir("obliterate-dry-run-clone")
    repo.run(["repository", "clone", repo.remote_path, str(clone)])
    assert (clone / "secret.txt").read_bytes() == b"secret content\n", (
        "a dry run removed the content from the remote"
    )

    repo.file_obliterate(path="secret.txt")

    assert "Store match : 0" in repo.file_dump(path="secret.txt")
