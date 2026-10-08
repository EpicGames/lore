# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from error_types import OverlappingLinkError
from link_helpers import parse_revision_diff
from test_utils import unstaged_entries, working_tree_files

from lore import Lore

_OVERLAP_LINK_FILES = {
    "sub/outer.txt": "outer content\n",
    "sub/test/inner.txt": "inner content\n",
    "sub/other/sibling.txt": "sibling content\n",
}


def _parent_and_link_for_overlap(new_lore_repo, name: str = ""):
    parent: Lore = new_lore_repo()
    parent.write_commit_push("Initial parent", {"parent.txt": "parent content\n"})

    link_repo: Lore = new_lore_repo(name)
    link_repo.write_commit_push("Initial link", _OVERLAP_LINK_FILES)

    return parent, link_repo


def _mounted_source_paths(repo: Lore) -> dict:
    """Maps every mount path in `repo` to the source path it exposes."""
    mounts = {}
    link_path = None
    for line in repo.link_list().splitlines():
        line = line.strip()
        if line.startswith("Link path:"):
            link_path = line.split(":", 1)[1].split("(")[0].strip()
        elif line.startswith("Source path:") and link_path is not None:
            mounts[link_path] = line.split(":", 1)[1].split("(")[0].strip()
            link_path = None
    return mounts


@pytest.mark.smoke
def test_link_add_rejects_source_path_inside_existing_mount(new_lore_repo):
    """`sub/test` lives inside `sub`, so both mounts would materialize
    `sub/test/inner.txt` on disk, each under its own pin.

    The argument is spelled in a different case from the stored path, which
    resolves to the same node, so the comparison has to run on the stored path.
    """
    parent, link_repo = _parent_and_link_for_overlap(new_lore_repo)

    parent.link_add("vendor/whole", link_repo.get_id(), "sub")
    parent.commit("Add link at vendor/whole")
    parent.push()

    with pytest.raises(OverlappingLinkError):
        parent.link_add("vendor/part", link_repo.get_id(), "SUB/test")

    assert _mounted_source_paths(parent) == {"vendor/whole": "sub"}, (
        "A refused link add must leave the registry holding only the first mount"
    )


@pytest.mark.smoke
def test_link_add_rejects_source_path_containing_existing_mount(new_lore_repo):
    """The check is symmetric: mounting `sub` when `sub/test` is already mounted
    from the same repository is refused too.
    """
    parent, link_repo = _parent_and_link_for_overlap(new_lore_repo)

    parent.link_add("vendor/part", link_repo.get_id(), "sub/test")
    parent.commit("Add link at vendor/part")
    parent.push()

    with pytest.raises(OverlappingLinkError):
        parent.link_add("vendor/whole", link_repo.get_id(), "sub")

    assert _mounted_source_paths(parent) == {"vendor/part": "sub/test"}, (
        "A refused link add must leave the registry holding only the first mount"
    )


@pytest.mark.smoke
def test_link_add_rejects_nesting_under_a_root_mount(new_lore_repo):
    """A root mount exposes every subtree, so any other source path nests under
    it. The root is the empty stored path, which no prefix comparison matches.
    """
    parent, link_repo = _parent_and_link_for_overlap(new_lore_repo)

    parent.link_add("vendor/all", link_repo.get_id(), "/")
    parent.commit("Add root mount")
    parent.push()

    with pytest.raises(OverlappingLinkError):
        parent.link_add("vendor/part", link_repo.get_id(), "sub/test")

    assert _mounted_source_paths(parent) == {"vendor/all": "/"}, (
        "A refused link add must leave the registry holding only the root mount"
    )


@pytest.mark.smoke
def test_link_add_allows_non_nesting_source_paths(new_lore_repo):
    """Only strictly nesting subtrees of one repository are refused.

    Disjoint siblings cannot shadow each other, identical subtrees resolve to one
    shared linked state and advance together, and a second repository shares no
    pin with the first however its source paths line up.
    """
    parent, first_link = _parent_and_link_for_overlap(new_lore_repo)
    second_link: Lore = new_lore_repo()
    second_link.write_commit_push("Initial second link", _OVERLAP_LINK_FILES)

    parent.link_add("vendor/test", first_link.get_id(), "sub/test")
    parent.link_add("vendor/other", first_link.get_id(), "sub/other")
    parent.link_add("vendor/test-again", first_link.get_id(), "sub/test")
    parent.link_add("vendor/second", second_link.get_id(), "sub")
    parent.commit("Add non-nesting mounts")
    parent.push()

    assert _mounted_source_paths(parent) == {
        "vendor/test": "sub/test",
        "vendor/other": "sub/other",
        "vendor/test-again": "sub/test",
        "vendor/second": "sub",
    }

    for mount in ("vendor/test", "vendor/test-again"):
        with parent.open_file(f"{mount}/inner.txt") as f:
            assert f.read() == _OVERLAP_LINK_FILES["sub/test/inner.txt"]
    with parent.open_file("vendor/other/sibling.txt") as f:
        assert f.read() == _OVERLAP_LINK_FILES["sub/other/sibling.txt"]
    with parent.open_file("vendor/second/test/inner.txt") as f:
        assert f.read() == _OVERLAP_LINK_FILES["sub/test/inner.txt"]


