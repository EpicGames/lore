# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import time

import pytest
from error_types import BranchBehindLatest
from lore_parsers import parse_jsonl, parse_status_json, parse_status_summary_json
from test_utils import to_posix

from lore import Lore, verify_signatures

logger = logging.getLogger(__name__)

# Top directories of generated files in `test_restore`, each holding 100 files in
# ten subdirectories, one of each ten 500 KB. Ten made 1,000 files and 50 MB, a
# large share of one of the slowest tests in the suite; three still restore many
# directories and files in both directions.
GENERATED_TOP_DIRS = 3


@pytest.mark.smoke
def test_restore(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"
    binary_file = "binary-File.bin"

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    with repo.open_file(binary_file, "w+b") as output_file:
        output_file.write(os.urandom(4096))

    # Stage the files
    repo.stage(scan=True)

    # Commit the files
    repo.commit("Initial snapshot - add text & binary files")

    # Modify a file
    with repo.open_file(binary_file, "w+b") as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Snapshot 2 - modify binary file")

    # Modify a file
    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(
            ["One line\n", "Another line\n", "Third line\n", "Fourth line\n"]
        )

    repo.stage(scan=True)
    repo.commit("Snapshot 3 - modify text file")
    repo.push()

    repo.remove_file(text_file)

    repo.stage(scan=True)
    repo.commit("Snapshot 4 - delete text file")
    repo.push()

    clone = repo.clone()

    # Modify file
    cloned_binary_file = "binary-File.bin"
    with clone.open_file(cloned_binary_file, "w+b") as output_file:
        output_file.write(os.urandom(1100))

    # Stage the files
    clone.stage(scan=True)
    clone.commit("Snapshot 5 - modify clone binary file")
    clone.push()

    # List all revisions
    output = repo.revision_history(remote=True)

    verify_signatures(output, 5)

    # Sync source repo back to revision 2
    repo.sync(output[1].signature)

    # Try to restore to head in the source repo because not at the latest revision
    with pytest.raises(BranchBehindLatest):
        repo.revision_restore("Restored revision 2 to latest")

    # Sync source repo to local head
    # Because we are behind local head, running 'sync' without any arguments will sync to local head.
    repo.sync()

    # Sync source repo to remote head
    # Because we are on local head, running 'sync' without any arguments will sync to remote head.
    repo.sync()

    # Sync source repo back to revision 2 out of 5
    repo.sync(output[1].signature)

    # Preview list of files affected during restore
    repo.revision_restore(dry_run=True)

    # Restore revision 2
    repo.revision_restore("Restored revision 2 to latest")

    # List all revisions
    output = repo.revision_history()

    verify_signatures(output, 6)

    # Describe current revision with delta's
    repo.revision_info(output[0].signature, delta=True)

    # Show status
    repo.repository_status(unstaged=True)

    # Sync source repo back to revision 2 again
    repo.sync(output[1].signature)

    # Modify a file
    with repo.open_file(binary_file, "w+b") as output_file:
        output_file.write(os.urandom(1200))

    # Restore revision 2 to latest again whilst having pending changes
    repo.revision_restore("Restored revision 2 to latest again")

    # Show status and reset
    repo.repository_status(unstaged=True)

    repo.file_reset()

    # Generate a large number of files and verify restore works
    output = repo.revision_info()
    restore_base_revision = output.signature

    for i in range(GENERATED_TOP_DIRS):
        subpath = str(i)
        repo.make_dirs(subpath)
        for j in range(10):
            subsubpath = os.path.join(subpath, str(j))
            repo.make_dirs(subsubpath)
            for k in range(10):
                with repo.open_file(
                    os.path.join(subsubpath, str(k) + ".uasset"), "w+b"
                ) as output_file:
                    if k == 0:
                        output_file.write(os.urandom(500000))
                    else:
                        output_file.write(os.urandom(10))

    repo.stage(scan=True)
    repo.commit("Generated files")
    repo.push()

    output = repo.revision_info()
    restore_back_revision = output.signature

    repo.sync(restore_base_revision)
    repo.revision_restore("Restore base revision without generated files")

    repo.sync(restore_back_revision)
    repo.revision_restore("Restore revision with all generated files")


@pytest.mark.smoke
def test_restore_records_no_time_a_later_write_could_share(new_lore_repo):
    """A file the restore reads whose time is not older than the filesystem clock the restore
    read first has no time recorded: a write after the read may share it. The scan after the
    restore reads the file instead of answering for it by that time.
    """
    repo: Lore = new_lore_repo()
    name = "restored.bin"
    with repo.open_file(name, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    repo.stage(scan=True)
    repo.commit("First content")
    repo.push()
    first = repo.revision_info().signature

    # The same size, so only the recorded time or the content can tell the files apart.
    with repo.open_file(name, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    repo.stage(scan=True)
    repo.commit("Second content")
    repo.push()

    repo.sync(first)
    ahead = time.time() + 3600
    os.utime(os.path.join(repo.path, name), (ahead, ahead))

    restored = parse_jsonl(
        repo.revision_restore("Restore the first content", json=True),
        "revisionRestoreFile",
    )
    assert [to_posix(entry["path"]) for entry in restored] == [name], (
        f"the restore must read {name} from the working tree, got {restored}"
    )

    output = repo.status(scan=True, json=True)
    assert [
        entry for entry in parse_status_json(output) if to_posix(entry["path"]) == name
    ] == [], "the restored content is not a change"
    summary = parse_status_summary_json(output)
    assert summary is not None, "scan must emit a repositoryStatusSummary event"
    assert (summary["hashChecks"], summary["mtimeMatches"]) == (1, 0), (
        f"the restore recorded a time a later write could share, got {summary}"
    )


@pytest.mark.smoke
def test_restore_records_the_deletion_of_a_file_the_latest_added(new_lore_repo):
    """Restoring the revision before the latest, which added a file, deletes the file, and the
    restored revision's delta records the deletion."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("Base", {"base.txt": "base\n"})
    first = repo.revision_info().signature
    repo.write_commit_push("Add a file", {"added.txt": "added\n"})

    repo.sync(first)
    repo.revision_restore("Restore the base")

    deltas = parse_jsonl(
        repo.revision_info(delta=True, json=True, offline=True), "revisionInfoDelta"
    )
    assert [(delta["action"], delta["path"]) for delta in deltas] == [
        ("delete", "added.txt")
    ], deltas
