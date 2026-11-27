# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""One repository linked at several paths, each mount exposing another folder of it: every mount
pins the same revision."""

import pytest
from error_types import ImproperArgumentsError, LocalChanges
from link_helpers import (
    MAPPED_BAR,
    MAPPED_BAZ,
    MAPPED_FOO,
    MAPPED_LINKED_FILES,
    assert_link_holds,
    make_mapped_parent,
    make_repo,
)
from test_utils import (
    pins_by_link_path,
    read_text,
    status_by_path,
    unstaged_entries,
    working_tree_files,
)


pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)


@pytest.mark.smoke
def test_mapped_link_added_later_joins_the_pin_of_its_mounts(new_lore_repo):
    """A third mount of a repository that moved on since pins the revision its other mounts
    pin, refuses another, and `link list` shows the repository once with all three mounts."""
    parent, linked = make_mapped_parent(new_lore_repo)
    pinned = pins_by_link_path(parent)[MAPPED_FOO]
    linked.write_commit_push("Linked advances", {"foo/f2.txt": "foo 2\n"})

    with pytest.raises(ImproperArgumentsError):
        parent.link_add(
            MAPPED_BAZ, linked.get_id(), "/baz", pin=linked.branch_info().local_latest
        )
    parent.link_add(MAPPED_BAZ, linked.get_id(), "/baz")

    assert pins_by_link_path(parent) == {
        MAPPED_FOO: pinned,
        MAPPED_BAR: pinned,
        MAPPED_BAZ: pinned,
    }
    parent.commit("Mount the linked repository a third time")
    parent.push()
    listing = parent.link_list()
    assert listing.count(f"Link {linked.get_id()}") == 1, listing
    for mount, source in (
        (MAPPED_FOO, "foo"),
        (MAPPED_BAR, "bar"),
        (MAPPED_BAZ, "baz"),
    ):
        assert f"{mount} <- {source}" in listing, listing
    clone = parent.clone()
    assert pins_by_link_path(clone) == {
        MAPPED_FOO: pinned,
        MAPPED_BAR: pinned,
        MAPPED_BAZ: pinned,
    }
    assert read_text(clone, f"{MAPPED_BAZ}/z1.txt") == "baz 1\n"


@pytest.mark.smoke
def test_mapped_link_commit_through_one_mount_moves_every_mount(new_lore_repo):
    """On a branch created over the mounts, a commit of one mount alone pins the revision it
    commits at every mount; the pushed revision clones and verifies with every mount, and main
    keeps its pin."""
    parent, linked = make_mapped_parent(new_lore_repo)
    parent.link_add(MAPPED_BAZ, linked.get_id(), "/baz")
    parent.commit("Mount baz")
    parent.push()
    pinned = pins_by_link_path(parent)[MAPPED_FOO]

    parent.branch_create("feature")
    branched = pins_by_link_path(parent)[MAPPED_FOO]
    assert pins_by_link_path(parent) == {
        MAPPED_FOO: branched,
        MAPPED_BAR: branched,
        MAPPED_BAZ: branched,
    }
    parent.write_files({f"{MAPPED_FOO}/f1.txt": "edited through a\n"})
    parent.stage([f"{MAPPED_FOO}/f1.txt"])
    parent.commit("Edit through one mount alone", link=MAPPED_FOO)
    parent.commit("Pin every mount")
    parent.push()

    committed = pins_by_link_path(parent)[MAPPED_FOO]
    mounts = {MAPPED_FOO: committed, MAPPED_BAR: committed, MAPPED_BAZ: committed}
    assert pins_by_link_path(parent) == mounts
    assert committed not in (pinned, branched)
    parent.repository_verify()
    clone = parent.clone(branch="feature")
    assert pins_by_link_path(clone) == mounts
    assert_link_holds(clone, MAPPED_FOO, {"f1.txt": "edited through a\n"})
    assert_link_holds(clone, MAPPED_BAR, {"b1.txt": "bar 1\n"})
    parent.branch_switch("main")
    assert pins_by_link_path(parent) == {
        MAPPED_FOO: pinned,
        MAPPED_BAR: pinned,
        MAPPED_BAZ: pinned,
    }


@pytest.mark.smoke
def test_mapped_link_update_moves_and_unstage_restores_every_mount(new_lore_repo):
    """Updating one mount, refused by a local change at another, leaves every mount as it was;
    once the change is gone it moves every mount of the repository, unstaging it, or every mount
    at once, puts every mount back, and the update committed again clones with both mounts at the
    new revision."""
    parent, linked = make_mapped_parent(new_lore_repo)
    pinned = pins_by_link_path(parent)[MAPPED_FOO]
    linked.write_commit_push(
        "Linked advances", {"foo/f2.txt": "foo 2\n", "bar/b1.txt": "bar 2\n"}
    )
    newest = linked.branch_info().local_latest
    advanced = {"f1.txt": "foo 1\n", "f2.txt": "foo 2\n"}
    parent.write_files({f"{MAPPED_BAR}/b1.txt": "edited through b\n"})

    with pytest.raises(LocalChanges):
        parent.link_update(MAPPED_FOO)
    assert pins_by_link_path(parent) == {MAPPED_FOO: pinned, MAPPED_BAR: pinned}
    assert working_tree_files(parent, MAPPED_FOO) == [f"{MAPPED_FOO}/f1.txt"]
    parent.reset(f"{MAPPED_BAR}/b1.txt")

    for unstaged in (MAPPED_FOO, [MAPPED_FOO, MAPPED_BAR]):
        parent.link_update(MAPPED_FOO)
        assert pins_by_link_path(parent) == {MAPPED_FOO: newest, MAPPED_BAR: newest}
        assert_link_holds(parent, MAPPED_FOO, advanced)
        parent.unstage(unstaged)
        assert pins_by_link_path(parent) == {MAPPED_FOO: pinned, MAPPED_BAR: pinned}
        assert_link_holds(parent, MAPPED_FOO, {"f1.txt": "foo 1\n"})

    parent.link_update(MAPPED_BAR)
    parent.commit("Update the linked repository")
    parent.push()
    clone = parent.clone()
    assert pins_by_link_path(clone) == {MAPPED_FOO: newest, MAPPED_BAR: newest}
    assert_link_holds(clone, MAPPED_FOO, advanced)


@pytest.mark.smoke
def test_mapped_link_stage_unstage_and_dirty_pin_one_revision(new_lore_repo):
    """Staging under two mounts pins both at one revision holding both edits, unstaging under one
    moves both to one revision holding the other edit, and marking dirty under both shows both."""
    parent, _ = make_mapped_parent(new_lore_repo)
    pinned = pins_by_link_path(parent)[MAPPED_FOO]
    edited = {
        f"{MAPPED_FOO}/f1.txt": "edited through a\n",
        f"{MAPPED_BAR}/b1.txt": "edited through b\n",
    }
    parent.write_files(edited)

    parent.stage(list(edited))
    staged = pins_by_link_path(parent)
    assert staged[MAPPED_FOO] == staged[MAPPED_BAR] != pinned, staged
    assert status_by_path(parent).keys() == edited.keys()

    parent.unstage(f"{MAPPED_FOO}/f1.txt")
    unstaged = pins_by_link_path(parent)
    assert (
        unstaged[MAPPED_FOO] == unstaged[MAPPED_BAR] not in (pinned, staged[MAPPED_FOO])
    ), unstaged
    assert [entry["path"] for entry in unstaged_entries(parent)] == [
        f"{MAPPED_FOO}/f1.txt"
    ]
    assert status_by_path(parent)[f"{MAPPED_BAR}/b1.txt"]["flagStaged"]

    added = [f"{MAPPED_FOO}/f2.txt", f"{MAPPED_BAR}/b2.txt"]
    parent.write_files({path: "added\n" for path in added})
    parent.dirty(added)
    dirtied = pins_by_link_path(parent)
    assert dirtied[MAPPED_FOO] == dirtied[MAPPED_BAR], dirtied
    assert set(added) <= status_by_path(parent, check_dirty=True).keys()


@pytest.mark.smoke
def test_mapped_link_change_in_a_nested_link_moves_the_mount_holding_it(new_lore_repo):
    """A change staged in a repository linked inside one of two mounts, staged with a path through
    the other mount, moves the mount holding it, which commits it, and leaves the other mount at
    its pin until the commit moves both."""
    nested = make_repo(new_lore_repo, {"n1.txt": "nested 1\n"}, "Nested initial")
    linked = make_repo(new_lore_repo, MAPPED_LINKED_FILES, "Linked initial")
    linked.link_add("bar/nested", nested.get_id(), "/")
    linked.commit("Link the nested repository")
    linked.push()
    parent, _ = make_mapped_parent(new_lore_repo, linked)
    pinned = pins_by_link_path(parent)[MAPPED_FOO]
    parent.write_files({f"{MAPPED_BAR}/nested/n1.txt": "edited in the nested link\n"})

    parent.stage([f"{MAPPED_FOO}/f1.txt", f"{MAPPED_BAR}/nested/n1.txt"])
    staged = pins_by_link_path(parent)
    assert staged[MAPPED_FOO] == pinned != staged[MAPPED_BAR], staged
    parent.commit("Edit the nested repository")
    parent.push()

    linked.sync()
    committed = linked.branch_info().local_latest
    pins = pins_by_link_path(parent)
    assert {MAPPED_FOO: pins[MAPPED_FOO], MAPPED_BAR: pins[MAPPED_BAR]} == {
        MAPPED_FOO: committed,
        MAPPED_BAR: committed,
    }
    clone = parent.clone()
    assert (
        read_text(clone, f"{MAPPED_BAR}/nested/n1.txt") == "edited in the nested link\n"
    )


EDITED_THROUGH_BOTH = {
    f"{MAPPED_FOO}/f1.txt": "edited through a\n",
    f"{MAPPED_BAR}/b1.txt": "edited through b\n",
}


@pytest.mark.smoke
def test_mapped_link_commit_of_edits_under_two_mounts_commits_them_once(new_lore_repo):
    """Changes staged under two mounts commit as one revision of the linked repository holding
    both, which every mount pins and a clone holds: an edit, and a staged edit gone from the
    working tree, which commits as deleted."""
    parent, linked = make_mapped_parent(new_lore_repo)
    pinned = pins_by_link_path(parent)[MAPPED_FOO]
    parent.write_files(EDITED_THROUGH_BOTH)
    parent.stage(list(EDITED_THROUGH_BOTH))
    parent.remove_file(f"{MAPPED_BAR}/b1.txt")

    parent.commit("Edit through both mounts")
    parent.push()

    linked.sync()
    committed = linked.branch_info().local_latest
    assert pins_by_link_path(parent) == {MAPPED_FOO: committed, MAPPED_BAR: committed}
    assert linked.revision_info(committed).parent == pinned
    clone = parent.clone()
    assert_link_holds(clone, MAPPED_FOO, {"f1.txt": "edited through a\n"})
    assert_link_holds(clone, MAPPED_BAR, {})


@pytest.mark.smoke
def test_mapped_link_commit_of_one_mount_commits_every_mount(new_lore_repo):
    """A commit of one mount alone commits the edits staged under every mount as one revision,
    which every mount pins. A commit of the other mount alone then records a deletion on top of
    it, and a commit of the parent continues the same line of the linked branch."""
    parent, linked = make_mapped_parent(new_lore_repo)
    pinned = pins_by_link_path(parent)[MAPPED_FOO]
    parent.write_files(EDITED_THROUGH_BOTH)
    parent.stage(list(EDITED_THROUGH_BOTH))

    parent.commit("Edit through both mounts", link=MAPPED_FOO)
    first = pins_by_link_path(parent)[MAPPED_FOO]
    assert pins_by_link_path(parent) == {MAPPED_FOO: first, MAPPED_BAR: first}
    parent.remove_file(f"{MAPPED_BAR}/b1.txt")
    parent.stage([f"{MAPPED_BAR}/b1.txt"])
    parent.commit("Delete a file through one mount", link=MAPPED_BAR)
    second = pins_by_link_path(parent)[MAPPED_BAR]
    assert pins_by_link_path(parent) == {MAPPED_FOO: second, MAPPED_BAR: second}
    parent.write_files({f"{MAPPED_BAR}/b2.txt": "added through b\n"})
    parent.stage([f"{MAPPED_BAR}/b2.txt"])
    parent.commit("Add through one mount")
    parent.push()

    linked.sync()
    third = linked.branch_info().local_latest
    assert pins_by_link_path(parent) == {MAPPED_FOO: third, MAPPED_BAR: third}
    parents = [
        linked.revision_info(revision).parent for revision in (first, second, third)
    ]
    assert parents == [pinned, first, second]
    clone = parent.clone()
    assert_link_holds(clone, MAPPED_FOO, {"f1.txt": "edited through a\n"})
    assert_link_holds(clone, MAPPED_BAR, {"b2.txt": "added through b\n"})


@pytest.mark.smoke
@pytest.mark.parametrize("link", [None, f"{MAPPED_FOO}/nested"])
def test_mapped_link_commit_of_a_repository_linked_under_two_mounts(
    new_lore_repo, link
):
    """Edits staged in a repository that the linked repository links under both of its mounts
    commit as one revision of it, which both of its mounts pin, whether the commit is of the
    parent or of one of the nested links alone."""
    nested = make_repo(
        new_lore_repo, {"x/n1.txt": "x\n", "y/n2.txt": "y\n"}, "Nested initial"
    )
    nested_pinned = nested.branch_info().local_latest
    linked = make_repo(new_lore_repo, MAPPED_LINKED_FILES, "Linked initial")
    linked.link_add("foo/nested", nested.get_id(), "/x")
    linked.link_add("bar/nested", nested.get_id(), "/y")
    linked.commit("Link the nested repository twice")
    linked.push()
    parent, _ = make_mapped_parent(new_lore_repo, linked)
    edits = {
        f"{MAPPED_FOO}/nested/n1.txt": "edited through a\n",
        f"{MAPPED_BAR}/nested/n2.txt": "edited through b\n",
    }
    parent.write_files(edits)
    parent.stage(list(edits))

    if link:
        parent.commit("Edit the nested repository alone", link=link)
    parent.commit("Edit the nested repository")
    parent.push()

    nested.sync()
    committed = nested.branch_info().local_latest
    assert nested.revision_info(committed).parent == nested_pinned
    linked.sync()
    assert pins_by_link_path(linked) == {
        "foo/nested": committed,
        "bar/nested": committed,
    }
    linked_latest = linked.branch_info().local_latest
    pins = pins_by_link_path(parent)
    assert {
        MAPPED_FOO: pins[MAPPED_FOO],
        MAPPED_BAR: pins[MAPPED_BAR],
    } == dict.fromkeys((MAPPED_FOO, MAPPED_BAR), linked_latest)
    clone = parent.clone()
    assert_link_holds(clone, f"{MAPPED_FOO}/nested", {"n1.txt": "edited through a\n"})
    assert_link_holds(clone, f"{MAPPED_BAR}/nested", {"n2.txt": "edited through b\n"})
