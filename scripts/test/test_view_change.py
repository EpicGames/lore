# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Changing the view an existing instance materializes its working tree under.

`lore sync --view <file>` carries the working tree from the subset one view
materializes to the subset another does, at a standing revision or alongside a
move to a new one. The instance is left coherent: `.lore/view` names the view the
tree stands under, so a following `status --scan` agrees with it, and what is on
disk is what `lore clone --view` of the same file would have produced.
"""

import logging
import os
from dataclasses import dataclass

import pytest
from error_types import LocalModificationsError, LoreException
from lore_parsers import parse_jsonl, parse_status_json
from test_utils import to_posix

from lore import Lore

logger = logging.getLogger(__name__)

# The tree every test here commits. `assets/drop` is what a narrowing view drops,
# `assets/keep` is what it keeps, and `keep.txt` is at the top level so a view
# rooted at `/*` drops it while a view naming `assets/drop` does not.
TOP_LEVEL = "keep.txt"
KEPT = os.path.join("assets", "keep", "keep.uasset")
DROPPED = os.path.join("assets", "drop", "one.uasset")
ALSO_DROPPED = os.path.join("assets", "drop", "two.uasset")
DROPPED_DIRECTORY = os.path.join("assets", "drop")
# The size of every committed file, so the bytes a view change reports writing are
# a number the test can name.
FILE_BYTES = 256

# Excludes the dropped directory and everything below it.
NARROW_VIEW = ["/assets/drop"]
# Excludes what the directory holds while leaving the directory node itself in
# view, which has to stay materialized and empty -- the same shape `lore clone`
# carries a regression test for.
EMPTIED_VIEW = ["/assets/drop/**"]
# The canonical shape of an authored view: drop everything at the top, re-include
# the one tree wanted, then narrow within it. `*` cannot match below depth one, so
# the re-included subtree's own children were never excluded.
REINCLUDING_VIEW = ["/*", "!/assets", "/assets/drop"]
# No rules at all, which puts the whole repository in view.
WIDE_VIEW = []


@dataclass(frozen=True)
class Realized:
    """What a sync reported doing to the working files.

    The progress counters are monotonic and a sync ends by sending its final ones,
    so the largest of what arrived is what it did.
    """

    files_written: int
    bytes_written: int
    files_deleted: int

    @staticmethod
    def from_events(output: str) -> "Realized":
        progress = parse_jsonl(output, "revisionSyncProgress")
        return Realized(
            files_written=max((e["fileUpdate"] for e in progress), default=0),
            bytes_written=max((e["bytesUpdate"] for e in progress), default=0),
            files_deleted=max((e["fileDelete"] for e in progress), default=0),
        )


def committed_repository(new_lore_repo) -> Lore:
    """A pushed repository holding the tree above, materialized whole."""
    repo: Lore = new_lore_repo()
    for path in [TOP_LEVEL, KEPT, DROPPED, ALSO_DROPPED]:
        directory = os.path.dirname(path)
        if directory:
            repo.make_dirs(directory)
        with repo.open_file(path, "w+b") as output_file:
            output_file.write(os.urandom(FILE_BYTES))
    repo.stage(scan=True)
    repo.commit()
    repo.push()
    return repo


def view_file(scratch_dir, name: str, rules: list[str]) -> str:
    """A view filter file holding `rules`, one per line, beside the repositories.

    Outside every working tree, since a file inside one would be part of the tree
    the view change carries.
    """
    directory = scratch_dir("view", create=True)
    path = os.path.join(directory, name + ".txt")
    with open(path, "w+") as output_file:
        output_file.writelines(rule + "\n" for rule in rules)
    return path


def sync_view(instance: Lore, view: str, **kwargs) -> tuple[str, Realized]:
    """Syncs `instance` under `view`, answering the event output and what it did."""
    output = instance.sync(view=view, json=True, **kwargs)
    return output, Realized.from_events(output)


def reported_files(output: str) -> set[str]:
    """Every file a sync reported by name, with the action it carried."""
    return {
        f"{event['action']} {to_posix(event['path'])}"
        for event in parse_jsonl(output, "revisionSyncFile")
    }


def stored_view(instance: Lore) -> str | None:
    """The rules the instance's own view file holds, or `None` where it has none."""
    path = os.path.join(instance.dot_path(), "view")
    if not os.path.exists(path):
        return None
    with open(path) as view:
        return view.read()


def materialized(instance: Lore) -> set[str]:
    """Every path the instance holds on disk."""
    return {to_posix(path) for path in instance.list_paths()}


