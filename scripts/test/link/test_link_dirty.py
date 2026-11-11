# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from error_types import NotSupportedError
from link_helpers import (
    DEFAULT_LINK_MOUNT,
    DEFAULT_PARENT_FILE,
    MAX_LINK_DEPTH,
    build_link_chain,
    committed_link_pin,
    link_pin,
    make_parent_with_link,
    make_repo,
)
from test_utils import status_by_path

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)


@pytest.mark.smoke
def test_link_dirty_inside_a_link_mounted_from_a_subfolder(new_lore_repo):
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_files={
            "mounted/linked.txt": "linked\n",
            "mounted/doomed.txt": "doomed\n",
            "mounted/other.txt": "other\n",
            "mounted/subdir/kept.txt": "kept\n",
            "outside.txt": "outside\n",
        },
        source_path="/mounted",
    )
    modified = f"{DEFAULT_LINK_MOUNT}/linked.txt"
    added = f"{DEFAULT_LINK_MOUNT}/added.txt"
    deleted = f"{DEFAULT_LINK_MOUNT}/doomed.txt"
    recursed = f"{DEFAULT_LINK_MOUNT}/other.txt"
    nested = f"{DEFAULT_LINK_MOUNT}/subdir/nested.txt"
    new_dir = f"{DEFAULT_LINK_MOUNT}/fresh"
    in_new_dir = f"{new_dir}/nested.txt"
    added_paths = {added, nested, new_dir, in_new_dir}

    with pytest.raises(NotSupportedError):
        parent_repo.dirty_move(modified, added)
    with pytest.raises(NotSupportedError):
        parent_repo.dirty_copy(DEFAULT_PARENT_FILE, added)
    with pytest.raises(NotSupportedError):
        parent_repo.dirty_copy(DEFAULT_PARENT_FILE, in_new_dir)

    parent_repo.write_files(
        {modified: "edited through the mount\n"}
        | dict.fromkeys([added, nested, in_new_dir], "added through the mount\n")
    )
    parent_repo.remove_file(deleted)
    parent_repo.dirty([modified, added, nested, in_new_dir, deleted])

    entries = status_by_path(parent_repo)
    assert entries.keys() == {modified, deleted} | added_paths, (
        f"Expected every dirtied path below its own parent without a scan, got: {entries}"
    )
    assert all(entry.get("flagDirty") is True for entry in entries.values())
    assert all(entries[path].get("action") == "add" for path in added_paths)
    assert entries[deleted].get("action") == "delete"

    parent_repo.write_files({recursed: "edited, then the mount dirtied\n"})
    parent_repo.dirty(DEFAULT_LINK_MOUNT)
    assert recursed in status_by_path(parent_repo), (
        "Expected dirtying the mount to recurse into the linked tree"
    )

    parent_repo.stage(DEFAULT_LINK_MOUNT)
    staged = status_by_path(parent_repo)
    assert all(
        staged.get(path, {}).get("flagStaged") is True
        for path in {modified, deleted, recursed} | added_paths
    ), f"Expected staging the mount to pick up the dirty files, got: {staged}"


@pytest.mark.smoke
def test_link_dirty_of_a_removed_file_a_directory_only_rule_names(new_lore_repo):
    """Marking a file below a mount, removed from the working tree, marks its
    removal where a directory-only rule such as `Intermediate/` names it: the path
    is asked about as the file the linked tree holds there.
    """
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_files={"src/Intermediate": "file\n"}
    )
    removed = f"{DEFAULT_LINK_MOUNT}/src/Intermediate"
    parent_repo.write_files({parent_repo.ignore_file(): "Intermediate/\n"})
    parent_repo.remove_file(removed)
    parent_repo.dirty(removed)

    entries = status_by_path(parent_repo)
    assert entries.get(removed, {}).get("action") == "delete", (
        f"Expected the removal of {removed} marked, got: {entries}"
    )


@pytest.mark.smoke
def test_link_dirty_inside_the_deepest_nested_link(new_lore_repo):
    parent_repo, deepest_mount = build_link_chain(new_lore_repo, MAX_LINK_DEPTH)
    edited = f"{deepest_mount}/a.txt"
    untouched = f"{deepest_mount}/b.txt"

    parent_repo.write_files(
        {
            edited: "edited through the nested mount\n",
            DEFAULT_PARENT_FILE: "parent edit\n",
        }
    )
    parent_repo.dirty([edited, untouched])
    assert status_by_path(parent_repo, check_dirty=True).keys() == {edited}
    assert status_by_path(parent_repo).keys() == {edited}, (
        "Expected the marker check-dirty cleared in the deepest link to stay cleared"
    )

    parent_repo.stage(DEFAULT_PARENT_FILE)
    parent_repo.commit("Parent edit only")
    parent_repo.push()
    entries = status_by_path(parent_repo)
    assert entries.keys() == {edited}, (
        f"Expected the marker in the deepest link carried across the commit, got: {entries}"
    )
    assert entries[edited].get("flagDirty") is True

    parent_repo.stage("l1")
    assert status_by_path(parent_repo)[edited].get("flagStaged") is True, (
        "Expected staging the outermost mount to reach the deepest link"
    )


