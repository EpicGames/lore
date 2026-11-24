# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""The directories a sync, a branch switch and a merge to another revision leave under an
unchanged view.

An operation that moves the working tree to another revision under the same view
leaves the directories an instance cloned at that revision with that view holds:
every directory the view keeps, and one it excludes only where it holds what the
operation keeps. It keeps every directory holding anything, an untracked or an
ignored file included.
"""

from dataclasses import dataclass

import pytest
from lore_parsers import parse_status_json
from test_utils import status_revisions, view_file, working_tree_files

from lore import Lore

# A view that leaves out `.bat` files and excludes `a` while re-including the `.txt`
# files below it, so a directory in `a` stands on disk only for a `.txt` file in it.
VIEW = ["*.bat", "/a/", "!/a/**/*.txt"]
# The file each case commits beside its own, so no tree is empty.
TOP_LEVEL = "top.txt"
# The file a merge's other side commits, so the merge is not a fast-forward.
OTHER_SIDE = "other.txt"


@dataclass
class Case:
    """The tree a case commits at R-1, as files and empty directories, and the
    files R deletes from it and adds to it."""

    name: str
    files: list[str]
    empty: list[str]
    deleted: list[str]
    added: list[str]


# R deletes `a/x.txt` and adds `a/sub/y.bat`, so an instance cloned at R holds no `a`.
EXCLUDED_DIRECTORY_ADDED = Case(
    "excluded_directory_added", ["a/x.txt"], [], ["a/x.txt"], ["a/sub/y.bat"]
)

CASES = [
    # R deletes the one kept file of `a/b`, which still holds a `.bat`.
    Case("kept_file_deleted", ["a/b/x.txt", "a/b/y.bat"], [], ["a/b/x.txt"], []),
    # The same, two levels deep, `a/b` holding only the directory `c`.
    Case(
        "nested_kept_file_deleted",
        ["a/b/c/x.txt", "a/b/c/y.bat", "a/k.bat"],
        [],
        ["a/b/c/x.txt"],
        [],
    ),
    # R deletes the kept file and the `.bat` of `a/b`, so an instance cloned at R holds no `a`.
    Case(
        "kept_and_excluded_content_deleted",
        ["a/b/x.txt", "a/b/y.bat"],
        [],
        ["a/b/x.txt", "a/b/y.bat"],
        [],
    ),
    # R empties `a/b`, which held only a `.bat`, so no instance holds `a` before or after.
    Case("excluded_content_deleted", ["a/b/y.bat"], [], ["a/b/y.bat"], []),
    # R adds a `.bat` to the empty `a/b`, so no instance holds `a` before or after.
    Case("excluded_content_added", [], ["a/b"], [], ["a/b/y.bat"]),
    # R deletes the one file of `k`, which the view keeps, so `k` stays.
    Case("kept_directory_emptied", ["k/x.txt"], [], ["k/x.txt"], []),
]


def commit_and_push(repo: Lore, files: dict[str, str]) -> None:
    """Commits and pushes `files`, each holding its content."""
    repo.write_files(files)
    repo.stage(scan=True)
    repo.commit()
    repo.push()


def revision_of(instance: Lore) -> str:
    """The revision the instance's working tree stands on."""
    return status_revisions(instance)["revision"]


def commit_case(repo: Lore, case: Case) -> str:
    """Commits R-1 on `main` and R on `feature`, then a file on `main` beside
    R-1, and answers R-1."""
    repo.write_files({path: path for path in [TOP_LEVEL] + case.files})
    for directory in case.empty:
        repo.make_dirs(directory)
    repo.stage(scan=True)
    repo.commit()
    repo.push()
    before = revision_of(repo)
    repo.branch_create("feature")
    for path in case.deleted:
        repo.remove_file(path)
    repo.write_files({path: path for path in case.added})
    repo.stage(scan=True)
    repo.commit()
    repo.push()
    repo.branch_switch("main")
    repo.write_files({OTHER_SIDE: OTHER_SIDE})
    repo.stage(scan=True)
    repo.commit()
    repo.push()
    return before


