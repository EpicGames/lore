# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""One repository linked at several paths, each mount exposing another folder of it: every mount
pins the same revision."""

import pytest
from error_types import ImproperArgumentsError, NotSupportedError
from link_helpers import assert_link_holds, make_repo
from lore_parsers import parse_jsonl

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
    """Updating one mount moves every mount of the repository, unstaging it, or every mount at
    once, puts every mount back, and the update committed again clones with both mounts at the new
    revision."""
    parent, linked = _mapped(new_lore_repo)
    pinned = _pins(parent)[FOO]
    linked.write_commit_push("Linked advances", {"foo/f2.txt": "foo 2\n"})
    newest = linked.branch_info().local_latest
    advanced = {"f1.txt": "foo 1\n", "f2.txt": "foo 2\n"}

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