@pytest.mark.smoke
def test_link_dirty_markers_survive_a_commit_and_a_branch_switch(new_lore_repo):
    parent_repo, link_repo = make_parent_with_link(
        new_lore_repo, link_files={"a.txt": "a\n", "b.txt": "b\n"}
    )
    committed_pin = link_pin(parent_repo, link_repo.get_id())
    linked_files = [f"{DEFAULT_LINK_MOUNT}/a.txt", f"{DEFAULT_LINK_MOUNT}/b.txt"]

    parent_repo.write_files(
        dict.fromkeys(linked_files, "edited through the mount\n")
        | {DEFAULT_PARENT_FILE: "parent edit\n"}
    )
    parent_repo.dirty(linked_files)
    parent_repo.stage(DEFAULT_PARENT_FILE)
    parent_repo.commit("Parent edit only")
    parent_repo.push()

    assert committed_link_pin(parent_repo, link_repo.get_id()) == committed_pin, (
        "A commit must leave the pin of a link holding only dirty markers at "
        "its committed revision"
    )
    assert sorted(status_by_path(parent_repo)) == linked_files, (
        "Expected the dirty markers inside the link carried across the commit"
    )

    parent_repo.branch_create("feature")
    parent_repo.branch_switch("main")
    entries = status_by_path(parent_repo)
    assert sorted(entries) == linked_files, (
        f"Expected the dirty markers carried across a branch switch, got: {entries}"
    )
    assert all(entry.get("flagDirty") is True for entry in entries.values())

    parent_repo.stage(DEFAULT_LINK_MOUNT.split("/")[0])
    staged = status_by_path(parent_repo)
    assert all(staged[path].get("flagStaged") is True for path in linked_files), (
        f"Expected staging the directory above the mount to stage them, got: {staged}"
    )


@pytest.mark.smoke
@pytest.mark.parametrize("staged_link_change", ["add", "pin update"])
def test_link_commit_of_a_staged_link_pins_its_staged_revision_past_dirty_markers(
    new_lore_repo, staged_link_change
):
    parent_repo = make_repo(new_lore_repo, {DEFAULT_PARENT_FILE: "baseline\n"})
    link_repo = make_repo(new_lore_repo, {"a.txt": "a\n"})
    linked_file = f"{DEFAULT_LINK_MOUNT}/a.txt"
    parent_repo.link_add(DEFAULT_LINK_MOUNT, link_repo.get_id(), "/")
    if staged_link_change == "pin update":
        parent_repo.commit("Add link")
        link_repo.write_commit_push("v2", {"b.txt": "b\n"})
        parent_repo.link_update(
            DEFAULT_LINK_MOUNT, pin=link_repo.branch_info().local_latest
        )
    staged_pin = link_repo.branch_info().local_latest

    parent_repo.write_files({linked_file: "edited through the mount\n"})
    parent_repo.dirty(linked_file)
    parent_repo.commit("Commit the staged link")
    parent_repo.push()

    assert committed_link_pin(parent_repo, link_repo.get_id()) == staged_pin
    entries = status_by_path(parent_repo)
    assert entries.keys() == {linked_file}, (
        f"Expected the unstaged linked edit to remain, got: {entries}"
    )
    assert entries[linked_file].get("flagDirty") is True
    assert entries[linked_file].get("flagStaged") is False


@pytest.mark.smoke
def test_link_check_dirty_persists_verified_markers_inside_a_link(new_lore_repo):
    parent_repo, link_repo = make_parent_with_link(
        new_lore_repo, link_files={"a.txt": "a\n", "b.txt": "b\n"}
    )
    committed_pin = link_pin(parent_repo, link_repo.get_id())
    edited = f"{DEFAULT_LINK_MOUNT}/a.txt"
    untouched = f"{DEFAULT_LINK_MOUNT}/b.txt"

    parent_repo.write_files({edited: "edited through the mount\n"})
    parent_repo.dirty([edited, untouched])

    assert status_by_path(parent_repo, check_dirty=True).keys() == {edited}
    assert status_by_path(parent_repo).keys() == {edited}, (
        "Expected the marker check-dirty cleared inside the link to stay cleared"
    )

    parent_repo.write_files({edited: "a\n"})
    parent_repo.status(json=True, check_dirty=True)

    assert link_pin(parent_repo, link_repo.get_id()) == committed_pin, (
        "Expected the link pin restored to the committed revision once no marker remains"
    )
    assert status_by_path(parent_repo) == {}


@pytest.mark.smoke
def test_link_switch_drops_the_dirty_marker_of_a_linked_file_it_restores(
    new_lore_repo,
):
    """A switch to a branch pinning the link elsewhere restores a linked file
    marked as deleted, and leaves no marker on it."""
    parent_repo, link_repo = make_parent_with_link(
        new_lore_repo, link_files={"a.txt": "a\n"}
    )
    linked_file = f"{DEFAULT_LINK_MOUNT}/a.txt"
    parent_repo.branch_create("feature")
    link_repo.write_commit_push("v2", {"a.txt": "a2\n"})
    parent_repo.link_update(
        DEFAULT_LINK_MOUNT, pin=link_repo.branch_info().local_latest
    )
    parent_repo.commit("Pin v2")
    parent_repo.push()
    parent_repo.branch_switch("main")

    parent_repo.remove_file(linked_file)
    parent_repo.dirty(linked_file)
    assert status_by_path(parent_repo).keys() == {linked_file}, (
        "setup: the deleted linked file must be marked"
    )

    parent_repo.branch_switch("feature")

    with parent_repo.open_file(linked_file, "r") as input_file:
        assert input_file.read() == "a2\n", "the switch must restore the linked file"
    assert status_by_path(parent_repo) == {}, (
        "the restored linked file must stay unmarked"
    )