def assert_status_clean(instance: Lore, message: str) -> None:
    """A scan of the working tree reports nothing.

    The instance's view and what is on disk agree only if this holds: a scan reads
    the view from `.lore/view`, so a path the sync removed while the file still
    admits it surfaces as a phantom local delete.
    """
    entries = parse_status_json(instance.status(json=True, offline=True, scan=True))
    assert {to_posix(entry.get("path", "")) for entry in entries} == set(), message


def status_revisions(instance: Lore) -> dict:
    """What the instance reports of the revisions it stands on."""
    entries = parse_jsonl(
        instance.status(json=True, offline=True, revision_only=True),
        "repositoryStatusRevision",
    )
    return entries[-1]


def current_revision(instance: Lore) -> str:
    """The revision the instance's working tree stands on."""
    return status_revisions(instance)["revision"]


def staged_revision(instance: Lore) -> str:
    """The staged state the instance holds, which is zero while it holds none."""
    return status_revisions(instance)["revisionStaged"]


@pytest.mark.smoke
def test_sync_view_narrows_then_widens_an_instance(new_lore_repo, scratch_dir):
    """A narrowing removes what the target view drops and a widening reads it back
    from the store, both at a revision that does not move.

    The narrowed tree is held against a clone taken with the same view file, which
    is what the feature replaces: the two must materialize the same set of paths.
    """
    repo = committed_repository(new_lore_repo)
    instance = repo.clone()
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)
    wide = view_file(scratch_dir, "wide", WIDE_VIEW)
    before = current_revision(instance)

    output, realized = sync_view(instance, narrow)

    assert not instance.path_exists(DROPPED_DIRECTORY), (
        "the target view drops the directory whole"
    )
    assert instance.file_exists(TOP_LEVEL) and instance.file_exists(KEPT), (
        "a path both views admit is left as it stands"
    )
    assert realized == Realized(files_written=0, bytes_written=0, files_deleted=3), (
        "a narrowing writes nothing and removes the two files and their directory"
    )
    assert reported_files(output) == set(), (
        "a successful delete reports no per-file event, which a narrowing makes "
        "visible: the success arm of the unlink clears the flag the event is gated on"
    )
    assert current_revision(instance) == before, "a view change mints no revision"
    assert stored_view(instance) == "assets/drop\n", (
        "the view the tree stands under is published as the instance's own, written "
        "back from the rules as parsed: a rule holding a separator is anchored at the "
        "root already, so it keeps no leading one"
    )
    assert_status_clean(instance, "a narrowed instance is coherent with its view")
    assert materialized(instance) == materialized(repo.clone(view=narrow)), (
        "the narrowed tree holds what a clone with the same view would have"
    )

    output, realized = sync_view(instance, wide)

    for path in [DROPPED, ALSO_DROPPED]:
        assert repo.compare_file(instance, path), (
            f"a path entering the view is written from the store: {path}"
        )
    assert realized == Realized(
        files_written=2, bytes_written=2 * FILE_BYTES, files_deleted=0
    ), "a widening writes what enters the view and removes nothing"
    assert reported_files(output) == {
        "add assets/drop",
        "add assets/drop/one.uasset",
        "add assets/drop/two.uasset",
    }, "the directory and the files entering the view are each reported once"
    assert stored_view(instance) == "", "a view holding no rules is published as one"
    assert_status_clean(instance, "a widened instance is coherent with its view")
    assert materialized(instance) == materialized(repo), (
        "a view holding no rules materializes the whole repository"
    )


@pytest.mark.smoke
def test_sync_view_narrows_to_a_re_including_view(new_lore_repo, scratch_dir):
    """A view built the way they are authored -- drop everything at the top,
    re-include the one tree wanted, narrow within it -- is carried as written.

    The unanchored exclusion it opens with is also the shape that defeats the
    content-equality prune, so this is the walk that visits the whole tree and has
    to arrive at the same answer.
    """
    repo = committed_repository(new_lore_repo)
    instance = repo.clone()
    reincluding = view_file(scratch_dir, "reincluding", REINCLUDING_VIEW)

    _output, realized = sync_view(instance, reincluding)

    assert not instance.path_exists(TOP_LEVEL), (
        "a top-level file the view drops leaves the tree"
    )
    assert not instance.path_exists(DROPPED_DIRECTORY), (
        "an exclusion below the re-included tree is honoured"
    )
    assert instance.file_exists(KEPT), "the re-included tree is materialized"
    assert realized == Realized(files_written=0, bytes_written=0, files_deleted=4), (
        "the top-level file leaves beside the dropped directory and its two files"
    )
    assert_status_clean(instance, "a re-including view leaves a coherent instance")
    assert materialized(instance) == materialized(repo.clone(view=reincluding)), (
        "the tree holds what a clone with the same view would have"
    )


