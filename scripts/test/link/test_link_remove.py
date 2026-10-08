# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os

import pytest
from error_types import LocalModificationsError
from link_helpers import DEFAULT_PARENT_FILE, make_parent_with_link

from lore import Lore


@pytest.mark.smoke
def test_link_remove_keeps_uncommitted_local_edit(new_lore_repo):
    link_path = "libs/shared"
    edited_file = f"{link_path}/deep/inner.txt"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({edited_file: "locally edited\n"})

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove(link_path)

    assert parent_repo.file_exists(edited_file)
    with parent_repo.open_file(edited_file) as f:
        assert f.read() == "locally edited\n"
    assert link_path in parent_repo.link_list(), (
        "A refused remove must leave the link in place"
    )


@pytest.mark.smoke
def test_link_remove_keeps_staged_local_edit(new_lore_repo):
    """A staged edit is still uncommitted, and unlinking would take it with it."""
    link_path = "libs/shared"
    edited_file = f"{link_path}/deep/inner.txt"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({edited_file: "locally edited\n"})
    parent_repo.stage(edited_file)

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove(link_path)

    with parent_repo.open_file(edited_file) as f:
        assert f.read() == "locally edited\n"


@pytest.mark.smoke
def test_link_remove_keeps_untracked_file_under_mount(new_lore_repo):
    """Unlinking deletes the mount wholesale, untracked files included."""
    link_path = "libs/shared"
    untracked_file = f"{link_path}/deep/scratch.txt"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({untracked_file: "not tracked yet\n"})

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove(link_path)

    assert parent_repo.file_exists(untracked_file)


@pytest.mark.smoke
def test_link_remove_refuses_when_path_case_differs(new_lore_repo):
    """Node lookup is case-insensitive, so the guard has to be too."""
    link_path = "libs/shared"
    edited_file = f"{link_path}/deep/inner.txt"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({edited_file: "locally edited\n"})

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove("libs/SHARED")

    with parent_repo.open_file(edited_file) as f:
        assert f.read() == "locally edited\n"


@pytest.mark.smoke
def test_link_remove_force_discards_local_edit(new_lore_repo):
    link_path = "libs/shared"
    edited_file = f"{link_path}/deep/inner.txt"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({edited_file: "locally edited\n"})

    parent_repo.link_remove(link_path, force=True)

    assert not parent_repo.file_exists(edited_file), (
        "A forced remove must discard the edited file with the mount"
    )
    assert "No links found in this repository" in parent_repo.link_list()


@pytest.mark.smoke
def test_link_remove_ignores_changes_outside_the_mount(new_lore_repo):
    """The guard scans the whole working tree, so it must filter to the mount."""
    link_path = "libs/shared"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files(
        {
            "README.txt": "edited outside the mount\n",
            "libs/shared-extra/sibling.txt": "a path sharing the mount prefix\n",
        }
    )

    parent_repo.link_remove(link_path)

    assert "No links found in this repository" in parent_repo.link_list()
    assert parent_repo.file_exists("libs/shared-extra/sibling.txt")


@pytest.mark.smoke
def test_link_remove_keeps_ignored_file_under_mount(new_lore_repo):
    """Ignoring a file says it is not Lore's to track, not that it is worthless
    - a key or a local config is exactly the sort of thing that gets ignored,
    and removal takes the whole mount with it.
    """
    link_path = "libs/shared"
    ignored_file = f"{link_path}/deep/local.key"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    with parent_repo.open_file(parent_repo.ignore_file(), "w+") as ignore_file:
        ignore_file.write("*.key\n")
    parent_repo.write_commit_push("Add ignore file", {"placeholder.txt": "x\n"})
    parent_repo.write_files({ignored_file: "secret\n"})

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove(link_path)

    assert parent_repo.file_exists(ignored_file), (
        "An ignored file must survive a refused remove"
    )


@pytest.mark.smoke
def test_link_remove_of_staged_add_leaves_an_empty_mount_directory(new_lore_repo):
    """A link removed before it was ever committed leaves its mount path behind, empty.

    The path held a directory the repository never committed, so removal empties
    it rather than reporting a deletion of committed content.
    """
    link_path = "libs/shared"
    parent_repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo()

    parent_repo.write_commit_push("Baseline", {DEFAULT_PARENT_FILE: "baseline\n"})
    link_repo.write_commit_push(
        "Initial linked content", {"deep/inner.txt": "linked content\n"}
    )

    parent_repo.link_add(link_path, link_repo.get_id(), "/")
    assert parent_repo.file_exists(f"{link_path}/deep/inner.txt"), (
        "Precondition: linked content is mounted"
    )

    parent_repo.link_remove(link_path)

    mount = os.path.join(parent_repo.path, link_path)
    assert os.path.isdir(mount), (
        "Removing a link staged for add must leave the mount path as a directory"
    )
    assert os.listdir(mount) == [], "The mount directory left behind must be empty"


@pytest.mark.smoke
def test_link_remove_of_committed_link_deletes_the_mount_directory(new_lore_repo):
    """Removing a committed link deletes its mount directory outright."""
    link_path = "libs/shared"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.link_remove(link_path)

    assert not parent_repo.file_exists(link_path), (
        "Removing a committed link must delete the mount directory"
    )
