# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os

import pytest

from lore import Lore

# Nodes are allocated 512 to a block, and a NodeID encodes its block index. The
# linked repository is filled past that so its nodes sit in blocks the parent
# state does not have: using one of its ids against the parent state is then a
# hard "Invalid block index" rather than a silent hit on a colliding node.
FILLER_FILE_COUNT = 1100


@pytest.mark.regression
def test_scan_stage_into_link_mount(new_lore_repo):
    """`stage --scan` of a path inside a link mount stages it in the linked
    repository, and leaves the parent repository usable.

    `--scan` used to resolve such a path against the parent state alone. A
    NodeID only means anything in the state that owns it, so the add either hit
    a block index the parent state does not have ("Invalid block index: N", the
    shape this test forces) or silently landed on a colliding node in the
    parent - and either way it did so after mutating state, which left `status`
    unable to run and the link registry destroyed on a store that
    `repository verify` still reported as healthy.
    """
    repo: Lore = new_lore_repo()
    repo.write_commit_push(None, {"main.txt": b"main repository\n"})

    link_repo: Lore = new_lore_repo()
    filler = {
        f"filler/f{index:05}.txt": f"filler {index}\n".encode()
        for index in range(FILLER_FILE_COUNT)
    }
    link_repo.write_files(filler)
    link_repo.write_files({"tracked/keep.txt": b"tracked in the linked repository\n"})
    link_repo.stage(scan=True)
    link_repo.commit("Linked repository content")
    link_repo.push()

    link_path = "mount"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    def assert_usable(stage_description: str) -> str:
        """A scan that completes - or refuses - must leave the repository
        readable. Corruption showed up here first: `status` stopped running and
        the mounts went missing while the store still verified clean."""
        status_output = repo.status()
        assert link_path in repo.link_list(), (
            f"link registry lost the mount after {stage_description}"
        )
        return status_output

    # A new file under a directory already tracked in the mount.
    nested_file = f"{link_path}/tracked/added.txt"
    with repo.open_file(nested_file, "w+") as output_file:
        output_file.writelines(["added under a tracked directory\n"])

    # `status --scan` is the entry that walks the filesystem against state;
    # `stage --scan` of a file path reconciles it directly.
    repo.status(nested_file, scan=True)
    assert_usable("status --scan of a new file in the mount")

    repo.stage(nested_file, scan=True)
    status_output = assert_usable("scanning a new file in the mount")
    assert "added.txt" in status_output, (
        f"new file in the mount should be staged, got:\n{status_output}"
    )

    # A new directory inside the mount, which has no tracked directory of its
    # own to name - the case with no workaround short of cloning the linked
    # repository and committing there.
    new_dir_file = f"{link_path}/added-dir/inside.txt"
    repo.make_dirs(os.path.dirname(new_dir_file))
    with repo.open_file(new_dir_file, "w+") as output_file:
        output_file.writelines(["added in a new directory\n"])

    repo.status(f"{link_path}/added-dir", scan=True)
    assert_usable("status --scan of a new directory in the mount")

    repo.stage(f"{link_path}/added-dir", scan=True)
    status_output = assert_usable("scanning a new directory in the mount")
    assert "inside.txt" in status_output, (
        f"new directory in the mount should be staged, got:\n{status_output}"
    )

    # The path that always worked - a directory already tracked inside the
    # mount - keeps working. Guards against a fix that over-refuses.
    tracked_file = f"{link_path}/tracked/keep.txt"
    with repo.open_file(tracked_file, "w+") as output_file:
        output_file.writelines(["modified in the linked repository\n"])

    repo.status(f"{link_path}/tracked", scan=True)
    assert_usable("status --scan of a tracked directory in the mount")

    repo.stage(f"{link_path}/tracked", scan=True)
    status_output = assert_usable("scanning a tracked directory in the mount")
    assert "keep.txt" in status_output, (
        f"modified file in the mount should be staged, got:\n{status_output}"
    )

    commit_output = repo.commit("Stage into the link mount by scan")
    assert "Commit succeeded" in commit_output, commit_output
    repo.push()
    assert link_path in repo.link_list()

    # The adds belong to the linked repository, not to the parent that scanned
    # them. A fresh clone of the linked repository is where that shows.
    linked_clone = link_repo.clone()
    assert linked_clone.file_exists("tracked/added.txt"), (
        "file scanned into the mount should be committed to the linked repository"
    )
    assert linked_clone.file_exists("added-dir/inside.txt"), (
        "directory scanned into the mount should be committed to the linked repository"
    )
