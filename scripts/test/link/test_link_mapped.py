# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""One repository linked at several paths, each mount exposing another folder of it: every mount
pins the same revision."""

import pytest
from error_types import ImproperArgumentsError, LocalChanges, NotSupportedError
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
FOO_AGAIN = "d/lib"
FOO_THIRD = "e/lib"


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
    """On a branch created over the mounts, a commit of a change staged under one mount pins the
    revision it commits at every mount and shows the change at another mount of the same folder,
    where a commit of that mount alone is refused; the pushed revision clones and verifies with
    every mount, and main keeps its pin."""
    parent, linked = _mapped(new_lore_repo)
    parent.link_add(FOO_AGAIN, linked.get_id(), "/foo")
    parent.commit("Mount foo again")
    parent.push()
    pinned = _pins(parent)[FOO]

    parent.branch_create("feature")
    branched = _pins(parent)[FOO]
    assert _pins(parent) == {FOO: branched, BAR: branched, FOO_AGAIN: branched}
    parent.write_files({f"{FOO}/f1.txt": "edited through a\n"})
    parent.stage([f"{FOO}/f1.txt"])
    with pytest.raises(NotSupportedError):
        parent.commit("Edit through one mount alone", link=FOO)
    parent.commit("Edit through one mount")
    parent.push()

    committed = _pins(parent)[FOO]
    mounts = {FOO: committed, BAR: committed, FOO_AGAIN: committed}
    assert _pins(parent) == mounts
    assert committed not in (pinned, branched)
    assert_link_holds(parent, FOO_AGAIN, {"f1.txt": "edited through a\n"})
    parent.repository_verify()
    clone = parent.clone(branch="feature")
    assert _pins(clone) == mounts
    assert_link_holds(clone, FOO, {"f1.txt": "edited through a\n"})
    assert_link_holds(clone, BAR, {"b1.txt": "bar 1\n"})
    parent.branch_switch("main")
    assert _pins(parent) == {FOO: pinned, BAR: pinned, FOO_AGAIN: pinned}


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
def test_mapped_link_folder_mounted_three_times(new_lore_repo):
    """An edit through one of three mounts of one folder stays marked dirty where another copy is
    unchanged. A commit of it that the third mount refuses to realize leaves the second copy
    and the linked branch as they were, so a push publishes nothing of the linked repository, and
    the commit goes through once the third mount's local change is gone."""
    parent, linked = _mapped(new_lore_repo)
    for mount in (FOO_AGAIN, FOO_THIRD):
        parent.link_add(mount, linked.get_id(), "/foo")
    parent.commit("Mount foo twice more")
    parent.push()
    pinned = _pins(parent)[FOO]
    parent.write_files({f"{FOO}/f1.txt": "edited through a\n"})
    parent.dirty([f"{FOO}/f1.txt", f"{FOO_AGAIN}/f1.txt"])

    assert f"{FOO}/f1.txt" in status_by_path(parent, check_dirty=True)
    assert f"{FOO}/f1.txt" in status_by_path(parent)

    parent.stage([f"{FOO}/f1.txt"])
    parent.write_files({f"{FOO_THIRD}/f1.txt": "edited through e\n"})
    with pytest.raises(LocalChanges):
        parent.commit("Edit through one mount")
    with parent.open_file(f"{FOO_AGAIN}/f1.txt") as foo:
        assert foo.read() == "foo 1\n"
    parent.push()
    linked.sync()
    assert linked.branch_info().local_latest == pinned

    parent.write_files({f"{FOO_THIRD}/f1.txt": "foo 1\n"})
    parent.commit("Edit through one mount")
    parent.push()
    linked.sync()
    committed = linked.branch_info().local_latest
    mounts = (FOO, BAR, FOO_AGAIN, FOO_THIRD)
    assert _pins(parent) == dict.fromkeys(mounts, committed)
    assert linked.revision_info(committed).parent == pinned
    with parent.open_file(f"{FOO_AGAIN}/f1.txt") as foo:
        assert foo.read() == "edited through a\n"


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
