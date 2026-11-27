# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from error_types import (
    ImproperArgumentsError,
    OverlappingLinkError,
    OverlappingMountError,
)
from link_helpers import (
    OVERLAP_LINK_FILES,
    add_a_link,
    assert_switch_realized,
    error_line,
    layer_targets,
    make_repo,
    parent_and_link_for_overlap,
    parse_revision_diff,
    remote_branch_names,
)
from test_utils import (
    links_by_path,
    read_text,
    source_paths_by_link_path,
    status_by_path,
    working_tree_files,
)

from lore import Lore

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)


@pytest.mark.smoke
def test_link_add_rejects_source_path_inside_existing_mount(new_lore_repo):
    """`sub/test` lives inside `sub`, so both mounts would materialize
    `sub/test/inner.txt` on disk, each under its own pin.

    The argument is spelled in a different case from the stored path, which
    resolves to the same node, so the comparison has to run on the stored path.
    """
    parent, link_repo = parent_and_link_for_overlap(new_lore_repo)

    parent.link_add("vendor/whole", link_repo.get_id(), "sub")
    parent.commit("Add link at vendor/whole")
    parent.push()

    with pytest.raises(OverlappingLinkError) as failure:
        parent.link_add("vendor/part", link_repo.get_id(), "SUB/test")

    error = error_line(failure)
    for path in ("vendor/part", "sub/test", "vendor/whole"):
        assert path in error, error

    assert source_paths_by_link_path(parent) == {"vendor/whole": "sub"}, (
        "A refused link add must leave the registry holding only the first mount"
    )


@pytest.mark.smoke
def test_link_add_rejects_source_path_containing_existing_mount(new_lore_repo):
    """The check is symmetric: mounting `sub` when `sub/test` is already mounted
    from the same repository is refused too.
    """
    parent, link_repo = parent_and_link_for_overlap(new_lore_repo)

    parent.link_add("vendor/part", link_repo.get_id(), "sub/test")
    parent.commit("Add link at vendor/part")
    parent.push()

    with pytest.raises(OverlappingLinkError):
        parent.link_add("vendor/whole", link_repo.get_id(), "sub")

    assert source_paths_by_link_path(parent) == {"vendor/part": "sub/test"}, (
        "A refused link add must leave the registry holding only the first mount"
    )


@pytest.mark.smoke
def test_link_add_rejects_nesting_under_a_root_mount(new_lore_repo):
    """A root mount exposes every subtree, so any other source path nests under
    it. The root is the empty stored path, which no prefix comparison matches.
    """
    parent, link_repo = parent_and_link_for_overlap(new_lore_repo)

    parent.link_add("vendor/all", link_repo.get_id(), "/")
    parent.commit("Add root mount")
    parent.push()

    with pytest.raises(OverlappingLinkError):
        parent.link_add("vendor/part", link_repo.get_id(), "sub/test")

    assert source_paths_by_link_path(parent) == {"vendor/all": "/"}, (
        "A refused link add must leave the registry holding only the root mount"
    )


@pytest.mark.smoke
def test_link_add_allows_disjoint_source_paths_and_refuses_one_folder_twice(
    new_lore_repo,
):
    """Disjoint subtrees of one repository mount side by side, and a second repository shares no
    pin with the first however its source paths line up. A second mount of the same subtree is
    refused, as the working tree would hold its files twice."""
    parent, first_link = parent_and_link_for_overlap(new_lore_repo)
    second_link: Lore = new_lore_repo()
    second_link.write_commit_push("Initial second link", OVERLAP_LINK_FILES)

    parent.link_add("vendor/test", first_link.get_id(), "sub/test")
    parent.link_add("vendor/other", first_link.get_id(), "sub/other")
    with pytest.raises(OverlappingLinkError):
        parent.link_add("vendor/test-again", first_link.get_id(), "sub/test")
    parent.link_add("vendor/second", second_link.get_id(), "sub")
    parent.commit("Add non-nesting mounts")
    parent.push()

    assert source_paths_by_link_path(parent) == {
        "vendor/test": "sub/test",
        "vendor/other": "sub/other",
        "vendor/second": "sub",
    }

    assert (
        read_text(parent, "vendor/test/inner.txt")
        == OVERLAP_LINK_FILES["sub/test/inner.txt"]
    )
    assert (
        read_text(parent, "vendor/other/sibling.txt")
        == OVERLAP_LINK_FILES["sub/other/sibling.txt"]
    )
    assert (
        read_text(parent, "vendor/second/test/inner.txt")
        == OVERLAP_LINK_FILES["sub/test/inner.txt"]
    )