@pytest.mark.smoke
def test_sync_view_keeps_a_directory_its_children_leave(new_lore_repo, scratch_dir):
    """A view that drops what a directory holds while keeping the directory node
    leaves it materialized and empty, which is what a clone with that view does.

    The status that follows is what makes it matter: the node is in view, so an
    instance missing the directory would report a phantom local delete of it.
    """
    repo = committed_repository(new_lore_repo)
    instance = repo.clone()
    emptied = view_file(scratch_dir, "emptied", EMPTIED_VIEW)

    _output, realized = sync_view(instance, emptied)

    directory = os.path.join(instance.path, DROPPED_DIRECTORY)
    assert os.path.isdir(directory), (
        "a directory node in view stays materialized once its children leave"
    )
    assert os.listdir(directory) == [], (
        f"every child is dropped by the view, found: {os.listdir(directory)}"
    )
    assert realized == Realized(files_written=0, bytes_written=0, files_deleted=2), (
        "the two files leave and the directory the view still admits does not"
    )
    assert_status_clean(instance, "an emptied directory is not a phantom delete")
    assert materialized(instance) == materialized(repo.clone(view=emptied)), (
        "the tree holds what a clone with the same view would have"
    )


@pytest.mark.smoke
def test_sync_view_carries_a_revision_move_as_well(new_lore_repo, scratch_dir):
    """One sync carries the tree to the target revision and the target view at
    once, leaving what that view materializes of that revision -- not one of the
    two applied to the other.
    """
    repo = committed_repository(new_lore_repo)
    instance = repo.clone()
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)
    added = os.path.join("assets", "keep", "added.uasset")
    source = current_revision(instance)

    with repo.open_file(KEPT, "w+b") as output_file:
        output_file.write(os.urandom(FILE_BYTES))
    with repo.open_file(added, "w+b") as output_file:
        output_file.write(os.urandom(FILE_BYTES))
    repo.stage(scan=True)
    repo.commit("Second")
    repo.push()

    output, realized = sync_view(instance, narrow)

    target = parse_jsonl(output, "revisionSyncTarget")[-1]
    assert (target["sourceRevision"], target["targetRevision"]) == (
        source,
        current_revision(repo),
    ), "the sync resolves the revision the remote branch holds"
    assert current_revision(instance) == current_revision(repo), (
        "the instance is left on the revision it synced to"
    )
    assert repo.compare_file(instance, KEPT), "a rewritten file in view is written"
    assert repo.compare_file(instance, added), "a file added in view is written"
    assert realized == Realized(
        files_written=2, bytes_written=2 * FILE_BYTES, files_deleted=3
    ), "the revision's two files are written and the view's directory leaves"
    assert not instance.path_exists(DROPPED_DIRECTORY), (
        "the target view drops the directory whatever the revision holds"
    )
    assert_status_clean(instance, "the instance is coherent with revision and view")


@pytest.mark.smoke
@pytest.mark.parametrize("force", [False, True])
def test_sync_view_drops_dirty_flags_for_paths_leaving_the_view(
    new_lore_repo, scratch_dir, force
):
    """A dirty flag on a path the target view drops is not carried forward: the
    file it names is one the working tree no longer holds, so the flag can only be
    re-applied to nothing -- hidden while that view holds, since a status asks the
    same filter, and a phantom local change once the view widens again.

    `--force` carries flags the filter excludes for an operation whose filter is
    the one they were recorded under. A view change is not one: its filter is the
    view the tree is left under.
    """
    repo = committed_repository(new_lore_repo)
    instance = repo.clone()
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)

    # Marked without touching the file, so the flag is the only thing stale about
    # it and the delete the narrowing emits is verified against matching content.
    unflagged = staged_revision(instance)
    instance.dirty(DROPPED)
    flagged = parse_status_json(instance.status(json=True, offline=True))
    assert {to_posix(entry.get("path", "")) for entry in flagged} == {to_posix(DROPPED)}
    assert staged_revision(instance) != unflagged, "a dirty path anchors a staged state"

    sync_view(instance, narrow, force=force)

    assert not instance.path_exists(DROPPED), "the narrowing removes the flagged file"
    # The staged anchor rather than what status reports of it: a flag carried
    # forward for a path the new view excludes is filtered back out of the report,
    # so only the anchor says whether the rebase kept it.
    assert staged_revision(instance) == unflagged, (
        "the flag leaves with the file it names"
    )
    assert_status_clean(instance, "the instance is coherent with its view")


