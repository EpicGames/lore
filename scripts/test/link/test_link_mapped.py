# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""One repository linked at several paths, each mount exposing another folder of it: every mount
pins the same revision."""

import pytest
from error_types import ImproperArgumentsError, LocalChanges
from link_helpers import assert_link_holds, make_repo
from lore_parsers import parse_jsonl
from test_utils import status_by_path, unstaged_entries, working_tree_files

from lore import Lore

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)

LINKED_FILES = {
    "foo/f1.txt": "foo 1\n",
    "bar/b1.txt": "bar 1\n",
    "baz/z1.txt": "baz 1\n",
}
FOO = "a/lib"
BAR = "b/lib"
BAZ = "c/lib"


def _mapped(new_lore_repo) -> tuple[Lore, Lore]:
    """Returns (parent, linked), the parent mounting the linked repository's `foo` at
    `FOO` and its `bar` at `BAR`, committed and pushed."""
    linked = make_repo(new_lore_repo, LINKED_FILES, "Linked initial")
    parent = make_repo(new_lore_repo, {"README.txt": "baseline\n"}, "Baseline")
    parent.link_add(FOO, linked.get_id(), "/foo")
    parent.link_add(BAR, linked.get_id(), "/bar")
    parent.commit("Mount the linked repository twice")
    parent.push()
    return parent, linked


def _pins(repo: Lore) -> dict[str, str]:
    """The revision each mount pins, by mount path."""
    return {
        entry["linkPath"]: entry["revision"]
        for entry in parse_jsonl(repo.link_list(json=True), "linkEntry")
    }


@pytest.mark.smoke
def test_mapped_link_added_later_joins_the_pin_of_its_mounts(new_lore_repo):
    """A third mount of a repository that moved on since pins the revision its other mounts
    pin, refuses another, and `link list` shows the repository once with all three mounts."""
    parent, linked = _mapped(new_lore_repo)
    pinned = _pins(parent)[FOO]
    linked.write_commit_push("Linked advances", {"foo/f2.txt": "foo 2\n"})

    with pytest.raises(ImproperArgumentsError):
        parent.link_add(
            BAZ, linked.get_id(), "/baz", pin=linked.branch_info().local_latest
        )
    parent.link_add(BAZ, linked.get_id(), "/baz")

    assert _pins(parent) == {FOO: pinned, BAR: pinned, BAZ: pinned}
    parent.commit("Mount the linked repository a third time")
    parent.push()
    listing = parent.link_list()
    assert listing.count(f"Link {linked.get_id()}") == 1, listing
    for mount, source in ((FOO, "foo"), (BAR, "bar"), (BAZ, "baz")):
        assert f"{mount} <- {source}" in listing, listing
    clone = parent.clone()
    assert _pins(clone) == {FOO: pinned, BAR: pinned, BAZ: pinned}
    with clone.open_file(f"{BAZ}/z1.txt") as baz:
        assert baz.read() == "baz 1\n"


@pytest.mark.smoke
def test_mapped_link_commit_through_one_mount_moves_every_mount(new_lore_repo):
    """On a branch created over the mounts, a commit of one mount alone pins the revision it
    commits at every mount; the pushed revision clones and verifies with every mount, and main
    keeps its pin."""
    parent, linked = _mapped(new_lore_repo)
    parent.link_add(BAZ, linked.get_id(), "/baz")
    parent.commit("Mount baz")
    parent.push()
    pinned = _pins(parent)[FOO]

    parent.branch_create("feature")
    branched = _pins(parent)[FOO]
    assert _pins(parent) == {FOO: branched, BAR: branched, BAZ: branched}
    parent.write_files({f"{FOO}/f1.txt": "edited through a\n"})
    parent.stage([f"{FOO}/f1.txt"])
    parent.commit("Edit through one mount alone", link=FOO)
    parent.commit("Pin every mount")
    parent.push()

    committed = _pins(parent)[FOO]
    mounts = {FOO: committed, BAR: committed, BAZ: committed}
    assert _pins(parent) == mounts
    assert committed not in (pinned, branched)
    parent.repository_verify()
    clone = parent.clone(branch="feature")
    assert _pins(clone) == mounts
    assert_link_holds(clone, FOO, {"f1.txt": "edited through a\n"})
    assert_link_holds(clone, BAR, {"b1.txt": "bar 1\n"})
    parent.branch_switch("main")
    assert _pins(parent) == {FOO: pinned, BAR: pinned, BAZ: pinned}


@pytest.mark.smoke
def test_mapped_link_update_moves_and_unstage_restores_every_mount(new_lore_repo):
    """Updating one mount, refused by a local change at another, leaves every mount as it was;
    once the change is gone it moves every mount of the repository, unstaging it, or every mount
    at once, puts every mount back, and the update committed again clones with both mounts at the
    new revision."""
    parent, linked = _mapped(new_lore_repo)
    pinned = _pins(parent)[FOO]
    linked.write_commit_push(
        "Linked advances", {"foo/f2.txt": "foo 2\n", "bar/b1.txt": "bar 2\n"}
    )
    newest = linked.branch_info().local_latest
    advanced = {"f1.txt": "foo 1\n", "f2.txt": "foo 2\n"}
    parent.write_files({f"{BAR}/b1.txt": "edited through b\n"})

    with pytest.raises(LocalChanges):
        parent.link_update(FOO)
    assert _pins(parent) == {FOO: pinned, BAR: pinned}
    assert working_tree_files(parent, FOO) == [f"{FOO}/f1.txt"]
    parent.reset(f"{BAR}/b1.txt")

    for unstaged in (FOO, [FOO, BAR]):
        parent.link_update(FOO)
        assert _pins(parent) == {FOO: newest, BAR: newest}
        assert_link_holds(parent, FOO, advanced)
        parent.unstage(unstaged)
        assert _pins(parent) == {FOO: pinned, BAR: pinned}
        assert_link_holds(parent, FOO, {"f1.txt": "foo 1\n"})

    parent.link_update(BAR)
    parent.commit("Update the linked repository")
    parent.push()
    clone = parent.clone()
    assert _pins(clone) == {FOO: newest, BAR: newest}
    assert_link_holds(clone, FOO, advanced)


@pytest.mark.smoke
def test_mapped_link_stage_unstage_and_dirty_pin_one_revision(new_lore_repo):
    """Staging under two mounts pins both at one revision holding both edits, unstaging under one
    moves both to one revision holding the other edit, and marking dirty under both shows both."""
    parent, _ = _mapped(new_lore_repo)
    pinned = _pins(parent)[FOO]
    edited = {
        f"{FOO}/f1.txt": "edited through a\n",
        f"{BAR}/b1.txt": "edited through b\n",
    }
    parent.write_files(edited)

    parent.stage(list(edited))
    staged = _pins(parent)
    assert staged[FOO] == staged[BAR] != pinned, staged
    assert status_by_path(parent).keys() == edited.keys()

    parent.unstage(f"{FOO}/f1.txt")
    unstaged = _pins(parent)
    assert unstaged[FOO] == unstaged[BAR] not in (pinned, staged[FOO]), unstaged
    assert [entry["path"] for entry in unstaged_entries(parent)] == [f"{FOO}/f1.txt"]
    assert status_by_path(parent)[f"{BAR}/b1.txt"]["flagStaged"]

    added = [f"{FOO}/f2.txt", f"{BAR}/b2.txt"]
    parent.write_files({path: "added\n" for path in added})
    parent.dirty(added)
    dirtied = _pins(parent)
    assert dirtied[FOO] == dirtied[BAR], dirtied
    assert set(added) <= status_by_path(parent, check_dirty=True).keys()


@pytest.mark.smoke
def test_mapped_link_change_in_a_nested_link_moves_the_mount_holding_it(new_lore_repo):
    """A change staged in a repository linked inside one of two mounts, staged with a path through
    the other mount, moves the mount holding it, which commits it, and leaves the other mount at
    its pin until the commit moves both."""
    nested = make_repo(new_lore_repo, {"n1.txt": "nested 1\n"}, "Nested initial")
    linked = make_repo(new_lore_repo, LINKED_FILES, "Linked initial")
    linked.link_add("bar/nested", nested.get_id(), "/")
    linked.commit("Link the nested repository")
    linked.push()
    parent = make_repo(new_lore_repo, {"README.txt": "baseline\n"}, "Baseline")
    parent.link_add(FOO, linked.get_id(), "/foo")
    parent.link_add(BAR, linked.get_id(), "/bar")
    parent.commit("Mount the linked repository twice")
    parent.push()
    pinned = _pins(parent)[FOO]
    parent.write_files({f"{BAR}/nested/n1.txt": "edited in the nested link\n"})

    parent.stage([f"{FOO}/f1.txt", f"{BAR}/nested/n1.txt"])
    staged = _pins(parent)
    assert staged[FOO] == pinned != staged[BAR], staged
    parent.commit("Edit the nested repository")
    parent.push()

    linked.sync()
    committed = linked.branch_info().local_latest
    pins = _pins(parent)
    assert {FOO: pins[FOO], BAR: pins[BAR]} == {FOO: committed, BAR: committed}
    clone = parent.clone()
    with clone.open_file(f"{BAR}/nested/n1.txt") as edited:
        assert edited.read() == "edited in the nested link\n"


EDITED_THROUGH_BOTH = {
    f"{FOO}/f1.txt": "edited through a\n",
    f"{BAR}/b1.txt": "edited through b\n",
}


@pytest.mark.smoke
def test_mapped_link_commit_of_edits_under_two_mounts_commits_them_once(new_lore_repo):
    """Changes staged under two mounts commit as one revision of the linked repository holding
    both, which every mount pins and a clone holds: an edit, and a staged edit gone from the
    working tree, which commits as deleted."""
    parent, linked = _mapped(new_lore_repo)
    pinned = _pins(parent)[FOO]
    parent.write_files(EDITED_THROUGH_BOTH)
    parent.stage(list(EDITED_THROUGH_BOTH))
    parent.remove_file(f"{BAR}/b1.txt")

    parent.commit("Edit through both mounts")
    parent.push()

    linked.sync()
    committed = linked.branch_info().local_latest
    assert _pins(parent) == {FOO: committed, BAR: committed}
    assert linked.revision_info(committed).parent == pinned
    clone = parent.clone()
    assert_link_holds(clone, FOO, {"f1.txt": "edited through a\n"})
    assert_link_holds(clone, BAR, {})


@pytest.mark.smoke
def test_mapped_link_commit_of_one_mount_commits_every_mount(new_lore_repo):
    """A commit of one mount alone commits the edits staged under every mount as one revision,
    which every mount pins. A commit of the other mount alone then records a deletion on top of
    it, and a commit of the parent continues the same line of the linked branch."""
    parent, linked = _mapped(new_lore_repo)
    pinned = _pins(parent)[FOO]
    parent.write_files(EDITED_THROUGH_BOTH)
    parent.stage(list(EDITED_THROUGH_BOTH))

    parent.commit("Edit through both mounts", link=FOO)
    first = _pins(parent)[FOO]
    assert _pins(parent) == {FOO: first, BAR: first}
    parent.remove_file(f"{BAR}/b1.txt")
    parent.stage([f"{BAR}/b1.txt"])
    parent.commit("Delete a file through one mount", link=BAR)
    second = _pins(parent)[BAR]
    assert _pins(parent) == {FOO: second, BAR: second}
    parent.write_files({f"{BAR}/b2.txt": "added through b\n"})
    parent.stage([f"{BAR}/b2.txt"])
    parent.commit("Add through one mount")
    parent.push()

    linked.sync()
    third = linked.branch_info().local_latest
    assert _pins(parent) == {FOO: third, BAR: third}
    parents = [
        linked.revision_info(revision).parent for revision in (first, second, third)
    ]
    assert parents == [pinned, first, second]
    clone = parent.clone()
    assert_link_holds(clone, FOO, {"f1.txt": "edited through a\n"})
    assert_link_holds(clone, BAR, {"b2.txt": "added through b\n"})


@pytest.mark.smoke
@pytest.mark.parametrize("link", [None, f"{FOO}/nested"])
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
    linked = make_repo(new_lore_repo, LINKED_FILES, "Linked initial")
    linked.link_add("foo/nested", nested.get_id(), "/x")
    linked.link_add("bar/nested", nested.get_id(), "/y")
    linked.commit("Link the nested repository twice")
    linked.push()
    parent = make_repo(new_lore_repo, {"README.txt": "baseline\n"}, "Baseline")
    parent.link_add(FOO, linked.get_id(), "/foo")
    parent.link_add(BAR, linked.get_id(), "/bar")
    parent.commit("Mount the linked repository twice")
    parent.push()
    edits = {
        f"{FOO}/nested/n1.txt": "edited through a\n",
        f"{BAR}/nested/n2.txt": "edited through b\n",
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
    assert _pins(linked) == {"foo/nested": committed, "bar/nested": committed}
    linked_latest = linked.branch_info().local_latest
    assert {mount: _pins(parent)[mount] for mount in (FOO, BAR)} == dict.fromkeys(
        (FOO, BAR), linked_latest
    )
    clone = parent.clone()
    assert_link_holds(clone, f"{FOO}/nested", {"n1.txt": "edited through a\n"})
    assert_link_holds(clone, f"{BAR}/nested", {"n2.txt": "edited through b\n"})