@pytest.mark.smoke
def test_link_mounts_of_one_repository_must_agree_on_branch_and_flags(new_lore_repo):
    """Every mount of one repository shares its pin, so a mount of another folder has to track
    the same branch with the same flags."""
    parent, link_repo = parent_and_link_for_overlap(new_lore_repo)
    parent.link_add("vendor/test", link_repo.get_id(), "sub/test")

    with pytest.raises(ImproperArgumentsError):
        parent.link_add(
            "vendor/other", link_repo.get_id(), "sub/other", disable_branching=True
        )
    parent.link_add("vendor/other", link_repo.get_id(), "sub/other")
    parent.commit("Add links")
    parent.push()

    assert (
        read_text(parent, "vendor/test/inner.txt")
        == OVERLAP_LINK_FILES["sub/test/inner.txt"]
    )
    assert (
        read_text(parent, "vendor/other/sibling.txt")
        == OVERLAP_LINK_FILES["sub/other/sibling.txt"]
    )


@pytest.mark.smoke
def test_link_mount_added_without_flags_takes_the_flags_of_its_repository(
    new_lore_repo,
):
    """A mount of a repository mounted without following branches, added without
    `--disable-branching`, takes the flags of the other mounts."""
    parent, link_repo = parent_and_link_for_overlap(new_lore_repo)
    parent.link_add(
        "vendor/test", link_repo.get_id(), "sub/test", disable_branching=True
    )
    parent.link_add("vendor/other", link_repo.get_id(), "sub/other")

    assert links_by_path(parent, "flags") == {"vendor/test": 1, "vendor/other": 1}


@pytest.mark.smoke
@pytest.mark.parametrize("replaced", [False, True])
@pytest.mark.parametrize("part", ["sub/test", "sub"])
def test_link_reset_rejects_restoring_an_overlapping_mount(
    new_lore_repo, replaced, part
):
    """Staging a removal hides a mount from the registry, so a nesting mount, or one of the same
    folder, can be added while it is gone. Resetting the removal must not restore the overlap,
    and leaves a link to another repository added in its place as it was.
    """
    parent, link_repo = parent_and_link_for_overlap(new_lore_repo)

    parent.link_add("vendor/whole", link_repo.get_id(), "sub")
    parent.commit("Add link at vendor/whole")
    parent.push()

    parent.link_remove("vendor/whole")
    expected = {"vendor/part": part}
    if replaced:
        other_repo: Lore = new_lore_repo()
        other_repo.write_commit_push("Initial other", {"other.txt": "other\n"})
        parent.link_add("vendor/whole", other_repo.get_id(), "/")
        expected["vendor/whole"] = "/"
    parent.link_add("vendor/part", link_repo.get_id(), part)
    files = parent.read_files()

    with pytest.raises(OverlappingLinkError):
        parent.reset("vendor/whole")

    assert source_paths_by_link_path(parent) == expected, (
        "A refused reset must leave the staged registry as it was"
    )
    assert parent.read_files() == files