@pytest.mark.smoke
def test_sync_view_refuses_what_it_cannot_carry(new_lore_repo, scratch_dir):
    """A reset, a dependency set and a view file that cannot be read are each
    refused, and each refusal leaves the instance under the view it holds.

    An unreadable view file especially: read as the empty filter every other
    loader answers with, it would mean the whole repository in view.
    """
    repo = committed_repository(new_lore_repo)
    instance = repo.clone()
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)
    absent = os.path.join(os.path.dirname(narrow), "absent.txt")

    refusals = [
        ("Unable to change the view of a sync that resets", {"reset": True}),
        ("Unable to change the view of a sync restricted", {"root_files": [KEPT]}),
        ("Failed to read view filter", {"view": absent}),
    ]
    for message, options in refusals:
        with pytest.raises(LoreException, match=message):
            instance.sync(**{"view": narrow} | options)

    assert stored_view(instance) is None, "a refused sync publishes no view"
    assert instance.file_exists(DROPPED), "a refused sync carries nothing"
    assert_status_clean(instance, "a refused sync leaves the instance as it was")


@pytest.mark.smoke
def test_sync_view_refuses_a_sync_that_would_merge(new_lore_repo, scratch_dir):
    """A sync that resolves to a merge is refused with a view change: the merge
    realizes its result under one view and leaves it staged, so the view change
    would have to be carried on top of a tree no revision holds.

    Two instances part company the way the divergence tests do -- one commits and
    pushes, the other commits without pushing -- which is what puts a sync on the
    merge path.
    """
    repo = committed_repository(new_lore_repo)
    ahead = repo.clone()
    instance = repo.clone()
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)

    with ahead.open_file(KEPT, "w+b") as output_file:
        output_file.write(os.urandom(FILE_BYTES))
    ahead.stage(scan=True)
    ahead.commit("Ahead of the other instance")
    ahead.push()

    with instance.open_file(TOP_LEVEL, "w+b") as output_file:
        output_file.write(os.urandom(FILE_BYTES))
    instance.stage(scan=True)
    instance.commit("Aside from the remote", local=True)

    with pytest.raises(LoreException, match="Unable to change the view of a sync"):
        instance.sync(view=narrow)

    assert stored_view(instance) is None, "a refused sync publishes no view"
    assert instance.file_exists(DROPPED), "a refused sync carries nothing"


@pytest.mark.smoke
def test_sync_view_refuses_to_delete_local_modifications(new_lore_repo, scratch_dir):
    """A narrowing that would delete a locally modified file is refused, leaving
    the instance under its old view with the local work in place.

    `--forward-changes` is the way through: the local work is kept, the rest of
    the subtree leaves, and the directory holding it stays for what it still
    holds.
    """
    repo = committed_repository(new_lore_repo)
    instance = repo.clone()
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)
    with instance.open_file(DROPPED, "w+b") as output_file:
        output_file.write(os.urandom(2 * FILE_BYTES))

    with pytest.raises(LocalModificationsError):
        instance.sync(view=narrow)

    assert stored_view(instance) is None, "a refused sync publishes no view"
    assert instance.file_exists(DROPPED) and instance.file_exists(ALSO_DROPPED), (
        "the changes are verified before any of them is realized"
    )

    sync_view(instance, narrow, forward_changes=True)

    assert instance.file_exists(DROPPED), "the local work is what was forwarded"
    assert not instance.path_exists(ALSO_DROPPED), (
        "the rest of the subtree leaves the view"
    )
    assert stored_view(instance) == "assets/drop\n", "the view is published"


@pytest.mark.smoke
def test_sync_view_applied_twice_changes_nothing(new_lore_repo, scratch_dir):
    """Applying the view the instance already holds carries nothing.

    The same view file twice is also what re-running an interrupted apply looks
    like, so the second pass has to be a no-op rather than a second narrowing of
    an already narrowed tree.
    """
    repo = committed_repository(new_lore_repo)
    instance = repo.clone()
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)

    sync_view(instance, narrow)
    once = materialized(instance)
    _output, realized = sync_view(instance, narrow)

    assert realized == Realized(files_written=0, bytes_written=0, files_deleted=0), (
        "a re-applied view carries nothing"
    )
    assert materialized(instance) == once, "a re-applied view leaves the tree alone"
    assert stored_view(instance) == "assets/drop\n", "the view file is rewritten as is"
    assert_status_clean(instance, "the instance stays coherent with its view")