def result_tree(repo: Lore, instance: Lore, branch: str, view: str | None) -> list[str]:
    """The files and directories an instance cloned with `view` at the revision `instance`
    stands on, pushed as `branch`."""
    instance.branch_create(branch)
    instance.push()
    return working_tree_files(repo.clone(view=view, branch=branch), directories=True)


def assert_moves_leave_what_cloning_leaves(
    repo: Lore, before: str, view: str | None, cherry_picks: bool = True
) -> None:
    """A sync and a branch switch from R-1 to R leave the files and directories an
    instance cloned at R holds, a merge, a cherry-pick and a revert of R those an
    instance cloned at the revision each commits holds, and a scan after each
    reports nothing."""
    cloned: Lore = repo.clone(view=view, branch="feature")
    after = revision_of(cloned)
    synced: Lore = repo.clone(view=view, revision=f"feature@{before}")
    synced.sync()
    switched: Lore = repo.clone(view=view, revision=before)
    switched.branch_switch("feature")
    merged: Lore = repo.clone(view=view)
    merged.branch_merge("feature")
    picked: Lore = repo.clone(view=view)
    picked.revision_cherry_pick(after)
    reverted: Lore = repo.clone(view=view, branch="feature")
    reverted.revision_revert(after)
    expected = working_tree_files(cloned, directories=True)
    left = {
        "sync": (working_tree_files(synced, directories=True), expected),
        "switch": (working_tree_files(switched, directories=True), expected),
    }
    for operation, instance in [
        ("merge", merged),
        ("cherry-pick", picked),
        ("revert", reverted),
    ]:
        if instance is picked and not cherry_picks:
            continue
        tree = working_tree_files(instance, directories=True)
        left[operation] = (tree, result_tree(repo, instance, operation, view))
    differing = {
        operation: trees for operation, trees in left.items() if trees[0] != trees[1]
    }
    assert differing == {}, "each holds what it left, then what a cloned instance holds"
    for what, instance in [
        ("sync", synced),
        ("switch", switched),
        ("merge", merged),
        ("revert", reverted),
    ] + ([("cherry-pick", picked)] if cherry_picks else []):
        scanned = parse_status_json(instance.status(json=True, offline=True, scan=True))
        assert scanned == [], f"after the {what}, status --scan reported {scanned}"


@pytest.mark.smoke
@pytest.mark.parametrize("case", CASES, ids=lambda case: case.name)
def test_moving_to_another_revision_under_a_view_leaves_what_cloning_creates(
    new_lore_repo, scratch_dir, case
):
    """A sync and a branch switch from R-1 to R, and a merge, a cherry-pick and a
    revert of R, under one view leave the directories an instance cloned with that
    view holds, and a scan after each reports nothing."""
    repo: Lore = new_lore_repo()
    before = commit_case(repo, case)
    view = view_file(scratch_dir, "view", VIEW)

    assert_moves_leave_what_cloning_leaves(repo, before, view)


@pytest.mark.smoke
def test_moving_to_another_revision_under_a_view_creates_no_parent_of_what_it_excludes(
    new_lore_repo, scratch_dir
):
    """A sync, a branch switch, a merge and a revert under one view leave no `a`
    where R deletes its kept file and adds a directory holding only what the view
    excludes, as an instance cloned at R holds none. A cherry-pick under a view
    commits that directory without what the view excludes in it, so it isn't compared here."""
    repo: Lore = new_lore_repo()
    before = commit_case(repo, EXCLUDED_DIRECTORY_ADDED)
    view = view_file(scratch_dir, "view", VIEW)

    assert_moves_leave_what_cloning_leaves(repo, before, view, cherry_picks=False)