@pytest.mark.smoke
def test_link_merge_rejects_incoming_overlapping_mount(new_lore_repo):
    """Both branches fork from a revision holding neither mount, so neither
    `link add` can see the other. The merge that brings them together is refused.
    """
    parent, link_repo = parent_and_link_for_overlap(new_lore_repo)

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

    assert source_paths_by_link_path(parent) == {"vendor/whole": "sub"}, (
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


@pytest.mark.smoke
def test_link_branch_switch_realizes_each_mount_against_its_own_source(new_lore_repo):
    """Two branches mount one repository at different, non-nesting source paths.

    Both mounts carry the linked repository's id, which is the identity a link
    node holds, so neither the working tree nor a revision diff may take the
    pair for one mount moving between paths. The configuration is the one
    `test_link_add_allows_non_nesting_source_paths` leaves deliberately legal.
    """
    parent, link_repo = parent_and_link_for_overlap(new_lore_repo)

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
    assert_switch_realized(parent, _WHOLE_MOUNT_FILES, {"vendor/whole": "sub"})

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
    assert_switch_realized(parent, _PART_MOUNT_FILES, {"vendor/part": "sub/test"})


_LIB_FILES = ["vendor/lib/lib.txt"]


@pytest.fixture
def parent_with_link_and_layer_repo(new_lore_repo):
    parent = make_repo(
        new_lore_repo, {"parent.txt": "parent content\n"}, "Initial parent"
    )

    link_repo = make_repo(new_lore_repo, {"lib.txt": "lib content\n"}, "Initial link")

    layer_repo: Lore = new_lore_repo(parent.name + "_layer")
    layer_repo.write_commit_push("Initial layer", {"layer.txt": "layer content\n"})

    add_a_link(parent, link_repo, link_path="vendor/lib")
    return parent, link_repo, layer_repo


@pytest.fixture
def parent_with_layer_and_link_repo(new_lore_repo):
    parent = make_repo(
        new_lore_repo, {"parent.txt": "parent content\n"}, "Initial parent"
    )

    layer_repo: Lore = new_lore_repo(parent.name + "_layer")
    layer_repo.write_commit_push("Initial layer", {"layer.txt": "layer content\n"})
    # A second repository, since a branch create in a parent with one layer
    # repository mounted twice refuses to create the branch in it twice.
    maps_layer_repo: Lore = new_lore_repo(parent.name + "_maps_layer")
    maps_layer_repo.write_commit_push("Initial maps layer", {"layer.txt": "maps\n"})

    link_repo = make_repo(new_lore_repo, {"lib.txt": "lib content\n"}, "Initial link")

    parent.layer_add("assets", layer_repo, "/")
    parent.layer_add("art/maps", maps_layer_repo, "/")
    return parent, layer_repo, link_repo


@pytest.mark.smoke
def test_layer_add_refuses_a_target_overlapping_a_link(parent_with_link_and_layer_repo):
    parent, _, layer_repo = parent_with_link_and_layer_repo
    parent.branch_create("feature")

    for target, dry_run in [
        ("vendor/lib/x", False),
        ("vendor/lib", False),
        ("vendor", False),
        ("vendor/lib", True),
    ]:
        with pytest.raises(OverlappingMountError):
            parent.layer_add(target, layer_repo, "/", dry_run=dry_run)

    assert layer_targets(parent) == set(), "A refused layer add must add no layer"
    assert working_tree_files(parent, "vendor") == _LIB_FILES, (
        "A refused layer add must leave the link's files as they were"
    )
    assert status_by_path(parent) == {}, "A refused layer add must leave status clean"
    assert "feature" not in remote_branch_names(layer_repo), (
        "A refused layer add must create no branch in the layer repository"
    )

    parent.layer_add("vendor/other", layer_repo, "/")

    assert layer_targets(parent) == {"vendor/other"}
    assert working_tree_files(parent, "vendor") == _LIB_FILES + [
        "vendor/other/layer.txt"
    ]
    assert "feature" in remote_branch_names(layer_repo), (
        "A layer add beside the link must create the branch the refusals left out"
    )


@pytest.mark.smoke
def test_layer_add_refuses_a_target_overlapping_a_removed_link(
    parent_with_link_and_layer_repo,
):
    parent, _, layer_repo = parent_with_link_and_layer_repo
    parent.link_remove("vendor/lib")

    for target in ["vendor/lib", "vendor/lib/x", "vendor"]:
        with pytest.raises(OverlappingMountError):
            parent.layer_add(target, layer_repo, "/")

    assert layer_targets(parent) == set(), (
        "A layer add over a link staged for removal must add no layer"
    )


@pytest.mark.smoke
def test_link_add_refuses_a_path_overlapping_a_layer(parent_with_layer_and_link_repo):
    parent, _, link_repo = parent_with_layer_and_link_repo
    parent.branch_create("feature")

    for link_path, dry_run in [
        ("assets/lib", False),
        ("assets", False),
        ("art", False),
        ("assets/lib", True),
    ]:
        with pytest.raises(OverlappingMountError):
            parent.link_add(link_path, link_repo.get_id(), "/", dry_run=dry_run)

    assert source_paths_by_link_path(parent) == {}, (
        "A refused link add must add no link"
    )
    assert working_tree_files(parent, "assets") == ["assets/layer.txt"], (
        "A refused link add must leave the layer's files as they were"
    )
    assert working_tree_files(parent, "art") == ["art/maps/layer.txt"], (
        "A refused link add must leave the layer's files as they were"
    )
    assert status_by_path(parent) == {}, "A refused link add must stage nothing"
    assert "feature" not in remote_branch_names(link_repo), (
        "A refused link add must create no branch in the linked repository"
    )

    parent.link_add("assets2/lib", link_repo.get_id(), "/")

    assert source_paths_by_link_path(parent) == {"assets2/lib": "/"}
    assert working_tree_files(parent, "assets2") == ["assets2/lib/lib.txt"]
    assert "feature" in remote_branch_names(link_repo), (
        "A link add beside the layer must create the branch the refusals left out"
    )