@pytest.mark.smoke
def test_link_reset_rejects_restoring_an_overlapping_mount(new_lore_repo):
    """Staging a removal hides a mount from the registry, so a nesting mount can
    be added while it is gone. Resetting the removal must not restore the overlap.
    """
    parent, link_repo = _parent_and_link_for_overlap(new_lore_repo)

    parent.link_add("vendor/whole", link_repo.get_id(), "sub")
    parent.commit("Add link at vendor/whole")
    parent.push()

    parent.link_remove("vendor/whole")
    parent.link_add("vendor/part", link_repo.get_id(), "sub/test")

    with pytest.raises(OverlappingLinkError):
        parent.reset("vendor/whole")

    assert _mounted_source_paths(parent) == {"vendor/part": "sub/test"}, (
        "A refused reset must leave the staged registry as it was"
    )


@pytest.mark.smoke
def test_link_merge_rejects_incoming_overlapping_mount(new_lore_repo):
    """Both branches fork from a revision holding neither mount, so neither
    `link add` can see the other. The merge that brings them together is refused.
    """
    parent, link_repo = _parent_and_link_for_overlap(new_lore_repo)

    parent.branch_create("mount-whole")
    parent.link_add("vendor/whole", link_repo.get_id(), "sub")
    parent.commit("Add link at vendor/whole")
    parent.push()

    parent.branch_switch("main")
    parent.branch_create("mount-part")
    parent.link_add("vendor/part", link_repo.get_id(), "sub/test")
    parent.commit("Add link at vendor/part")
    parent.push()

    parent.branch_switch("mount-whole")

    with pytest.raises(OverlappingLinkError):
        parent.branch_merge_start("mount-part", message="Merge nesting mount")

    assert _mounted_source_paths(parent) == {"vendor/whole": "sub"}, (
        "A refused merge must leave the target's registry as it was"
    )
    assert not parent.path_exists("vendor/part"), (
        "A refused merge must be rejected before the incoming mount is cloned, "
        "so no untracked content is left behind"
    )
    assert parent.branch_info().local_latest == parent.branch_info().remote_latest, (
        "A refused merge must not have committed anything"
    )


_WHOLE_MOUNT_FILES = [
    "vendor/whole/other/sibling.txt",
    "vendor/whole/outer.txt",
    "vendor/whole/test/inner.txt",
]


_PART_MOUNT_FILES = ["vendor/part/inner.txt"]


def _assert_switch_realized(repo: Lore, expected_files: list[str], source: dict):
    unstaged = unstaged_entries(repo)
    assert unstaged == [], f"A clean switch must leave nothing unstaged, got {unstaged}"
    assert _mounted_source_paths(repo) == source, (
        "The switch must restore the registry the branch committed"
    )
    assert working_tree_files(repo, "vendor") == expected_files, (
        "The switch must realize the mount against the source path its own branch names"
    )


@pytest.mark.smoke
def test_link_branch_switch_realizes_each_mount_against_its_own_source(new_lore_repo):
    """Two branches mount one repository at different, non-nesting source paths.

    Both mounts carry the linked repository's id, which is the identity a link
    node holds, so neither the working tree nor a revision diff may take the
    pair for one mount moving between paths. The configuration is the one
    `test_link_add_allows_non_nesting_source_paths` leaves deliberately legal.
    """
    parent, link_repo = _parent_and_link_for_overlap(new_lore_repo)

    parent.branch_create("mount-whole")
    parent.link_add("vendor/whole", link_repo.get_id(), "sub")
    parent.commit("Add link at vendor/whole")
    parent.push()
    assert working_tree_files(parent, "vendor") == _WHOLE_MOUNT_FILES, (
        "The mount of `sub` must expose that subtree in full"
    )

    parent.branch_switch("main")
    assert working_tree_files(parent, "vendor") == [], (
        "main holds neither mount, so it realizes no content under vendor/"
    )

    parent.branch_create("mount-part")
    parent.link_add("vendor/part", link_repo.get_id(), "sub/test")
    parent.commit("Add link at vendor/part")
    parent.push()
    assert working_tree_files(parent, "vendor") == _PART_MOUNT_FILES, (
        "The mount of `sub/test` must expose only that subtree"
    )
    part_revision = parent.branch_info().local_latest

    parent.branch_switch("mount-whole")
    _assert_switch_realized(parent, _WHOLE_MOUNT_FILES, {"vendor/whole": "sub"})

    mount_actions = {
        path.rstrip("/"): action
        for action, path in parse_revision_diff(
            parent.revision_diff(part_revision, no_pager=True)
        )
        if path.rstrip("/") in ("vendor/whole", "vendor/part")
    }
    assert mount_actions == {"vendor/whole": "A", "vendor/part": "D"}, (
        "Each mount must be reported on its own, the one this branch holds as an "
        "add and the one it does not as a delete"
    )

    parent.branch_switch("mount-part")
    _assert_switch_realized(parent, _PART_MOUNT_FILES, {"vendor/part": "sub/test"})