@pytest.mark.smoke
@pytest.mark.parametrize("case", CASES, ids=lambda case: case.name)
def test_moving_to_another_revision_without_a_view_leaves_what_cloning_creates(
    new_lore_repo, case
):
    """Without a view, a sync, a branch switch, a merge, a cherry-pick and a revert
    leave the directories a freshly cloned instance holds."""
    repo: Lore = new_lore_repo()
    before = commit_case(repo, case)

    assert_moves_leave_what_cloning_leaves(repo, before, None)


@pytest.mark.smoke
@pytest.mark.parametrize(
    "local, ignore",
    [("a/b/local.dat", []), ("a/b/build.log", ["*.log"])],
    ids=["untracked", "ignored"],
)
def test_a_sync_keeps_a_directory_holding_a_local_file(
    new_lore_repo, scratch_dir, local, ignore
):
    """A sync that deletes the one kept file of `a/b` keeps `a/b` where it holds an
    untracked file, or a file the ignore file leaves out."""
    repo: Lore = new_lore_repo()
    if ignore:
        repo.write_files({".loreignore": [rule + "\n" for rule in ignore]})
    before = commit_case(repo, CASES[0])
    instance: Lore = repo.clone(
        view=view_file(scratch_dir, "view", VIEW), revision=f"feature@{before}"
    )
    instance.write_files({local: "local"})

    instance.sync()

    assert local in working_tree_files(instance), f"the sync removed {local}"


@pytest.mark.smoke
def test_a_narrowing_sync_removes_an_excluded_directory_a_move_empties(
    new_lore_repo, scratch_dir
):
    """A sync that narrows the view to leave out `old`, and applies a move of the
    one file below `old/sub` out of it, leaves no `old` on disk, as an instance
    cloned at the revision with the narrowed view doesn't hold one."""
    repo: Lore = new_lore_repo()
    repo.write_files({path: path for path in [TOP_LEVEL, "old/sub/f.txt"]})
    repo.stage(scan=True)
    repo.commit()
    repo.push()
    instance: Lore = repo.clone()
    repo.make_dirs("new")
    repo.move("old/sub/f.txt", "new/f.txt")
    repo.stage_move("old/sub/f.txt", "new/f.txt")
    repo.commit()
    repo.push()
    view = view_file(scratch_dir, "narrow", ["/old/", "!/old/sub/f.txt"])

    instance.sync(view=view)

    assert working_tree_files(instance, directories=True) == working_tree_files(
        repo.clone(view=view), directories=True
    )


@pytest.mark.smoke
@pytest.mark.parametrize(
    "case",
    [case for case in CASES if case.name != "excluded_content_added"],
    ids=lambda case: case.name,
)
def test_a_reset_sync_under_a_view_leaves_what_cloning_creates(
    new_lore_repo, scratch_dir, case
):
    """A sync that resets the working tree from R-1 to R under one view removes a
    directory the view excludes that R leaves holding nothing it keeps, as an
    instance cloned at R holds none."""
    repo: Lore = new_lore_repo()
    before = commit_case(repo, case)
    view = view_file(scratch_dir, "view", VIEW)
    instance: Lore = repo.clone(view=view, revision=f"feature@{before}")

    instance.sync(reset=True)

    assert working_tree_files(instance, directories=True) == working_tree_files(
        repo.clone(view=view, branch="feature"), directories=True
    )


@pytest.mark.smoke
@pytest.mark.parametrize("case", CASES, ids=lambda case: case.name)
def test_an_aborted_merge_under_a_view_leaves_the_tree_it_started_from(
    new_lore_repo, scratch_dir, case
):
    """A merge of R into `main` under one view, aborted, leaves the files and
    directories the working tree held before it started, and a scan reports
    nothing."""
    repo: Lore = new_lore_repo()
    commit_case(repo, case)
    instance: Lore = repo.clone(view=view_file(scratch_dir, "view", VIEW))
    started_from = working_tree_files(instance, directories=True)

    instance.branch_merge_start("feature", no_commit=True)
    instance.branch_merge_abort()

    assert working_tree_files(instance, directories=True) == started_from
    scanned = parse_status_json(instance.status(json=True, offline=True, scan=True))
    assert scanned == [], f"after the abort, status --scan reported {scanned}"


@pytest.mark.own_vfs("links a repository, which a virtual instance does not hold")
@pytest.mark.smoke
def test_a_merge_under_a_view_keeps_a_link_mount(new_lore_repo, scratch_dir):
    """A merge that pins a link to a revision deleting the one file the view keeps
    in it keeps the link's mount, which stands for the link whatever the view."""
    repo: Lore = new_lore_repo()
    source: Lore = new_lore_repo(repo.name + "_source")
    commit_and_push(source, {"x.txt": "x", "y.bat": "y"})
    commit_and_push(repo, {TOP_LEVEL: TOP_LEVEL})
    repo.link_add("lnk", source.get_id(), "/")
    repo.commit("Add link")
    repo.push()
    repo.branch_create("feature")
    source.remove_file("x.txt")
    source.stage(scan=True)
    source.commit()
    source.push()
    repo.link_update("lnk", pin=revision_of(source))
    repo.commit("Update link")
    repo.push()
    repo.branch_switch("main")
    commit_and_push(repo, {OTHER_SIDE: OTHER_SIDE})
    view = view_file(scratch_dir, "view", VIEW)
    merged: Lore = repo.clone(view=view)

    merged.branch_merge("feature")

    assert "lnk" in working_tree_files(merged, directories=True)


@pytest.mark.own_vfs("layers a repository, which a virtual instance does not hold")
@pytest.mark.smoke
@pytest.mark.parametrize("reset", [False, True], ids=["sync", "reset"])
@pytest.mark.parametrize(
    "files, deleted",
    [
        (["sub/x.txt", "sub/y.bat", "k.txt"], ["sub/x.txt"]),
        (["sub/y.bat", "k.txt"], ["sub/y.bat"]),
    ],
    ids=["kept_file_deleted", "excluded_content_deleted"],
)
def test_a_layer_sync_under_a_view_leaves_what_layering_creates(
    new_lore_repo, scratch_dir, files, deleted, reset
):
    """A sync that carries a layer to its source's next revision under one view,
    resetting the working tree or not, leaves the directories below the mount that
    layering that revision afresh creates."""
    repo: Lore = new_lore_repo()
    source: Lore = new_lore_repo(repo.name + "_source")
    commit_and_push(source, {path: path for path in files})
    commit_and_push(repo, {TOP_LEVEL: TOP_LEVEL})
    view = view_file(scratch_dir, "view", VIEW)
    instance: Lore = repo.clone(view=view)
    instance.layer_add("layer", source, "/")
    for path in deleted:
        source.remove_file(path)
    source.stage(scan=True)
    source.commit()
    source.push()

    instance.sync(force=True, reset=reset)

    layered: Lore = repo.clone(view=view)
    layered.layer_add("layer", source, "/")
    assert working_tree_files(instance, directories=True) == working_tree_files(
        layered, directories=True
    )


@pytest.mark.smoke
def test_a_reset_sync_under_a_view_removes_a_directory_above_one_it_deletes(
    new_lore_repo, scratch_dir
):
    """A sync that resets the working tree under one view to a revision deleting
    `a/b/c`, beside a file the view excludes in `a/b`, removes `a` and `a/b`, as an
    instance cloned at that revision with the view holds neither."""
    repo: Lore = new_lore_repo()
    commit_and_push(repo, {TOP_LEVEL: TOP_LEVEL, "a/b/c/x.txt": "x", "a/b/y.bat": "y"})
    before = revision_of(repo)
    repo.rmtree("a/b/c")
    repo.stage(scan=True)
    repo.commit()
    repo.push()
    view = view_file(scratch_dir, "view", VIEW)
    instance: Lore = repo.clone(view=view, revision=before)

    instance.sync(reset=True)

    assert working_tree_files(instance, directories=True) == working_tree_files(
        repo.clone(view=view), directories=True
    )
