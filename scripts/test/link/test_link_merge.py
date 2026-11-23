# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import os
import re
import stat
import sys

import pytest
from error_types import (
    ImproperArgumentsError,
    LocalChanges,
    LoreException,
    NotInProgress,
    NotSupportedError,
    UnresolvedConflictError,
)
from link_helpers import (
    DEFAULT_PARENT_FILE,
    SHARED_FOLDER,
    add_a_link,
    add_inside_the_folder,
    assert_crr_clean,
    assert_holds_folder,
    assert_holds_link,
    assert_merge_settled,
    assert_mount_exposes,
    assert_mount_source,
    change_inside_the_folder,
    committed_link_pin,
    delete_inside_the_folder,
    delete_the_folder,
    fail_unless_holds,
    folder_replaced_by_a_link,
    link_entry,
    link_field,
    link_pin,
    link_pins,
    in_the_shared_mount,
    link_replaced_by_its_folder,
    make_parent_with_link,
    make_parent_with_remount,
    make_repo,
    make_repo_renumbering_folder,
    mounted_files,
    remote_branch_entries,
    remote_branch_names,
    remount,
    replace_folder_with_a_link,
    replace_link_with_folder,
)
from lore_parsers import parse_jsonl, parse_status_json
from test_utils import (
    registered_links,
    unstaged_entries,
    view_file,
    working_tree_files,
)

from lore import Lore

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)

# ---------------------------------------------------------------------------
# Links crossing branches: the link registry travels with the merge.
#
# A link lives both as a link node in the tree and as a `LinkReference` in the
# state's link registry. A merge that carries the node has to carry the entry,
# or the link lands in a state whose registry never mentions it.
# ---------------------------------------------------------------------------


def _link_added_on_feature_branch(
    new_lore_repo,
    link_path: str,
    outer: str = "",
    link_file: str | bytes = "link source content\n",
    link_files: dict | None = None,
    **link_add_options,
) -> tuple[Lore, Lore]:
    """Parent repo on `main`, link added and committed on `feature-branch` only, to a repository
    holding `link_files`, or `link_file` as `link-file.txt`. `outer` links a repository there
    first, which `link_path` is in."""
    if outer:
        repo, _ = make_parent_with_link(new_lore_repo, outer)
    else:
        repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, link_files or {"link-file.txt": link_file})

    repo.branch_create("feature-branch")
    repo.link_add(link_path, source_repo.get_id(), "/", **link_add_options)
    repo.commit("Add link on feature branch")
    repo.push()

    return repo, source_repo


@pytest.mark.smoke
def test_link_add_on_branch_merge_start(new_lore_repo):
    """A link added on a branch survives `branch merge start` into main."""
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_feature_branch(new_lore_repo, link_path)

    repo.branch_switch("main")
    assert not repo.path_exists(link_path), (
        "Link path should not exist on main before the merge"
    )

    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    assert_crr_clean(
        repo,
        expected_files_present=["main-file.txt", f"{link_path}/link-file.txt"],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )

    # The merged link tracks the branch it landed on, not the source branch.
    link_output = repo.link_list()
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Branch:\s+main", link_output, re.DOTALL
    ), f"Merged link should track 'main'.\nGot: {link_output}"

    # A fresh clone of main resolves the same link.
    clone = repo.clone()
    clone_link_output = clone.link_list()
    assert source_repo.get_id() in clone_link_output, (
        f"Merged link should be listed in a fresh clone.\nGot: {clone_link_output}"
    )
    assert clone.file_exists(f"{link_path}/link-file.txt"), (
        "Linked content should be cloned from the merged revision"
    )


@pytest.mark.smoke
def test_link_add_on_branch_merge_into(new_lore_repo):
    """A link added on a branch survives `branch merge into` main."""
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_feature_branch(new_lore_repo, link_path)

    repo.branch_merge_into("main", message="Merge feature-branch into main")
    repo.branch_switch("main")

    assert_crr_clean(
        repo,
        expected_files_present=["main-file.txt", f"{link_path}/link-file.txt"],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )


@pytest.mark.smoke
@pytest.mark.parametrize("merge_into", [False, True])
def test_link_add_merged_into_another_branch_pushes(new_lore_repo, merge_into):
    """A link merged onto a branch it was not added on follows that branch and pushes.

    The link follows its parent's branch, which only `link add` and `branch create` give a
    counterpart in the linked repository, so the merge has to give it one.
    """
    link_path = "linked/repo"
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(
        new_lore_repo, {"shared/link-file.txt": "link source content\n"}
    )

    repo.branch_create("release")
    repo.write_commit_push("Release change", {"release-file.txt": "release\n"})
    repo.branch_switch("main")
    repo.branch_create("feature-branch")
    repo.link_add(link_path, source_repo.get_id(), "shared")
    repo.commit("Add link on feature-branch")
    repo.push()

    if merge_into:
        repo.branch_merge_start("release", message="Merge release into feature-branch")
        repo.push()
        repo.branch_merge_into("release", message="Merge feature-branch into release")
        repo.branch_switch("release")
    else:
        repo.branch_switch("release")
        repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.write_commit_push("Change on release", {"after-merge.txt": "after\n"})

    link_output = repo.link_list()
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Branch:\s+release",
        link_output,
        re.DOTALL,
    ), f"Merged link should track 'release'.\nGot: {link_output}"
    clone = repo.clone(branch="release")
    assert clone.file_exists(f"{link_path}/link-file.txt"), (
        "A fresh clone of the merged branch should expose the linked content"
    )


@pytest.mark.smoke
def test_link_add_merged_with_ignore_links_leaves_linked_branches(new_lore_repo):
    """`--ignore-links` creates no branch in a linked repository a merge brings in, and warns
    that the repository was left unmerged."""
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})

    repo.branch_create("feature-branch")
    repo.write_commit_push("Feature change", {"feature-file.txt": "feature\n"})
    repo.branch_switch("main")
    repo.link_add("linked/repo", source_repo.get_id(), "/")
    repo.commit("Add link on main")
    repo.push()

    repo.branch_switch("feature-branch")
    output = repo.branch_merge_start("main", message="Merge main", ignore_links=True)

    branches = remote_branch_names(source_repo)
    assert branches == ["main"], (
        f"The linked repository should still hold only 'main'.\nGot: {branches}"
    )
    _assert_warns_once(output)


@pytest.mark.smoke
def test_link_add_merged_into_with_ignore_links_leaves_its_repository(new_lore_repo):
    """`merge into --ignore-links` leaves the repository of a link it brings in unmerged, and
    warns of it."""
    repo, source_repo = _link_added_on_feature_branch(new_lore_repo, "linked/repo")
    repo.write_commit_push(
        "Edit inside the link", {"linked/repo/link-file.txt": "edit\n"}
    )
    linked_main = source_repo.branch_info("main").remote_latest

    output = repo.branch_merge_into("main", message="Merge", ignore_links=True)

    assert source_repo.branch_info("main").remote_latest == linked_main
    _assert_warns_once(output)


def _assert_warns_once(output: str):
    warnings = [line for line in output.splitlines() if line.startswith("[Warn]")]
    assert len(warnings) == 1, f"Expected one warning for the link.\nGot: {output}"


def _link_added_on_branch_and_main_moved(
    new_lore_repo,
    link_path: str,
    feature_edit: str | bytes | None,
    linked_main_files: dict,
    outer: str = "",
    link_file: str | bytes = "link source content\n",
    link_files: dict | None = None,
) -> tuple[Lore, Lore]:
    """`feature-branch` adds a following link to a repository holding `link_file` and, given
    `feature_edit`, commits it to `link-file.txt` through the mount. `linked_main_files` are
    committed to the linked repository's `main` and a change to the parent's, which is left
    checked out. `outer` adds the link in a link mounted there, which main changes."""
    repo, source_repo = _link_added_on_feature_branch(
        new_lore_repo, link_path, outer, link_file, link_files
    )
    if feature_edit:
        repo.write_commit_push(
            "Edit inside the link", {f"{link_path}/link-file.txt": feature_edit}
        )
    if linked_main_files:
        source_repo.write_commit_push("Linked main moves on", linked_main_files)
    repo.branch_switch("main")
    repo.write_commit_push(
        "Change on main", {f"{outer}/root.txt".lstrip("/"): "root\n"}
    )
    return repo, source_repo


def _merge_main_into_feature(repo: Lore):
    repo.branch_switch("feature-branch")
    repo.branch_merge_start("main", message="Merge main")
    repo.push()


def _assert_conflict_marked(repo: Lore, path: str):
    with repo.open_file(path) as f:
        assert "<<<<<<<" in f.read(), f"{path} should hold conflict markers"


def _sorted_by_name(entries: list[dict]) -> list[dict]:
    return sorted(entries, key=lambda entry: entry["name"])


def _assert_pinned_at_linked_main(repo: Lore, source_repo: Lore):
    assert link_pin(repo, source_repo.get_id()) == (
        source_repo.branch_info("main").remote_latest
    ), "The link should pin the latest of the linked repository's main"


@pytest.mark.smoke
@pytest.mark.parametrize(
    "merge_into, feature_edit, linked_main_files, outer",
    [
        (False, "edited on feature\n", {}, ""),
        (False, None, {"main-only.txt": "main\n"}, ""),
        (True, "edited on feature\n", {"main-only.txt": "main\n"}, ""),
        (False, "edited on feature\n", {"main-only.txt": "main\n"}, "outer"),
    ],
    ids=["edited", "linked main moved", "merge into", "nested"],
)
def test_link_added_on_branch_merges_its_repository(
    new_lore_repo, merge_into, feature_edit, linked_main_files, outer
):
    """A following link a merged branch adds is merged in its repository's own branch, which the
    link lands pinned at, so a commit inside the link afterwards pushes. `outer` adds the link in
    a link mounted there."""
    link_path = f"{outer}/linked/repo".lstrip("/")
    repo, source_repo = _link_added_on_branch_and_main_moved(
        new_lore_repo,
        link_path,
        feature_edit,
        {} if merge_into else linked_main_files,
        outer,
    )

    if merge_into:
        _merge_main_into_feature(repo)
        source_repo.write_commit_push("Linked main moves on", linked_main_files)
        repo.branch_merge_into("main", message="Merge feature-branch into main")
        repo.branch_switch("main")
    else:
        repo.branch_merge_start("feature-branch", message="Merge feature-branch")
        repo.push()

    _assert_pinned_at_linked_main(repo, source_repo)
    assert mounted_files(repo, link_path) == {
        "link-file.txt": feature_edit or "link source content\n",
        **linked_main_files,
    }

    repo.write_commit_push(
        "Edit inside the link on main", {f"{link_path}/link-file.txt": "on main\n"}
    )
    _assert_pinned_at_linked_main(repo, source_repo)


@pytest.mark.smoke
def test_link_added_on_branch_to_a_repository_linked_already_shares_its_pin(
    new_lore_repo,
):
    """A following link a merged branch adds to a repository the target links already lands at
    the pin of the target's link, which one repository's links share, however the repository's
    own branch moved on."""
    repo, source_repo = make_parent_with_link(new_lore_repo, "linked/a")
    pin = link_pin(repo, source_repo.get_id())
    repo.branch_create("feature-branch")
    repo.link_add("linked/b", source_repo.get_id(), "/")
    repo.commit("Add a second link on feature branch")
    repo.push()
    source_repo.write_commit_push("Linked main moves on", {"main-only.txt": "main\n"})
    repo.branch_switch("main")
    repo.write_commit_push("Change on main", {"root.txt": "root\n"})

    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    assert link_pins(repo.clone(), source_repo.get_id()) == [pin, pin]
    assert mounted_files(repo, "linked/b") == {"linked.txt": "linked content\n"}


@pytest.mark.smoke
def test_link_added_on_branch_with_conflicting_edits_merges_once_resolved(
    new_lore_repo,
):
    """Edits a merged branch makes inside a link it adds stop the merge where they conflict with
    the linked repository's own branch. Once resolved, starting the merge again is refused, and
    the merge pushes the linked repository's merge with it."""
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_branch_and_main_moved(
        new_lore_repo,
        link_path,
        "edited on feature\n",
        {"link-file.txt": "edited on linked main\n"},
    )

    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    conflicted = f"{link_path}/link-file.txt"
    _assert_conflict_marked(repo, conflicted)

    repo.write_files({conflicted: "resolved\n"})
    repo.branch_merge_resolve(conflicted)
    with pytest.raises(NotSupportedError):
        repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.commit("Merge feature-branch")
    repo.push()

    _assert_pinned_at_linked_main(repo, source_repo)
    assert mounted_files(repo.clone(), link_path) == {"link-file.txt": "resolved\n"}


def _link_edited_on_both_sides(
    new_lore_repo,
    sides: dict[str, tuple[str | bytes, str | bytes, str | bytes]],
    executable_on: tuple[str, ...] = (),
) -> tuple[Lore, Lore]:
    """`feature-branch` adds a following link to a repository holding each file of `sides` at its
    base, with main merged in, as `merge into` requires, and commits each file's feature edit
    through the mount. The linked repository's `main` then commits each file's main edit. Each
    side `executable_on` names, `"feature"` or `"linked main"`, gives `link-file.txt` the
    executable bit with its edit."""
    repo, source_repo = _link_added_on_branch_and_main_moved(
        new_lore_repo,
        "linked/repo",
        None,
        {},
        link_files={name: base for name, (base, _, _) in sides.items()},
    )
    _merge_main_into_feature(repo)
    repo.write_files(
        {f"linked/repo/{name}": edit for name, (_, edit, _) in sides.items()}
    )
    if "feature" in executable_on:
        os.chmod(os.path.join(repo.path, "linked/repo/link-file.txt"), 0o755)
    repo.stage([f"linked/repo/{name}" for name in sides])
    repo.commit("Edit inside the link")
    repo.push()
    source_repo.write_files({name: edit for name, (_, _, edit) in sides.items()})
    if "linked main" in executable_on:
        os.chmod(os.path.join(source_repo.path, "link-file.txt"), 0o755)
    source_repo.stage(list(sides))
    source_repo.commit("Linked main moves on")
    source_repo.push()
    return repo, source_repo


def _is_executable(repo: Lore, path: str) -> bool:
    return bool(os.stat(os.path.join(repo.path, path)).st_mode & stat.S_IXUSR)


# Edits on different lines, as base, feature and linked main, and how they merge.
_LINK_FILE = ("one\ntwo\nthree\n", "feature\ntwo\nthree\n", "one\ntwo\nmain\n")
_OTHER_FILE = ("a\nb\nc\n", "feature\nb\nc\n", "a\nb\nmain\n")
_MERGED = {"link-file.txt": "feature\ntwo\nmain\n", "other.txt": "feature\nb\nmain\n"}


@pytest.mark.smoke
@pytest.mark.parametrize(
    "executable_on",
    [
        (),
        *(
            pytest.param(
                sides,
                marks=pytest.mark.skipif(
                    sys.platform == "win32", reason="Windows holds no executable bit"
                ),
            )
            for sides in [("feature",), ("linked main",)]
        ),
    ],
    ids=["text", "executable bit on feature", "executable bit on linked main"],
)
def test_link_added_on_branch_with_edits_that_merge_cleanly_merges_into(
    new_lore_repo, executable_on
):
    """`merge into` merges each file both sides changed in a linked repository the merged branch
    adds where its text merges cleanly, with the executable bit either side gives it, and pins
    the merge on the target. A dry run of it pushes neither repository."""
    link_path = "linked/repo"
    repo, source_repo = _link_edited_on_both_sides(
        new_lore_repo,
        {"link-file.txt": _LINK_FILE, "other.txt": _OTHER_FILE},
        executable_on,
    )
    feature_files = mounted_files(repo, link_path)
    if not executable_on:
        parent_main = repo.branch_info("main").remote_latest
        linked_main = source_repo.branch_info("main").remote_latest
        repo.branch_merge_into(
            "main", message="Merge feature-branch into main", dry_run=True
        )
        assert repo.branch_info("main").remote_latest == parent_main
        assert source_repo.branch_info("main").remote_latest == linked_main

    repo.branch_merge_into("main", message="Merge feature-branch into main")

    assert mounted_files(repo, link_path) == feature_files
    source_repo.sync()
    assert source_repo.revision_info().merge, "Linked main should take a merge"
    assert mounted_files(source_repo, "") == _MERGED
    clone = repo.clone(branch="main")
    _assert_pinned_at_linked_main(clone, source_repo)
    assert mounted_files(clone, link_path) == _MERGED
    assert _is_executable(clone, f"{link_path}/link-file.txt") == bool(executable_on)

    repo.branch_switch("main")
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    assert parse_status_json(repo.status(json=True)) == []
    assert mounted_files(repo, link_path) == _MERGED


@pytest.mark.smoke
@pytest.mark.parametrize(
    "sides",
    [
        {
            "link-file.txt": (
                "source\n",
                "edited on feature\n",
                "edited on linked main\n",
            )
        },
        {
            "link-file.txt": (
                b"{\\rtf1\none\ntwo\nthree\n",
                b"{\\rtf1\nfeature\ntwo\nthree\n",
                b"{\\rtf1\none\ntwo\nmain\n",
            )
        },
        {
            "link-file.txt": (
                b"{\\rtf1\none\ntwo\nthree\nfour\nfive\n",
                b"text\none\nfeature\nthree\nfour\nfive\n",
                b"text\none\ntwo\nthree\nmain\nfive\n",
            )
        },
        {
            "link-file.txt": _LINK_FILE,
            "other.txt": ("a\nb\nc\n", "feature\nb\nc\n", "main\nb\nc\n"),
        },
    ],
    ids=[
        "same line",
        "not text",
        "base not text",
        "one of two on the same line",
    ],
)
def test_link_added_on_branch_with_conflicting_edits_refuses_merge_into(
    new_lore_repo, sides
):
    """`merge into` refuses a merge whose linked repository holds a conflict it does not merge as
    text, though another merges cleanly, before pushing either."""
    repo, source_repo = _link_edited_on_both_sides(new_lore_repo, sides)
    parent_main = repo.branch_info("main").remote_latest
    linked_main = source_repo.branch_info("main").remote_latest

    with pytest.raises(NotSupportedError):
        repo.branch_merge_into("main", message="Merge feature-branch into main")

    assert repo.branch_info("main").remote_latest == parent_main
    assert source_repo.branch_info("main").remote_latest == linked_main


@pytest.mark.smoke
def test_link_added_on_branch_merge_dry_run_writes_nothing(new_lore_repo):
    """A dry run of a merge bringing in a following link leaves the working tree, the staged
    state and the linked repository as they were."""
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_branch_and_main_moved(
        new_lore_repo, link_path, "edited on feature\n", {"main-only.txt": "main\n"}
    )
    linked_branches = _sorted_by_name(remote_branch_entries(source_repo))
    linked_main = source_repo.branch_info("main").remote_latest

    repo.branch_merge_start(
        "feature-branch", message="Merge feature-branch", dry_run=True
    )

    assert not repo.path_exists(link_path)
    assert parse_status_json(repo.status(json=True)) == []
    assert _sorted_by_name(remote_branch_entries(source_repo)) == linked_branches
    assert source_repo.branch_info("main").remote_latest == linked_main


@pytest.mark.smoke
@pytest.mark.parametrize("ignore_links", [False, True])
def test_link_added_on_branch_merge_abort_restores_the_working_tree(
    new_lore_repo, ignore_links
):
    """Aborting a merge stopped on a conflict inside a link the merged branch adds removes the
    link with the merge."""
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_branch_and_main_moved(
        new_lore_repo,
        link_path,
        "edited on feature\n",
        {"link-file.txt": "edited on linked main\n"},
    )
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    _assert_conflict_marked(repo, f"{link_path}/link-file.txt")

    repo.branch_merge_abort(ignore_links=ignore_links)

    assert registered_links(repo) == {}
    assert not repo.path_exists(link_path)
    assert parse_status_json(repo.status(json=True)) == []


@pytest.mark.smoke
def test_link_added_on_branch_merge_abort_of_the_link_lands_it_on_linked_main(
    new_lore_repo,
):
    """Aborting only the merge of a link the merged branch adds leaves the link at the latest of
    the linked repository's own branch, which the merge was staged on."""
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_branch_and_main_moved(
        new_lore_repo,
        link_path,
        "edited on feature\n",
        {"link-file.txt": "edited on linked main\n"},
    )
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")

    repo.branch_merge_abort(link=link_path)

    assert mounted_files(repo, link_path) == {
        "link-file.txt": "edited on linked main\n"
    }
    assert link_pin(repo, source_repo.get_id()) == (
        source_repo.branch_info("main").remote_latest
    )


@pytest.mark.smoke
@pytest.mark.parametrize("merge_into", [False, True])
def test_link_remount_on_branch_merge(new_lore_repo, merge_into):
    """A remount made on a branch lands on main through `branch merge start` and `into`."""
    link_path = "linked/repo"
    repo, _source_repo = make_parent_with_remount(
        new_lore_repo,
        {"first/first.txt": "first\n", "second/second.txt": "second\n"},
        "first",
        "second",
        link_path,
    )

    if merge_into:
        repo.branch_merge_into("main", message="Merge feature-branch into main")
        repo.branch_switch("main")
    else:
        repo.branch_switch("main")
        repo.branch_merge_start("feature-branch", message="Merge feature-branch")
        repo.push()

    assert_mount_exposes(repo, link_path, "second", ["second.txt"])
    assert_mount_exposes(repo.clone(), link_path, "second", ["second.txt"])


@pytest.mark.smoke
def test_link_remount_with_content_change_on_branch_merge_start(new_lore_repo):
    """A remount lands even where the link's own merge owns the pin.

    Committing inside the link makes it eligible for `merge_repository`, which settles the pin
    before the parent's link rows are compared. The remount has to reach the parent anyway.
    """
    link_path = "linked/repo"
    repo, _source_repo = make_parent_with_remount(
        new_lore_repo,
        {"first/first.txt": "first\n", "second/second.txt": "second\n"},
        "first",
        "second",
        link_path,
    )

    with repo.open_file(f"{link_path}/second.txt", "w+") as f:
        f.writelines(["edited through the remounted subtree\n"])
    repo.stage(scan=True)
    repo.commit("Edit inside the link on feature branch")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    assert_mount_source(repo, link_path, "second")
    assert_mount_exposes(repo.clone(), link_path, "second", ["second.txt"])


@pytest.mark.smoke
@pytest.mark.parametrize("link_scoped", [False, True])
def test_link_remount_with_content_change_undone_by_branch_merge_abort(
    new_lore_repo, link_scoped
):
    """Aborting a merge that remounted a link returns it to the mount it had before.

    The remount targets a subtree the merged link content introduces, which the pin the abort
    returns to does not hold, so the mount has to return with the pin.
    """
    link_path = "linked/repo"
    repo, _source_repo = make_parent_with_remount(
        new_lore_repo,
        {"first/a.txt": "a\n"},
        "/",
        "second",
        link_path,
        link_changes={"second/b.txt": "b\n"},
    )

    repo.branch_switch("main")
    repo.write_commit_push(
        "Change inside the link on main", {f"{link_path}/first/x.txt": "x\n"}
    )
    repo.branch_merge_start("feature-branch", no_commit=True)
    assert_mount_exposes(repo, link_path, "second", ["b.txt"])

    repo.branch_merge_abort(link=link_path if link_scoped else None)

    assert_mount_exposes(repo, link_path, "/", ["first"])
    assert repo.file_exists(f"{link_path}/first/x.txt"), (
        "the abort should restore main's link content"
    )
    assert unstaged_entries(repo) == [], "the working tree should match the mount"


@pytest.mark.smoke
def test_link_add_on_branch_merge_start_link_usable(new_lore_repo):
    """A merged-in link still stages and commits through its mount path.

    Without a registry entry the link node is unreachable: staging under the
    mount path fails with `Link not found`.
    """
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_feature_branch(new_lore_repo, link_path)

    repo.branch_switch("main")
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    pin_before = link_pin(repo, source_repo.get_id())

    linked_file = f"{link_path}/link-file.txt"
    with repo.open_file(linked_file, "w+") as f:
        f.writelines(["changed through the mount path after the merge\n"])

    repo.stage(scan=True)
    output = repo.commit("Change inside the merged link")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    repo.push()

    with repo.open_file(linked_file, "r") as f:
        assert "changed through the mount path after the merge" in f.read(), (
            "Committed content should remain on disk"
        )

    pin_after = link_pin(repo, source_repo.get_id())
    assert pin_after != pin_before, (
        "Committing into the merged link should advance its pin"
    )

    assert_crr_clean(
        repo,
        expected_files_present=[linked_file],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )


@pytest.mark.smoke
def test_link_add_on_branch_merge_start_preserves_link_flags(new_lore_repo):
    """A merged-in fixed link keeps its `DisableAutoFollow` flag and branch."""
    link_path = "linked/fixed"
    repo, source_repo = _link_added_on_feature_branch(
        new_lore_repo, link_path, disable_branching=True
    )

    repo.branch_switch("main")
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    assert link_field(repo, source_repo.get_id(), "Flags") == "DisableAutoFollow", (
        "Merged link should keep the DisableAutoFollow flag"
    )
    link_output = repo.link_list()
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Branch:\s+main", link_output, re.DOTALL
    ), f"Merged fixed link should keep tracking 'main'.\nGot: {link_output}"


@pytest.mark.smoke
def test_link_remove_on_branch_merge_start(new_lore_repo):
    """A link removed on a branch is gone from the registry after merging."""
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})

    link_path = "linked/repo"
    repo.link_add(link_path, source_repo.get_id(), "/")
    repo.commit("Add link on main")
    repo.push()

    # The link is removed only on the feature branch.
    repo.branch_create("feature-branch")
    repo.link_remove(link_path)
    repo.commit("Remove link on feature branch")
    repo.push()

    repo.branch_switch("main")
    assert repo.file_exists(f"{link_path}/link-file.txt"), (
        "Linked content should still be on main before the merge"
    )

    repo.branch_merge_start("feature-branch", message="Merge link removal")
    repo.push()

    assert_crr_clean(
        repo,
        expected_files_present=["main-file.txt"],
        expected_files_absent=[f"{link_path}/link-file.txt"],
        expected_link_registry={source_repo.get_id(): False, link_path: False},
    )


def _fixed_link_pinned_on_main(new_lore_repo, link_path: str) -> tuple[Lore, Lore, str]:
    """Parent on main with a fixed link, and a newer revision to move it to.

    The link is added with `--disable-branching`, so its registry entry names a
    branch of the linked repository and its pin is plain revision data that a
    merge has to carry. The linked repository then gets a second revision, so
    the pin can move with nothing staged through the mount path.
    """
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})

    repo.link_add(link_path, source_repo.get_id(), "/", disable_branching=True)
    repo.commit("Add fixed link on main")
    repo.push()

    with source_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link source revision two\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Second source revision")
    source_repo.push()

    return repo, source_repo, link_pin(repo, source_repo.get_id())


@pytest.mark.smoke
def test_link_pin_update_on_branch_merge_start(new_lore_repo):
    """A pin-only `link update` on a branch survives `branch merge start`."""
    link_path = "linked/repo"
    repo, source_repo, pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)
    linked_file = f"{link_path}/link-file.txt"

    repo.branch_create("feature-branch")
    repo.link_update(link_path)
    repo.commit("Move link pin on feature branch")
    repo.push()

    pin_feature = link_pin(repo, source_repo.get_id())
    assert pin_feature != pin_base, "link update should have moved the pin"

    repo.branch_switch("main")
    assert link_pin(repo, source_repo.get_id()) == pin_base, (
        "Main should still hold the original pin before the merge"
    )

    repo.branch_merge_start("feature-branch", message="Merge link pin update")
    repo.push()

    assert link_pin(repo, source_repo.get_id()) == pin_feature, (
        "Merge should carry the pin the merged branch moved"
    )
    with repo.open_file(linked_file, "r") as f:
        assert "revision two" in f.read(), (
            "Mount path should hold the content of the newly pinned revision"
        )

    assert_crr_clean(
        repo,
        expected_files_present=["main-file.txt", linked_file],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )


@pytest.mark.smoke
@pytest.mark.parametrize("remount_too", [False, True])
def test_link_pin_move_on_a_subtree_link_merges_into_the_registry(
    new_lore_repo, remount_too
):
    """A subtree link's pin move reaches the registry before the merge commits.

    Covers the folder being renumbered at the new pin, and the move remounting the link as well.
    """
    link_path = "linked/repo"
    source_repo, pin_one, pin_two = make_repo_renumbering_folder(new_lore_repo)
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    repo.link_add(link_path, source_repo.get_id(), "sub", pin=pin_one)
    repo.commit("Add link")
    repo.push()

    repo.branch_create("feature-branch")
    if remount_too:
        remount(repo, link_path, source_repo.get_id(), "other", pin=pin_two)
    else:
        repo.link_update(link_path, pin=pin_two)
        repo.commit("Move the pin on feature branch")
        repo.push()

    repo.branch_switch("main")
    repo.write_commit_push("Change on main", {"main-only.txt": "main\n"})
    repo.branch_merge_start("feature-branch", no_commit=True)

    assert link_pin(repo, source_repo.get_id()) == pin_two, (
        "The merge should stage the pin the merged branch moved"
    )
    if remount_too:
        assert_mount_exposes(repo, link_path, "other", ["z.txt"])
    else:
        assert_mount_exposes(repo, link_path, "sub", ["a.txt"])
        with repo.open_file(f"{link_path}/a.txt") as f:
            assert f.read() == "two\n", (
                "The mount should hold the newly pinned revision"
            )


@pytest.mark.smoke
def test_link_pin_update_on_branch_merge_into(new_lore_repo):
    """A pin-only update reaches main through `branch merge into`.

    The link's own files must stay in the linked repository: realizing them into
    the parent tree overwrites the link node's child pointer and drops the
    linked content out of the parent's tree.
    """
    link_path = "linked/repo"
    repo, source_repo, _pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)
    linked_file = f"{link_path}/link-file.txt"

    repo.branch_create("feature-branch")
    repo.link_update(link_path)
    repo.commit("Move link pin on feature branch")
    repo.push()
    pin_feature = link_pin(repo, source_repo.get_id())

    repo.branch_merge_into("main", message="Merge link pin update into main")
    repo.branch_switch("main")

    assert link_pin(repo, source_repo.get_id()) == pin_feature, (
        "merge into should carry the pin onto the target branch"
    )

    sources = [
        entry["sourcePath"]
        for entry in parse_jsonl(repo.link_list(json=True), "linkEntry")
        if entry["link"] == source_repo.get_id()
    ]
    assert sources == ["/"], (
        f"Merged link should still mount the linked repository root, mounts {sources}"
    )

    with repo.open_file(linked_file, "r") as f:
        assert "revision two" in f.read(), (
            "Mount path should hold the content of the newly pinned revision"
        )

    # A fresh clone proves the linked content is reachable through the link node
    # in the committed tree rather than only present on this working copy.
    clone = repo.clone()
    with clone.open_file(linked_file, "r") as f:
        assert "revision two" in f.read(), (
            "Fresh clone should realize the newly pinned linked content"
        )


@pytest.mark.smoke
def test_link_autofollow_pin_not_carried_by_merge_into(new_lore_repo):
    """`merge into` leaves an auto-following link's pin on the target branch.

    Such a pin points into the branch of the linked repository that mirrors the
    parent branch, so writing it onto another parent branch leaves the parent
    pinned off the mirror its row resolves to, and the next commit into the link
    builds on the wrong mirror and fails to push.
    """
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})

    link_path = "linked/repo"
    repo.link_add(link_path, source_repo.get_id(), "/")
    repo.commit("Add auto-following link on main")
    repo.push()
    pin_main = link_pin(repo, source_repo.get_id())

    # Advance the link through the mount path, which moves its pin onto the
    # linked repository's mirror of the feature branch.
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Add a file inside the link on feature branch")
    repo.push()
    assert link_pin(repo, source_repo.get_id()) != pin_main, (
        "Committing into the link should have moved its pin on the feature branch"
    )

    repo.branch_merge_into("main", message="Merge feature branch into main")
    repo.branch_switch("main")

    assert link_pin(repo, source_repo.get_id()) == pin_main, (
        "merge into should not move an auto-following link's pin across branches"
    )

    # The link stays usable on the target branch: its pin is still on main's own
    # mirror, so committing into it and pushing succeed.
    with repo.open_file(f"{link_path}/link-file.txt", "w+") as f:
        f.writelines(["changed on main after merge into\n"])
    repo.stage(scan=True)
    output = repo.commit("Change inside the link on main")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    repo.push()


def _diverge_link_pins(new_lore_repo, link_path: str) -> tuple[Lore, Lore, str, str]:
    """Feature branch pins a third source revision, main pins the second."""
    repo, source_repo, _pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)
    revision_two = source_repo.branch_info().local_latest

    with source_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link source revision three\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Third source revision")
    source_repo.push()
    revision_three = source_repo.branch_info().local_latest

    repo.branch_create("feature-branch")
    repo.link_update(link_path, pin=f"{revision_three}")
    repo.commit("Pin link to the third revision on feature branch")
    repo.push()

    repo.branch_switch("main")
    repo.link_update(link_path, pin=f"{revision_two}")
    repo.commit("Pin link to the second revision on main")
    repo.push()

    return repo, source_repo, revision_two, revision_three


_REVISION_TWO_CONTENT = "link source revision two\n"
_REVISION_THREE_CONTENT = "link source revision three\n"


def _mounted_link_file(repo: Lore, link_path: str) -> str:
    with repo.open_file(f"{link_path}/link-file.txt", "r") as f:
        return f.read()


@pytest.mark.smoke
def test_link_pin_conflict_resolve_theirs_takes_merged_branch_pin(new_lore_repo):
    """A divergent link pin holds the merge open until `theirs` settles it.

    The merge names the row, on a dry run too, the commit before the resolve is
    refused, and the one after it carries the merged branch's pin and nothing more,
    which a clone reads and realizes.
    """
    link_path = "linked/repo"
    repo, source_repo, _revision_two, revision_three = _diverge_link_pins(
        new_lore_repo, link_path
    )

    output = repo.branch_merge_start(
        "feature-branch", message="Merge divergent link pins", dry_run=True
    )
    assert f"Link conflict: {link_path}" in output, (
        f"A dry run should name the divergent row - Got:\n{output}"
    )

    output = repo.branch_merge_start(
        "feature-branch", message="Merge divergent link pins"
    )
    assert f"Link conflict: {link_path}" in output, (
        f"The merge should name the divergent row - Got:\n{output}"
    )

    with pytest.raises(UnresolvedConflictError):
        repo.commit("Commit with the link pin still in conflict")

    output = repo.branch_merge_resolve_theirs(link_path)
    assert f"{link_path} (theirs)" in output, (
        f"The resolve should report the side it settled the row to - Got:\n{output}"
    )
    assert _mounted_link_file(repo, link_path) == _REVISION_THREE_CONTENT, (
        "Resolving theirs should realize the merged branch's revision at the mount"
    )

    output = repo.commit("Merge divergent link pins")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    assert link_pin(repo, source_repo.get_id()) == revision_three, (
        "The committed pin should be the revision the merged branch held"
    )
    repo.push()

    clone = repo.clone()
    assert link_pin(clone, source_repo.get_id()) == revision_three, (
        "A clone should read the resolved pin from the pushed revision"
    )
    assert _mounted_link_file(clone, link_path) == _REVISION_THREE_CONTENT, (
        "A clone should realize the resolved revision at the mount"
    )
    assert "No conflicts" in clone.branch_merge_conflicts_list(), (
        "A clone should find no merge state in the revision"
    )


@pytest.mark.smoke
def test_link_pin_conflict_is_not_read_as_an_already_merged_link(new_lore_repo):
    """A recorded pin conflict is outstanding work, not a link a prior pass merged.

    `--ignore-links` records the divergent row and leaves the merge open. A
    following `merge start` has to report that merge rather than read the row as
    a link it already merged and skip it.
    """
    link_path = "linked/repo"
    repo, source_repo, revision_two, _revision_three = _diverge_link_pins(
        new_lore_repo, link_path
    )

    repo.branch_merge_start(
        "feature-branch", message="Merge divergent link pins", ignore_links=True
    )

    output = repo.branch_merge_start(
        "feature-branch", message="Merge again", check=False
    )
    assert "already in progress" in output, (
        f"A second merge start should report the open merge - Got:\n{output}"
    )

    repo.branch_merge_resolve_mine(link_path)
    output = repo.commit("Merge divergent link pins")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    assert link_pin(repo, source_repo.get_id()) == revision_two, (
        "The committed pin should be the row this branch held"
    )


@pytest.mark.smoke
def test_merge_conflicts_list_names_a_branch_change(new_lore_repo):
    """Sides tracking different branches of the linked repository read as that."""
    link_path = "linked/repo"
    repo, source_repo, _pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)
    revision_two = source_repo.branch_info().local_latest

    source_repo.branch_create("other")
    with source_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link source on another branch\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Source revision on another branch")
    source_repo.push()

    repo.branch_create("feature-branch")
    repo.link_update(link_path, pin="other@LATEST")
    repo.commit("Track the other source branch on feature branch")
    repo.push()

    repo.branch_switch("main")
    repo.link_update(link_path, pin=f"{revision_two}")
    repo.commit("Pin link to the second revision on main")
    repo.push()

    repo.branch_merge_start("feature-branch", message="Merge divergent link rows")
    output = repo.branch_merge_conflicts_list()

    assert "branch change" in output, (
        f"The kind should name the branch change - Got:\n{output}"
    )
    assert "Branch: other" in output, (
        f"The branch the merged side tracks should be listed - Got:\n{output}"
    )
    assert "Branch: main" in output, (
        f"The branch this side tracks should be listed - Got:\n{output}"
    )


@pytest.mark.smoke
def test_merge_conflicts_list_follows_file_and_link_conflicts_until_settled(
    new_lore_repo,
):
    """`merge conflicts list` describes each conflict until a resolve settles it.

    The file conflict stays listed while the link row is settled through the
    folder holding its mount, so the row leaving the list is that resolve landing
    rather than a walk that found nothing. Plain `merge resolve` keeps the side a
    row was settled to, and a row can be settled again the other way.
    """
    link_path = "linked/repo"
    repo, source_repo, revision_two, revision_three = _diverge_link_pins(
        new_lore_repo, link_path
    )

    repo.branch_switch("feature-branch")
    with repo.open_file("shared.txt", "w+") as f:
        f.writelines(["feature\n"])
    repo.stage(scan=True)
    repo.commit("Feature edits shared.txt")
    repo.push()

    repo.branch_switch("main")
    with repo.open_file("shared.txt", "w+") as f:
        f.writelines(["main\n"])
    repo.stage(scan=True)
    repo.commit("Main edits shared.txt")
    repo.push()

    repo.branch_merge_start(
        "feature-branch", message="Merge divergent link pins", no_commit=True
    )

    output = repo.branch_merge_conflicts_list()
    assert "shared.txt" in output, (
        f"The conflicted file should be listed - Got:\n{output}"
    )
    assert f"Link conflict: {link_path} (revision divergence)" in output, (
        f"The link row should be listed with its kind - Got:\n{output}"
    )
    assert revision_two in output and revision_three in output, (
        f"Both sides' revisions should be listed - Got:\n{output}"
    )
    assert "Second source revision" in output and "Third source revision" in output, (
        f"The message of the revision each side pins should be listed - Got:\n{output}"
    )

    events = [
        json.loads(line)
        for line in repo.branch_merge_conflicts_list(json=True).strip().splitlines()
    ]
    assert [
        event["data"]["linkPath"]
        for event in events
        if event["tagName"] == "branchMergeLinkPinConflict"
    ] == [link_path], f"JSON output should carry the link row alone - Got:\n{events}"

    output = repo.branch_merge_resolve_theirs("linked")
    assert f"{link_path} (theirs)" in output, (
        f"A folder holding the mount should settle its row - Got:\n{output}"
    )

    output = repo.branch_merge_conflicts_list()
    assert "shared.txt" in output, (
        f"The file still needing a side should stay listed - Got:\n{output}"
    )
    assert link_path not in output, (
        f"The settled link row should have left the list - Got:\n{output}"
    )

    with repo.open_file("shared.txt", "w+") as f:
        f.writelines(["resolved by hand\n"])
    repo.branch_merge_resolve(["shared.txt", link_path])
    assert _mounted_link_file(repo, link_path) == _REVISION_THREE_CONTENT, (
        "A plain resolve should keep the side the row was settled to"
    )

    output = repo.branch_merge_resolve_mine(link_path)
    assert f"{link_path} (mine)" in output, (
        f"The resolve should report the side it settled the row to - Got:\n{output}"
    )
    assert _mounted_link_file(repo, link_path) == _REVISION_TWO_CONTENT, (
        "Settling again should realize this branch's revision at the mount"
    )

    output = repo.commit("Merge divergent link pins")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    assert link_pin(repo, source_repo.get_id()) == revision_two, (
        "The committed pin should be the one the last resolve chose"
    )
    with repo.open_file("shared.txt", "r") as f:
        assert f.read() == "resolved by hand\n", (
            "The file should keep the content the resolve settled on"
        )


def _link_readded_fixed_on_feature(
    new_lore_repo, link_path: str, fixed: bool = True
) -> tuple[Lore, Lore, str]:
    """A link on main, re-added at a newer revision on a branch, fixed there where `fixed`
    and following there otherwise."""
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})
    repo.link_add(link_path, source_repo.get_id(), "/", disable_branching=not fixed)
    repo.commit("Add the link", non_interactive=True)
    repo.push()

    source_repo.write_commit_push("Second source revision", {"link-file.txt": "two\n"})
    revision_two = source_repo.branch_info().local_latest
    repo.branch_create("feature-branch")
    repo.link_remove(link_path)
    repo.commit("Remove the link", non_interactive=True)
    repo.push()
    repo.link_add(link_path, source_repo.get_id(), "/", disable_branching=fixed)
    repo.commit("Re-add the link", non_interactive=True)
    repo.push()

    repo.branch_switch("main")
    return repo, source_repo, revision_two


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("fixed", "flags"),
    [(True, "DisableAutoFollow"), (False, "None")],
    ids=["fixed", "unfixed"],
)
def test_link_row_flags_travel_with_a_one_sided_merge(new_lore_repo, fixed, flags):
    """A row only the merged branch changed lands with its flags, not just its pin,
    whether the merged branch made the link fixed or following."""
    link_path = "linked/repo"
    repo, source_repo, revision_two = _link_readded_fixed_on_feature(
        new_lore_repo, link_path, fixed
    )

    repo.branch_merge_start(
        "feature-branch", message="Merge the re-added link", non_interactive=True
    )

    assert link_pin(repo, source_repo.get_id()) == revision_two, (
        "The merge should carry the merged branch's pin"
    )
    assert link_field(repo, source_repo.get_id(), "Flags") == flags, (
        "The merge should carry the merged branch's flags with its pin"
    )


@pytest.mark.smoke
def test_link_conflict_abort_of_one_link_keeps_the_others(new_lore_repo):
    """Aborting one link of a merge restores that link and leaves the other rows to settle."""
    repo = make_repo(new_lore_repo, {"main-file.txt": "main\n"})
    sources = {
        path: make_repo(new_lore_repo, {"file.txt": "one\n"})
        for path in ("linked/first", "linked/second")
    }
    for path, source in sources.items():
        repo.link_add(path, source.get_id(), "/", disable_branching=True)
    repo.commit("Add the links")
    repo.push()
    pins = {}
    for path, source in sources.items():
        source.write_commit_push("Second revision", {"file.txt": "two\n"})
        two = source.branch_info().local_latest
        source.write_commit_push("Third revision", {"file.txt": "three\n"})
        pins[path] = (two, source.branch_info().local_latest)

    repo.branch_create("feature-branch")
    for path, (_two, three) in pins.items():
        repo.link_update(path, pin=three)
    repo.commit("Pin the links to their third revisions")
    repo.push()
    repo.branch_switch("main")
    for path, (two, _three) in pins.items():
        repo.link_update(path, pin=two)
    repo.commit("Pin the links to their second revisions")
    repo.push()
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")

    repo.branch_merge_abort(link="linked/first")

    assert (
        link_pin(repo, sources["linked/first"].get_id()) == pins["linked/first"][0]
    ), "Aborting the first link should restore the row this branch held"
    listed = repo.branch_merge_conflicts_list()
    assert "Link conflict: linked/second" in listed, (
        f"The other link's row should stay listed - Got:\n{listed}"
    )
    with pytest.raises(UnresolvedConflictError):
        repo.commit("Merge feature-branch")

    repo.branch_merge_resolve_theirs("linked/second")
    output = repo.commit("Merge feature-branch")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    assert (
        link_pin(repo, sources["linked/second"].get_id()) == pins["linked/second"][1]
    ), "The committed row should be the one the merged branch held"


def _parent_linking_at(new_lore_repo, mount: str) -> tuple[Lore, Lore]:
    """A parent linking at `mount` a repository holding `conflicted.txt` and
    `sibling.txt`, and the linked repository. A mount below `linked/repo` is a link
    inside the link there, which holds `outer.txt`."""
    link_files = {"conflicted.txt": "base\n", "sibling.txt": "sibling\n"}
    if mount == "linked/repo":
        return make_parent_with_link(new_lore_repo, mount, link_files)
    repo, _ = make_parent_with_link(new_lore_repo, "linked/repo", {"outer.txt": "o\n"})
    source = make_repo(new_lore_repo, link_files)
    repo.link_add(mount, source.get_id(), "/")
    repo.commit("Link a repository inside the link")
    repo.push()
    return repo, source


@pytest.mark.smoke
@pytest.mark.parametrize("link_only", [False, True], ids=["merge", "link"])
@pytest.mark.parametrize(
    ("mount", "view", "kept"),
    [
        ("linked/repo", ["/linked/repo/"], []),
        (
            "linked/repo",
            ["/linked/repo/conflicted.txt"],
            ["linked/repo", "linked/repo/sibling.txt"],
        ),
        (
            "linked/repo",
            ["/linked/repo/", "!/linked/repo/sibling.txt"],
            ["linked/repo", "linked/repo/sibling.txt"],
        ),
        (
            "linked/repo/inner",
            ["/linked/repo/inner/"],
            ["linked/repo", "linked/repo/outer.txt"],
        ),
    ],
    ids=["mount", "file", "reinclude", "nested-mount"],
)
def test_link_conflict_abort_keeps_to_the_view(
    new_lore_repo, scratch_dir, mount, view, kept, link_only
):
    """Aborting a merge, or only its link's merge, that conflicts inside a link restores the
    files of the link the view keeps and the pin, and writes none the view excludes, as the
    merge start did, leaving nothing staged and the merge ended. Widening the view then realizes
    the pre-merge content."""
    repo, source = _parent_linking_at(new_lore_repo, mount)
    conflicted = f"{mount}/conflicted.txt"
    repo.branch_create("feature-branch")
    repo.clone(branch="main").write_commit_push("Theirs", {conflicted: "theirs\n"})
    repo.write_files({conflicted: "mine\n"})
    repo.stage([conflicted])
    repo.commit("Mine")
    pin_before = link_pin(repo, source.get_id())
    repo.sync(view=view_file(scratch_dir, "narrow", view))

    repo.branch_merge_start("main", message="Merge main", check=False)
    assert repo.unresolved_paths() == {conflicted}, (
        "the merge must conflict on the file both branches changed inside the link"
    )
    assert working_tree_files(repo, "linked", directories=True) == kept, (
        "the merge must write only what the view keeps"
    )

    repo.branch_merge_abort(link="linked/repo" if link_only else None)

    assert working_tree_files(repo, "linked", directories=True) == kept, (
        "aborting the merge must restore only what the view keeps"
    )
    staged = [entry["path"] for entry in parse_status_json(repo.status(json=True))]
    assert staged == [], "aborting the merge must leave nothing staged"
    assert link_pin(repo, source.get_id()) == pin_before, (
        "aborting the merge must restore the link's pin"
    )
    with pytest.raises(NotInProgress):
        repo.branch_merge_abort()
    repo.sync(view=view_file(scratch_dir, "wide", []))
    fail_unless_holds(repo, conflicted, "mine\n")


@pytest.mark.smoke
@pytest.mark.parametrize("same_file", [False, True], ids=["apart", "same-file"])
def test_link_fixed_on_the_merged_branch_conflicts_with_its_own_merge(
    new_lore_repo, same_file
):
    """A link both branches changed inside, which the merged branch also made fixed, holds its
    own merge's result against the merged branch's fixed row, and abort restores the pre-merge
    row and the files inside the link, a conflict there resolved included.

    The link's own merge produces a revision on the linked branch this branch follows, so the
    merged branch's flags cannot land on it.
    """
    link_path = "linked/repo"
    repo = make_repo(new_lore_repo, {"main-file.txt": "main\n"})
    source_repo = make_repo(
        new_lore_repo, {"link-file.txt": "one\n", "other.txt": "other\n"}
    )
    repo.link_add(link_path, source_repo.get_id(), "/")
    repo.commit("Add a following link", non_interactive=True)
    repo.push()

    def commit_inside_the_link(path, content, message):
        with repo.open_file(f"{link_path}/{path}", "w+") as f:
            f.write(content)
        repo.stage(scan=True)
        repo.commit(message, non_interactive=True)
        repo.push()

    repo.branch_create("feature-branch")
    commit_inside_the_link(
        "link-file.txt", "feature\n", "Feature edits inside the link"
    )
    feature_pin = link_pin(repo, source_repo.get_id())
    repo.branch_switch("main")
    commit_inside_the_link(
        "link-file.txt" if same_file else "other.txt",
        "main\n",
        "Main edits inside the link",
    )
    repo.link_remove(link_path)
    repo.commit("Remove the link", non_interactive=True)
    repo.push()
    repo.link_add(link_path, source_repo.get_id(), "/", disable_branching=True)
    repo.commit("Add the link back fixed", non_interactive=True)
    repo.push()
    main_pin = link_pin(repo, source_repo.get_id())

    repo.branch_switch("feature-branch")
    if same_file:
        repo.branch_merge_start("main", message="Merge main", non_interactive=True)
        repo.write_files({f"{link_path}/link-file.txt": "main\n"})
        repo.branch_merge_resolve(f"{link_path}/link-file.txt")
    output = repo.branch_merge_start("main", message="Merge main", non_interactive=True)
    assert f"Link conflict: {link_path}" in output, (
        f"The merge should hold the row open - Got:\n{output}"
    )

    repo.branch_merge_resolve_theirs(link_path)
    assert link_field(repo, source_repo.get_id(), "Flags") == "DisableAutoFollow"
    assert link_pin(repo, source_repo.get_id()) == main_pin, (
        "Theirs should be the merged branch's row with its own pin"
    )

    repo.branch_merge_abort()
    assert link_field(repo, source_repo.get_id(), "Flags") == "None"
    assert link_pin(repo, source_repo.get_id()) == feature_pin, (
        "Abort should restore the row this branch held before the link's own merge"
    )
    assert mounted_files(repo, link_path) == {
        "link-file.txt": "feature\n",
        "other.txt": "other\n",
    }


@pytest.mark.smoke
def test_link_pin_conflict_resolve_keeps_work_staged_in_the_link(new_lore_repo):
    """Work staged inside a divergent link during the merge is this branch's side: settling to
    mine keeps it, and taking theirs is refused rather than discard it."""
    link_path = "linked/repo"
    repo, _source_repo, _revision_two, _revision_three = _diverge_link_pins(
        new_lore_repo, link_path
    )
    repo.branch_merge_start("feature-branch", message="Merge divergent link pins")

    with repo.open_file(f"{link_path}/link-file.txt", "w+") as f:
        f.write("staged during the merge\n")
    repo.stage(f"{link_path}/link-file.txt")

    repo.branch_merge_resolve()
    assert _mounted_link_file(repo, link_path) == "staged during the merge\n", (
        "Settling to mine should keep the work staged inside the link"
    )
    with pytest.raises(ImproperArgumentsError):
        repo.branch_merge_resolve_theirs(link_path)
    assert _mounted_link_file(repo, link_path) == "staged during the merge\n", (
        "A refused resolve should leave the work staged inside the link"
    )


@pytest.mark.smoke
def test_link_remount_kept_where_the_merged_pin_lacks_it_is_a_conflict(new_lore_repo):
    """This branch remounting a link the merged branch repinned to a revision without that folder
    holds the merge open, and theirs takes the merged branch's row whole."""
    link_path = "linked/repo"
    source_repo = make_repo(new_lore_repo, {"first/a.txt": "a\n", "top.txt": "top\n"})
    pin_with_folder = source_repo.branch_info().local_latest
    source_repo.rmtree("first")
    source_repo.stage("first", scan=True)
    source_repo.commit("Remove first/")
    source_repo.push()
    pin_without_folder = source_repo.branch_info().local_latest

    repo = make_repo(new_lore_repo, {"main-file.txt": "main\n"})
    repo.link_add(
        link_path,
        source_repo.get_id(),
        "/",
        disable_branching=True,
        pin=pin_with_folder,
    )
    repo.commit("Add the link")
    repo.push()

    repo.branch_create("feature-branch")
    repo.link_update(link_path, pin=pin_without_folder)
    repo.commit("Repin to the revision without first/")
    repo.push()

    repo.branch_switch("main")
    repo.link_remove(link_path)
    repo.commit("Remove the link")
    repo.link_add(
        link_path,
        source_repo.get_id(),
        "first",
        disable_branching=True,
        pin=pin_with_folder,
    )
    repo.commit("Mount first/")
    repo.push()

    output = repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    assert f"Link conflict: {link_path} (mount change)" in output, (
        f"The merge should hold the row open - Got:\n{output}"
    )

    repo.branch_merge_resolve_theirs(link_path)
    assert link_pin(repo, source_repo.get_id()) == pin_without_folder
    assert_mount_exposes(repo, link_path, "/", ["top.txt"])


@pytest.mark.smoke
def test_link_pin_conflict_stays_unsettled_when_the_resolve_is_refused(new_lore_repo):
    """A resolve naming a divergent row and a conflict it refuses to settle settles neither."""
    repo = make_repo(
        new_lore_repo, {"main-file.txt": "main\n", "shared/a.txt": "a original\n"}
    )
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "one\n"})
    folder_repo = make_repo(new_lore_repo, {"a.txt": "a original\n"})
    repo.link_add("linked/repo", source_repo.get_id(), "/", disable_branching=True)
    repo.commit("Add a fixed link")
    repo.push()
    source_repo.write_commit_push("Second source revision", {"link-file.txt": "two\n"})
    revision_two = source_repo.branch_info().local_latest
    source_repo.write_commit_push("Third source revision", {"link-file.txt": "three\n"})
    revision_three = source_repo.branch_info().local_latest

    repo.branch_create("feature-branch")
    repo.link_update("linked/repo", pin=revision_three)
    repo.rmtree("shared")
    repo.stage("shared", scan=True)
    repo.link_add("shared", folder_repo.get_id(), "/")
    repo.commit("Repin the link and replace shared/ with a link")
    repo.push()

    repo.branch_switch("main")
    repo.link_update("linked/repo", pin=revision_two)
    repo.write_commit_push("Repin the link and change shared/", {"shared/a.txt": "a\n"})
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.write_files({"shared/a.txt": "a edited\n"})

    with pytest.raises(LocalChanges):
        repo.branch_merge_resolve_theirs(["linked/repo", "shared"])

    listed = repo.branch_merge_conflicts_list()
    assert "Link conflict: linked/repo" in listed, (
        f"The row a refused resolve named should stay listed - Got:\n{listed}"
    )
    assert link_pin(repo, source_repo.get_id()) == revision_two, (
        "A refused resolve should leave the row this branch holds"
    )
    assert repo.unresolved_paths() == {"shared"}
    with repo.open_file("shared/a.txt") as f:
        assert f.read() == "a edited\n", "the edit must stay in the working tree"


@pytest.mark.smoke
def test_link_pin_conflict_follows_a_branch_created_while_the_merge_is_open(
    new_lore_repo,
):
    """A branch created during the merge moves its divergent rows onto it, as it moves the
    links themselves, so the row a resolve writes back follows the new branch."""
    link_path = "linked/repo"
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "one\n"})
    repo.link_add(link_path, source_repo.get_id(), "/")
    repo.commit("Add a following link", non_interactive=True)
    repo.push()
    source_repo.branch_create("other")
    source_repo.write_commit_push("Second source revision", {"link-file.txt": "two\n"})
    revision_two = source_repo.branch_info().local_latest
    source_repo.write_commit_push("Third source revision", {"link-file.txt": "three\n"})

    repo.branch_create("feature-branch")
    repo.link_update(link_path, pin=source_repo.branch_info().local_latest)
    repo.commit("Pin the link to the other branch's latest", non_interactive=True)
    repo.push()
    repo.branch_switch("main")
    repo.link_update(link_path, pin=revision_two)
    repo.commit("Pin the link to an earlier revision", non_interactive=True)
    repo.push()
    repo.branch_merge_start(
        "feature-branch", message="Merge divergent link pins", non_interactive=True
    )

    repo.branch_create("followed")
    repo.branch_merge_resolve_mine(link_path)
    output = repo.commit("Merge divergent link pins", non_interactive=True)
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"

    assert link_field(repo, source_repo.get_id(), "Branch") == "followed", (
        "The settled row should follow the branch created during the merge"
    )
    assert link_pin(repo, source_repo.get_id()) == revision_two, (
        "The settled row should keep the pin this branch held"
    )


@pytest.mark.smoke
def test_link_pin_conflict_creates_the_branch_for_a_merged_in_link(new_lore_repo):
    """A merge held open by a divergent pin still brings in the links the merged branch adds,
    with the branch they follow created in their own repository."""
    repo, _source_repo, _revision_two, _revision_three = _diverge_link_pins(
        new_lore_repo, "linked/repo"
    )
    other_repo = make_repo(new_lore_repo, {"other.txt": "other\n"})
    repo.link_add("linked/other", other_repo.get_id(), "/")
    repo.commit("Add a following link on main")
    repo.push()

    repo.branch_switch("feature-branch")
    repo.branch_merge_start("main", message="Merge divergent link pins")

    listed = repo.branch_merge_conflicts_list()
    assert "Link conflict: linked/repo" in listed, (
        f"The divergent row should be listed - Got:\n{listed}"
    )
    branches = remote_branch_names(other_repo)
    assert branches == ["feature-branch", "main"], (
        f"The merged-in link should follow a branch of its own.\nGot: {branches}"
    )


@pytest.mark.smoke
def test_link_pin_conflict_resolve_writes_the_chosen_rows_flags(new_lore_repo):
    """Rows pinning the same revision but carrying different flags are a conflict,
    and settling it writes the flags of the side chosen.
    """
    link_path = "linked/repo"
    repo, source_repo, revision_two = _link_readded_fixed_on_feature(
        new_lore_repo, link_path
    )
    repo.link_update(link_path, pin=revision_two)
    repo.commit("Pin the link to the same revision", non_interactive=True)
    repo.push()

    output = repo.branch_merge_start(
        "feature-branch", message="Merge divergent link rows", non_interactive=True
    )
    assert f"Link conflict: {link_path} (flag change)" in output, (
        f"The merge should name the flag change - Got:\n{output}"
    )

    repo.branch_merge_resolve_mine(link_path)
    assert link_field(repo, source_repo.get_id(), "Flags") == "None", (
        "Resolving mine should keep this branch's flags"
    )

    repo.branch_merge_resolve_theirs(link_path)
    assert link_field(repo, source_repo.get_id(), "Flags") == "DisableAutoFollow", (
        "Resolving theirs should write the merged branch's flags"
    )

    output = repo.commit("Merge divergent link rows", non_interactive=True)
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    repo.push()
    assert (
        link_field(repo.clone(), source_repo.get_id(), "Flags") == "DisableAutoFollow"
    ), "The committed row should carry the flags of the side chosen"


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("abort_options", "kept"),
    [
        ({}, "mine"),
        ({"link": "linked/repo"}, "mine"),
        ({"ignore_links": True}, "theirs"),
    ],
    ids=["abort", "abort-link", "abort-ignore-links"],
)
def test_link_pin_conflict_abort_restores_the_whole_row(
    new_lore_repo, abort_options, kept
):
    """Abort after a resolve realized the other side puts this branch's row back whole:
    its pin, its flags and the content at the mount.

    `--ignore-links` keeps the link rows the merge produced, so it keeps the side chosen.
    """
    link_path = "linked/repo"
    repo, source_repo, revision_two = _link_readded_fixed_on_feature(
        new_lore_repo, link_path
    )
    source_repo.write_commit_push("Third source revision", {"link-file.txt": "three\n"})
    revision_three = source_repo.branch_info().local_latest
    repo.link_update(link_path, pin=revision_three)
    repo.commit("Pin the link to a newer revision", non_interactive=True)
    repo.push()

    repo.branch_merge_start(
        "feature-branch", message="Merge divergent link rows", non_interactive=True
    )
    repo.branch_merge_resolve_theirs(link_path)
    repo.branch_merge_abort(**abort_options)

    flags, pin, content = {
        "mine": ("None", revision_three, "three\n"),
        "theirs": ("DisableAutoFollow", revision_two, "two\n"),
    }[kept]
    assert link_field(repo, source_repo.get_id(), "Flags") == flags
    assert link_pin(repo, source_repo.get_id()) == pin
    assert _mounted_link_file(repo, link_path) == content


@pytest.mark.smoke
def test_link_pin_update_merge_ignore_links_keeps_pin(new_lore_repo):
    """`--ignore-links` leaves a one-sided pin move where the target has it.

    Carrying the row would have to realize the linked content at the mount,
    which is the link content work this flag suppresses.
    """
    link_path = "linked/repo"
    repo, source_repo, pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)

    repo.branch_create("feature-branch")
    repo.link_update(link_path)
    with repo.open_file("feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Move link pin and add a file on feature branch")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge_start(
        "feature-branch", message="Merge without links", ignore_links=True
    )
    repo.push()

    assert repo.file_exists("feature-file.txt"), (
        "Parent changes should still merge with --ignore-links"
    )
    assert link_pin(repo, source_repo.get_id()) == pin_base, (
        "--ignore-links should not move the link pin"
    )


@pytest.mark.smoke
def test_link_remount_on_the_target_survives_a_merged_pin_move(new_lore_repo):
    """The target's own remount stays when the merged branch only moved the pin."""
    link_path = "linked/repo"
    source_repo = make_repo(
        new_lore_repo, {"first/a.txt": "a\n", "second/b.txt": "b\n"}
    )
    pin_one = source_repo.branch_info().local_latest
    source_repo.write_commit_push("Second revision", {"second/c.txt": "c\n"})
    pin_two = source_repo.branch_info().local_latest
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    repo.link_add(link_path, source_repo.get_id(), "first", pin=pin_one)
    repo.commit("Add link")
    repo.push()

    repo.branch_create("feature-branch")
    repo.link_update(link_path, pin=pin_two)
    repo.commit("Move the pin on feature branch")
    repo.push()

    repo.branch_switch("main")
    remount(repo, link_path, source_repo.get_id(), "second", pin=pin_one)
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    assert link_pin(repo, source_repo.get_id()) == pin_two, (
        "The merge should carry the pin the merged branch moved"
    )
    assert_mount_exposes(repo, link_path, "second", ["b.txt", "c.txt"])


@pytest.mark.smoke
def test_link_remount_divergent_is_a_conflict_either_side_settles(new_lore_repo):
    """Both branches remounting a link holds the merge open on the mount each side chose,
    and settling it mounts the chosen side's folder.

    The mount is the one part of a link its pin cannot express, so a side that moved it is
    invisible to the pin comparison and has to be judged on its own.
    """
    link_path = "linked/repo"
    repo, source_repo = make_parent_with_remount(
        new_lore_repo,
        {"first/a.txt": "a\n", "second/b.txt": "b\n"},
        "/",
        "second",
        link_path,
    )

    repo.branch_switch("main")
    remount(repo, link_path, source_repo.get_id(), "first")

    output = repo.branch_merge_start(
        "feature-branch", message="Merge divergent remounts"
    )
    assert f"Link conflict: {link_path} (mount change)" in output, (
        f"The merge should name the mount change - Got:\n{output}"
    )
    assert "Source path: first" in output and "Source path: second" in output, (
        f"Both sides' mounts should be listed - Got:\n{output}"
    )
    assert_mount_exposes(repo, link_path, "first", ["a.txt"])

    repo.branch_merge_resolve_theirs(link_path)
    assert_mount_exposes(repo, link_path, "second", ["b.txt"])

    output = repo.commit("Merge divergent remounts")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    repo.push()
    assert_mount_source(repo.clone(), link_path, "second")


# ---------------------------------------------------------------------------
# A link replacing a committed folder, merged in both directions.
# ---------------------------------------------------------------------------


def _change_actions(output: str, path: str) -> set[str]:
    """The change actions a diff reported for `path`.

    A directory and a link node both print with a trailing slash, which the
    path a caller asks about does not carry.
    """
    actions = set()
    for line in output.splitlines():
        match = re.match(r"^([ADMCG])\s+(\S.*)$", line.strip())
        if match and match.group(2).rstrip("/") == path:
            actions.add(match.group(1))
    return actions


def _conflicted_count(output: str) -> int:
    """The conflict count a merge reported."""
    match = re.search(r"(\d+) conflicted", output)
    assert match, f"merge did not report a conflict count:\n{output}"
    return int(match.group(1))


def _conflicted_paths(parent: Lore) -> list[str]:
    """The paths `status` reports as conflicted."""
    return [
        entry["path"]
        for entry in parse_status_json(parent.status(json=True))
        if entry.get("flagConflict")
    ]


def _assert_mount_conflict(
    parent: Lore,
    merged_branch: str,
    message: str,
) -> None:
    """Merging `merged_branch` conflicts at the mount, commits nothing, and aborts cleanly."""
    head_before = parent.branch_info().local_latest

    output = parent.branch_merge_start(merged_branch, message=message, check=False)

    assert _conflicted_count(output) == 1, (
        f"the merge must report the replaced mount as its one conflict, got:\n{output}"
    )
    assert _conflicted_paths(parent) == ["shared"], (
        f"the conflict must be reported at the mount, got {_conflicted_paths(parent)}"
    )
    listed = parent.branch_merge_conflicts_list()
    assert "shared" in listed.splitlines(), (
        f"`merge conflicts list` must list the mount, got:\n{listed}"
    )
    assert parent.branch_info().local_latest == head_before, (
        "the merge must leave the branch on its pre-merge revision"
    )

    with pytest.raises(UnresolvedConflictError):
        parent.commit(message)

    parent.branch_merge_abort()

    assert parent.branch_info().local_latest == head_before, (
        "aborting the merge must leave the branch on its pre-merge revision"
    )
    assert parse_status_json(parent.status(json=True)) == [], (
        "aborting the merge must leave nothing staged"
    )


@pytest.mark.smoke
def test_branch_diff_reports_a_link_replacing_a_folder(new_lore_repo):
    """Replacing a committed folder with a link is a type change at the mount,
    and a diff reports it in both directions of the replacement."""
    parent, _source = folder_replaced_by_a_link(new_lore_repo)

    folder_to_link = parent.branch_diff("main", source="feature")
    assert _change_actions(folder_to_link, "shared") == {"A", "D"}, (
        "a folder replaced by a link must be reported as replaced, "
        f"got:\n{folder_to_link}"
    )

    parent.branch_create("restored")
    replace_link_with_folder(parent)

    link_to_folder = parent.branch_diff("feature", source="restored")
    assert _change_actions(link_to_folder, "shared") == {"A", "D"}, (
        "a link replaced by a folder must be reported as replaced, "
        f"got:\n{link_to_folder}"
    )


@pytest.mark.smoke
def test_merge_outside_the_folder_keeps_a_link_replacing_it(new_lore_repo):
    """A branch that touched nothing under the folder merges cleanly into the
    branch where a link replaced it, and the mount survives with its pin."""
    parent, source = folder_replaced_by_a_link(new_lore_repo)
    pin = link_pin(parent, source.get_id())

    parent.branch_switch("main")
    parent.write_commit_push(
        "Change a file outside shared/", {"root.txt": "root changed on main\n"}
    )

    parent.branch_switch("feature")
    parent.branch_merge_start("main", message="Merge main")
    parent.push()

    assert_holds_link(parent, source, pin)
    with parent.open_file("root.txt", "r") as input_file:
        assert "root changed on main" in input_file.read(), (
            "the merge must carry the change made outside the folder"
        )
    assert "Verified repository state integrity" in parent.repository_verify(), (
        "the merged revision must verify"
    )

    clone = parent.clone(branch="feature")
    assert source.get_id() in clone.link_info("shared"), (
        "a fresh clone of the merged revision must hold the link"
    )
    assert clone.compare_file(source, "shared/b.txt", "b.txt"), (
        "a fresh clone must serve the linked repository's content at the mount"
    )


@pytest.mark.smoke
def test_merge_of_a_link_replaced_by_another_repository_lands_the_new_one(
    new_lore_repo,
):
    """A link re-pointed at another repository at the same path merges as that repository.

    The two repositories' subtrees share nothing, so the mount and its registry row have to be
    replaced rather than diffed.
    """
    link_path = "linked/repo"
    first = make_repo(new_lore_repo, {"a.txt": "a\n"})
    second = make_repo(new_lore_repo, {"b.txt": "b\n"})
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    repo.link_add(link_path, first.get_id(), "/")
    repo.commit("Link the first repository")
    repo.push()

    repo.branch_create("feature-branch")
    repo.link_remove(link_path)
    repo.link_add(link_path, second.get_id(), "/")
    repo.commit("Link the second repository instead")
    repo.push()

    repo.branch_switch("main")
    repo.write_commit_push("Change on main", {"main-only.txt": "main\n"})
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    linked = re.findall(r"^Link\s+(\w+)", repo.link_list(), re.MULTILINE)
    assert linked == [second.get_id()], (
        f"The merged registry should hold only the second repository, got {linked}"
    )
    assert_mount_exposes(repo, link_path, "/", ["b.txt"])
    assert_mount_exposes(repo.clone(), link_path, "/", ["b.txt"])


@pytest.mark.smoke
def test_merge_of_a_link_replacing_a_folder_lands_the_link(new_lore_repo):
    """Merging the branch where a link replaced the folder into the branch that
    still holds the folder applies the replacement."""
    parent, source = folder_replaced_by_a_link(new_lore_repo)
    pin = link_pin(parent, source.get_id())

    parent.branch_switch("main")
    parent.branch_merge_start("feature", message="Merge feature")
    parent.push()

    assert_holds_link(parent, source, pin)
    assert "Verified repository state integrity" in parent.repository_verify(), (
        "the merged revision must verify"
    )

    clone = parent.clone(branch="main")
    assert source.get_id() in clone.link_info("shared"), (
        "a fresh clone of the merged revision must hold the link"
    )


@pytest.mark.smoke
def test_merge_of_a_folder_replacing_a_link_lands_the_folder(new_lore_repo):
    """Merging the branch where a folder replaced the link into the branch that
    still holds the link applies the replacement, where the folder holds content
    the link does not."""
    parent, _source = link_replaced_by_its_folder(new_lore_repo)
    parent.branch_switch("feature")
    parent.write_commit_push(
        "Edit the folder", {"shared/a.txt": "a edited in the folder\n"}
    )

    parent.branch_switch("main")
    parent.branch_merge_start("feature", message="Merge feature")
    parent.push()

    assert_holds_folder(
        parent, {**SHARED_FOLDER, "shared/a.txt": "a edited in the folder\n"}
    )


@pytest.mark.smoke
@pytest.mark.parametrize("merged_branch", ["main", "feature"])
@pytest.mark.parametrize(
    "change_folder",
    [add_inside_the_folder, delete_inside_the_folder, delete_the_folder],
    ids=["add", "delete", "delete_folder"],
)
def test_merge_of_a_lone_add_or_delete_under_a_replaced_folder_conflicts(
    new_lore_repo, change_folder, merged_branch
):
    """A file only added or only deleted under the folder a link replaced, or
    the whole folder deleted, conflicts at the mount in both directions, and
    aborting keeps each branch's side."""
    parent, source = folder_replaced_by_a_link(new_lore_repo)
    pin = link_pin(parent, source.get_id())

    parent.branch_switch("main")
    folder = change_folder(parent)
    if merged_branch == "main":
        parent.branch_switch("feature")

    _assert_mount_conflict(parent, merged_branch, f"Merge {merged_branch}")

    if merged_branch == "main":
        assert_holds_link(parent, source, pin)
    else:
        assert_holds_folder(parent, folder)
        assert registered_links(parent) == {}, "the branch must hold no link"


def _mount_conflict(new_lore_repo, layout: str, into: str):
    """Returns (parent, source, merged, sides) once a merge of the branch `merged` into the
    branch holding `into`, the link or the folder at `shared/`, stops on the conflict at the
    mount.

    `layout` names what the branches did: `folder_changed` and `folder_deleted` replace the
    folder with a link on one branch and change or delete the folder on the other;
    `link_changed` replaces the link with its folder on one branch and edits inside the link on
    the other. `sides` maps `mine` and `theirs` to what that side holds at the mount, as
    `_assert_holds_side` reads it.
    """
    if layout == "link_changed":
        parent, source = link_replaced_by_its_folder(new_lore_repo)
        link_branch, folder_branch = "main", "feature"
        folder = ("folder", SHARED_FOLDER)
        edited = {"shared/a.txt": "a changed in the link\n"}
        parent.write_commit_push("Change a file inside the link", edited)
        link = ("link", {**SHARED_FOLDER, **edited})
    else:
        parent, source = folder_replaced_by_a_link(new_lore_repo)
        link_branch, folder_branch = "feature", "main"
        link = ("link", SHARED_FOLDER)
        parent.branch_switch("main")
        if layout == "folder_changed":
            folder = ("folder", change_inside_the_folder(parent))
        else:
            delete_the_folder(parent)
            folder = ("nothing", None)

    current, merged = (
        (link_branch, folder_branch) if into == "link" else (folder_branch, link_branch)
    )
    parent.branch_switch(current)
    output = parent.branch_merge_start(merged, message=f"Merge {merged}", check=False)
    assert _conflicted_count(output) == 1, f"the merge must conflict:\n{output}"
    assert parent.unresolved_paths() == {"shared"}, (
        "the conflict must stand at the mount"
    )
    mine, theirs = (link, folder) if into == "link" else (folder, link)
    return parent, source, merged, {"mine": mine, "theirs": theirs}


def _assert_holds_side(repo: Lore, source: Lore, side: tuple):
    """`shared/` holds `side`: `("link", files)` for the link to `source` serving `files`,
    `("folder", files)` for a folder of `files`, or `("nothing", None)`."""
    kind, held = side
    if kind == "link":
        assert_holds_link(repo, source)
        assert mounted_files(repo, "shared") == in_the_shared_mount(held)
        assert registered_links(repo) == {"shared": source.get_id()}
    elif kind == "folder":
        assert_holds_folder(repo, held)
        assert registered_links(repo) == {}
    else:
        assert not repo.path_exists("shared"), "shared/ must be gone"
        assert registered_links(repo) == {}


def _assert_merge_settled(parent: Lore, source: Lore, merged: str, side: tuple):
    assert_merge_settled(
        parent, merged, lambda repo: _assert_holds_side(repo, source, side)
    )


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("into", "resolve"),
    [
        ("link", "mine"),
        ("link", "theirs"),
        ("folder", "mine"),
        ("folder", "theirs"),
        ("link", "theirs_walk"),
        ("folder", "theirs_overlapping"),
        ("folder", "bare"),
    ],
)
def test_resolving_a_mount_conflict_takes_the_chosen_side(new_lore_repo, into, resolve):
    """A conflict at a mount a link holds on one branch and a changed folder on the other
    resolves to the side named, or walked to, mine where none is, once however many of the
    paths named reach it: the commit holds that side."""
    parent, source, merged, sides = _mount_conflict(new_lore_repo, "folder_changed", into)

    if resolve == "bare":
        parent.branch_merge_resolve()
    elif resolve == "theirs_walk":
        parent.branch_merge_resolve_theirs(".")
    elif resolve == "theirs_overlapping":
        parent.branch_merge_resolve_theirs(["shared", "."])
    else:
        getattr(parent, f"branch_merge_resolve_{resolve}")("shared")
    side = "mine" if resolve in ("mine", "bare") else "theirs"
    assert parent.unresolved_paths() == set(), "the resolve must settle the mount"
    parent.commit(f"Merge {merged}")

    _assert_merge_settled(parent, source, merged, sides[side])


@pytest.mark.smoke
@pytest.mark.parametrize("into", ["link", "folder"])
@pytest.mark.parametrize("layout", ["folder_deleted", "link_changed"])
def test_resolving_a_mount_conflict_takes_the_merged_side_whatever_it_holds(
    new_lore_repo, layout, into
):
    """Taking the merged side of a mount conflict commits it whether it holds the link, a
    folder, or nothing, and whichever branch replaced the other's side."""
    parent, source, merged, sides = _mount_conflict(new_lore_repo, layout, into)

    parent.branch_merge_resolve_theirs("shared")
    parent.commit(f"Merge {merged}")

    _assert_merge_settled(parent, source, merged, sides["theirs"])


@pytest.mark.smoke
@pytest.mark.parametrize("into", ["link", "folder"])
def test_resolving_a_mount_conflict_back_to_this_branch_takes_its_side(new_lore_repo, into):
    """A mount conflict resolved to the merged side and then to this branch's commits this
    branch's side."""
    parent, source, merged, sides = _mount_conflict(new_lore_repo, "folder_changed", into)

    parent.branch_merge_resolve_theirs("shared")
    parent.branch_merge_resolve_mine("shared")
    parent.commit(f"Merge {merged}")

    _assert_merge_settled(parent, source, merged, sides["mine"])


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("into", "first", "named"),
    [("link", None, "theirs"), ("folder", None, "theirs"), ("folder", "theirs", "mine")],
)
def test_resolving_a_mount_conflict_in_a_dry_run_keeps_what_the_mount_holds(
    new_lore_repo, into, first, named
):
    """A dry run of a resolve that replaces what the mount holds keeps the side the mount
    holds, what is staged and the linked repository's branches, and the resolve still takes
    the side named after it."""
    parent, source, merged, sides = _mount_conflict(new_lore_repo, "folder_changed", into)
    if first:
        getattr(parent, f"branch_merge_resolve_{first}")("shared")
    held = sides[first or "mine"]
    status = parse_status_json(parent.status(json=True))
    branches = source.branch_list()

    getattr(parent, f"branch_merge_resolve_{named}")("shared", dry_run=True)

    _assert_holds_side_in_merge(parent, source, held)
    assert parse_status_json(parent.status(json=True)) == status
    assert source.branch_list() == branches
    getattr(parent, f"branch_merge_resolve_{named}")("shared")
    parent.commit(f"Merge {merged}")
    _assert_merge_settled(parent, source, merged, sides[named])


def _assert_holds_side_in_merge(repo: Lore, source: Lore, side: tuple):
    """`shared/` holds `side` in the working tree of a merge in progress, as
    `_assert_holds_side` reads it."""
    kind, held = side
    if kind == "link":
        assert registered_links(repo) == {"shared": source.get_id()}
        assert mounted_files(repo, "shared") == in_the_shared_mount(held)
    else:
        assert registered_links(repo) == {}
        assert working_tree_files(repo, "shared") == sorted(held)


@pytest.mark.smoke
@pytest.mark.parametrize("into", ["link", "folder"])
def test_aborting_a_resolved_mount_conflict_restores_the_branch(new_lore_repo, into):
    """Aborting a merge whose mount conflict took the merged side puts back what the branch
    held at the mount, with nothing left staged."""
    parent, source, _merged, sides = _mount_conflict(new_lore_repo, "folder_changed", into)
    parent.branch_merge_resolve_theirs("shared")

    parent.branch_merge_abort()

    _assert_holds_side(parent, source, sides["mine"])
    assert parse_status_json(parent.status(json=True)) == []


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("layout", "into", "first", "named"),
    [
        ("folder_changed", "link", None, "theirs"),
        ("folder_changed", "folder", None, "theirs"),
        ("folder_deleted", "folder", None, "theirs"),
        ("folder_changed", "folder", "theirs", "mine"),
    ],
)
def test_resolving_a_mount_conflict_refuses_losing_work_or_a_path_inside_it(
    new_lore_repo, layout, into, first, named
):
    """A resolve replacing what the mount holds, a folder, a link or nothing, is refused while
    the mount holds work not committed, which stays with the conflict. A path inside an
    unresolved mount conflict names no side of it: refused inside the link, and settling
    nothing elsewhere."""
    parent, _source, _merged, _sides = _mount_conflict(new_lore_repo, layout, into)
    if first:
        getattr(parent, f"branch_merge_resolve_{first}")("shared")
    parent.write_files({"shared/b.txt": "b edited\n"})

    with pytest.raises(LocalChanges):
        getattr(parent, f"branch_merge_resolve_{named}")("shared")
    if into == "link" and not first:
        with pytest.raises(NotSupportedError):
            getattr(parent, f"branch_merge_resolve_{named}")("shared/a.txt")
    elif not first:
        getattr(parent, f"branch_merge_resolve_{named}")("shared/a.txt")

    assert parent.unresolved_paths() == (set() if first else {"shared"})
    with parent.open_file("shared/b.txt") as f:
        assert f.read() == "b edited\n", "the edit must stay in the working tree"


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("other", "into", "side"),
    [
        (other, into, "theirs")
        for other in ["file", "another_repository"]
        for into in ["link", "other"]
    ]
    + [("file", "other", "mine")],
)
def test_resolving_a_conflict_with_a_link_at_its_path_takes_the_side_named(
    new_lore_repo, other, into, side
):
    """A link one branch adds where the other adds a file or a link to another repository
    conflicts at the path, and either side commits: the revision verifies, clones the same,
    and a merge after it has nothing left."""
    link_path = "linked/repo"
    parent = make_repo(new_lore_repo, {"root.txt": "root\n"})
    source = make_repo(new_lore_repo, {"link-file.txt": "from link\n"})
    parent.branch_create("feature")
    parent.link_add(link_path, source.get_id(), "/")
    parent.commit("Add a link")
    parent.push()

    parent.branch_switch("main")
    if other == "another_repository":
        other_repo = make_repo(new_lore_repo, {"other.txt": "other\n"})
        parent.link_add(link_path, other_repo.get_id(), "/")
        parent.commit("Link another repository at the path")
        parent.push()
    else:
        parent.write_commit_push("Add a file at the path", {link_path: "plain\n"})

    merged = "feature" if into == "other" else "main"
    parent.branch_switch("main" if into == "other" else "feature")
    parent.branch_merge_start(merged, message=f"Merge {merged}", check=False)
    assert parent.unresolved_paths() == {link_path}

    getattr(parent, f"branch_merge_resolve_{side}")(link_path)
    parent.commit(f"Merge {merged}")

    def assert_holds_the_side_named(repo: Lore):
        if (merged == "feature") == (side == "theirs"):
            assert registered_links(repo) == {link_path: source.get_id()}
            assert repo.compare_file(
                source, f"{link_path}/link-file.txt", "link-file.txt"
            )
        elif other == "another_repository":
            assert registered_links(repo) == {link_path: other_repo.get_id()}
            assert repo.compare_file(other_repo, f"{link_path}/other.txt", "other.txt")
        else:
            assert registered_links(repo) == {}
            fail_unless_holds(repo, link_path, "plain\n")

    assert_merge_settled(parent, merged, assert_holds_the_side_named)


@pytest.mark.smoke
def test_resolving_mount_conflicts_of_links_to_one_repository_merges_it_once(new_lore_repo):
    """Two folders a branch replaced with following links to one repository, which the other
    branch changed, resolve together to the links, which land on one merge of the repository:
    both pin it, at the linked branch's latest."""
    parent = make_repo(
        new_lore_repo, {"root.txt": "root\n", "one/a.txt": "a\n", "two/a.txt": "a\n"}
    )
    source = make_repo(new_lore_repo, {"a.txt": "a\n"})
    parent.branch_create("feature")
    for folder in ["one", "two"]:
        parent.rmtree(folder)
        parent.stage(folder, scan=True)
        parent.link_add(folder, source.get_id(), "/")
    parent.commit("Replace both folders with links")
    parent.push()
    parent.branch_switch("main")
    parent.write_commit_push(
        "Change both folders", {"one/a.txt": "one\n", "two/a.txt": "two\n"}
    )
    source.write_commit_push("Move the linked repository on", {"b.txt": "b\n"})
    latest = source.branch_info().local_latest
    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {"one", "two"}

    parent.branch_merge_resolve_theirs(["one", "two"])
    parent.commit("Merge feature")

    def assert_holds_both_links(repo: Lore):
        assert registered_links(repo) == {"one": source.get_id(), "two": source.get_id()}
        assert link_pins(repo, source.get_id()) == [latest, latest]
        for folder in ["one", "two"]:
            assert mounted_files(repo, folder) == {"a.txt": "a\n", "b.txt": "b\n"}

    assert_merge_settled(parent, "feature", assert_holds_both_links)


@pytest.mark.smoke
@pytest.mark.parametrize("picked", ["folder", "link", "revert"])
def test_resolving_a_picked_or_reverted_mount_conflict_takes_that_side(new_lore_repo, picked):
    """Cherry-picking folder changes onto the branch where a link replaced the folder, or the
    link onto the branch that changed the folder, or reverting the replacement once the link
    changed, conflicts at the mount, and taking the picked or reverted side commits it."""
    parent, source = folder_replaced_by_a_link(new_lore_repo)
    replacement = parent.branch_info().local_latest
    parent.branch_switch("main")
    folder = change_inside_the_folder(parent)

    if picked == "folder":
        picked_revision = parent.branch_info().local_latest
        parent.branch_switch("feature")
        parent.revision_cherry_pick(picked_revision, check=False)
    elif picked == "link":
        parent.revision_cherry_pick(replacement, check=False)
    else:
        parent.branch_switch("feature")
        source.write_commit_push("Change the linked repository", {"b.txt": "b changed\n"})
        parent.link_update("shared", pin=source.branch_info().local_latest)
        parent.commit("Repin the link")
        parent.push()
        parent.revision_revert(replacement, check=False)
    assert parent.unresolved_paths() == {"shared"}, "the operation must conflict at the mount"

    if picked == "revert":
        parent.revision_revert_resolve_theirs("shared")
    else:
        parent.revision_cherry_pick_resolve_theirs("shared")
    parent.commit(f"Take the {picked} side")
    parent.push()

    branch = parent.branch_info().name
    for repo in (parent, parent.clone(branch=branch)):
        if picked == "link":
            assert_holds_link(repo, source)
            assert registered_links(repo) == {"shared": source.get_id()}
        else:
            assert_holds_folder(repo, folder if picked == "folder" else SHARED_FOLDER)
            assert registered_links(repo) == {}


@pytest.mark.smoke
@pytest.mark.parametrize("merged_branch", ["main", "feature"])
def test_merge_of_the_same_link_replacing_a_folder_on_both_branches_lands_one_link(
    new_lore_repo, merged_branch
):
    """Both branches replacing the folder with the same link merge with no
    conflict into one link, which a clone holds and a merge after it keeps."""
    parent, source = folder_replaced_by_a_link(new_lore_repo)
    pin = link_pin(parent, source.get_id())
    parent.branch_switch("main")
    replace_folder_with_a_link(parent, source)
    current = "main" if merged_branch == "feature" else "feature"
    parent.branch_switch(current)
    head_before = parent.branch_info().local_latest

    output = parent.branch_merge_start(merged_branch, message=f"Merge {merged_branch}")
    parent.push()

    assert _conflicted_count(output) == 0, (
        f"the merge must not conflict, got:\n{output}"
    )
    assert parent.branch_info().local_latest != head_before, "the merge must commit"
    assert registered_links(parent) == {"shared": source.get_id()}, (
        "the merge must hold the one link"
    )
    assert_holds_link(parent, source, pin)
    assert "Verified repository state integrity" in parent.repository_verify(), (
        "the merged revision must verify"
    )
    clone = parent.clone(branch=current)
    assert registered_links(clone) == {"shared": source.get_id()}, (
        "a fresh clone must hold the one link"
    )
    assert clone.compare_file(source, "shared/a.txt", "a.txt"), (
        "a fresh clone must serve the linked repository's content at the mount"
    )

    output = parent.branch_merge_start(merged_branch, message="Merge again")
    assert _conflicted_count(output) == 0, (
        f"a second merge must not conflict, got:\n{output}"
    )
    assert registered_links(parent) == {"shared": source.get_id()}, (
        "a second merge must keep the one link"
    )
    assert_holds_link(parent, source, pin)


def _link_row(parent: Lore) -> tuple[str, int, str]:
    """The pin, the flags and the mounted subtree of the link at `shared/`."""
    entry = link_entry(parent, "shared")
    return entry["revision"], entry["flags"], entry["sourcePath"]


def _link_row_conflicts(parent: Lore) -> list[str]:
    """The paths of the links whose rows the merge in progress lists as conflicting."""
    return [
        event["linkPath"]
        for event in parse_jsonl(
            parent.branch_merge_conflicts_list(json=True), "branchMergeLinkPinConflict"
        )
    ]


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("differ", "resolution"),
    [("pin", "theirs"), ("flags", "mine"), ("mount", "theirs"), ("added", "mine")],
)
def test_merge_of_the_same_repository_linked_otherwise_on_both_branches_conflicts_at_its_row(
    new_lore_repo, differ, resolution
):
    """Both branches putting a link to one repository in place of a folder, or
    where nothing stood, at other pins, with other flags or mounting another
    subtree, conflict at the link's row alone, which either side settles and the
    commit carries."""
    fixed = differ in ("pin", "added")
    if differ == "added":
        parent = make_repo(new_lore_repo, {"root.txt": "root\n"})
        source = make_repo(new_lore_repo, {"a.txt": "a\n", "b.txt": "b\n"})
        parent.branch_create("feature")
        add_a_link(parent, source, disable_branching=True)
    else:
        parent, source = folder_replaced_by_a_link(
            new_lore_repo, disable_branching=fixed
        )
    theirs = _link_row(parent)
    parent.branch_switch("main")
    if differ in ("pin", "added"):
        source.write_commit_push("Second revision", {"a.txt": "a second\n"})
    if differ == "mount":
        source.write_commit_push("Add a subtree", {"sub/c.txt": "c\n"})
    put_link = add_a_link if differ == "added" else replace_folder_with_a_link
    put_link(
        parent,
        source,
        source_path="/sub" if differ == "mount" else "/",
        disable_branching=differ != "mount",
    )
    mine = _link_row(parent)
    assert mine != theirs, "the branches must link the repository otherwise"

    output = parent.branch_merge_start("feature", message="Merge feature")

    assert _conflicted_count(output) == 0, f"the tree must not conflict, got:\n{output}"
    assert _link_row_conflicts(parent) == ["shared"], (
        "the merge must conflict at the row"
    )
    if resolution == "mine":
        parent.branch_merge_resolve_mine("shared")
        expected = mine
    else:
        parent.branch_merge_resolve_theirs("shared")
        expected = theirs
    assert "Commit succeeded" in parent.commit("Merge feature")
    parent.push()

    assert registered_links(parent) == {"shared": source.get_id()}, (
        "the branch must hold the one link"
    )
    assert _link_row(parent) == expected, (
        f"the link must hold the {resolution} side's row"
    )
    assert_holds_link(parent, source, expected[0])
    assert "Verified repository state integrity" in parent.repository_verify(), (
        "the revision must verify"
    )


@pytest.mark.smoke
def test_merge_of_the_same_link_following_each_branch_in_place_of_a_folder_lands_the_later_pin(
    new_lore_repo,
):
    """Both branches replacing the folder with a link to one repository that
    follows the branch, at two pins, merge with no conflict, leaving the later
    pin, which holds the other."""
    parent, source = folder_replaced_by_a_link(new_lore_repo)
    parent.branch_switch("main")
    source.write_commit_push("Second revision", {"a.txt": "a second\n"})
    replace_folder_with_a_link(parent, source)
    later = link_pin(parent, source.get_id())

    output = parent.branch_merge_start("feature", message="Merge feature")

    assert _conflicted_count(output) == 0, (
        f"the merge must not conflict, got:\n{output}"
    )
    assert parse_status_json(parent.status(json=True)) == [], "the merge must commit"
    assert_holds_link(parent, source, later)


@pytest.mark.smoke
def test_push_of_the_same_repository_linked_at_another_pin_in_place_of_a_folder_is_refused(
    new_lore_repo,
):
    """A push that replaced the folder with a link to the repository another
    push linked there at another pin is refused rather than merged on the
    server, which would drop its pin, and the branch keeps the first."""
    parent = make_repo(new_lore_repo, {"root.txt": "root\n", **SHARED_FOLDER})
    source = make_repo(new_lore_repo, {"a.txt": "a\n", "b.txt": "b\n"})
    other = parent.clone()
    replace_folder_with_a_link(parent, source, disable_branching=True)
    first = link_pin(parent, source.get_id())
    source.write_commit_push("Second revision", {"a.txt": "a second\n"})
    other.rmtree("shared")
    other.stage("shared", scan=True)
    other.link_add("shared", source.get_id(), "/", disable_branching=True)
    other.commit("Replace shared/ with a link")

    with pytest.raises(LoreException) as refused:
        other.push(fast_forward_merge=True)

    assert refused.value.returncode == 41, (
        "the server's refusal exits with BranchAdvanced"
    )
    assert committed_link_pin(parent, source.get_id()) == first, (
        "the branch must keep the first push's pin"
    )


@pytest.mark.smoke
def test_merge_into_refuses_the_same_repository_linked_at_two_pins_in_a_linked_repository(
    new_lore_repo,
):
    """`merge into` refuses a merge whose linked repository, which the branch
    adds, replaced a folder with a link to one repository at two pins on the
    two branches, which it has no row conflict to settle, before pushing
    either."""
    parent = make_repo(new_lore_repo, {"root.txt": "root\n"})
    linked = make_repo(new_lore_repo, SHARED_FOLDER)
    nested = make_repo(new_lore_repo, {"a.txt": "a\n", "b.txt": "b\n"})
    parent.branch_create("feature")
    parent.link_add("outer", linked.get_id(), "/")
    parent.commit("Link outer/")
    parent.push()
    parent.rmtree("outer/shared")
    parent.stage("outer/shared", scan=True)
    parent.link_add("outer/shared", nested.get_id(), "/", disable_branching=True)
    parent.commit("Replace outer/shared/ with a link")
    parent.push()
    parent.branch_switch("main")
    parent.write_commit_push("Change on main", {"root.txt": "root on main\n"})
    parent.branch_switch("feature")
    parent.branch_merge_start("main", message="Merge main")
    parent.push()
    nested.write_commit_push("Second revision", {"a.txt": "a second\n"})
    replace_folder_with_a_link(linked, nested, disable_branching=True)
    parent_main = parent.branch_info("main").remote_latest
    linked_main = linked.branch_info("main").remote_latest

    with pytest.raises(NotSupportedError):
        parent.branch_merge_into("main", message="Merge feature into main")

    assert parent.branch_info("main").remote_latest == parent_main
    assert linked.branch_info("main").remote_latest == linked_main


@pytest.mark.smoke
def test_revert_of_a_link_add_removes_the_link(new_lore_repo):
    """Reverting the revision that added a link removes the mount and its
    registry row."""
    parent, _source = make_parent_with_link(new_lore_repo, "linked/repo")

    parent.revision_revert(parent.branch_info().local_latest)
    parent.push()

    assert parse_status_json(parent.status(json=True)) == []
    for repo in (parent, parent.clone()):
        assert registered_links(repo) == {}
        assert not repo.path_exists("linked/repo")
        assert repo.file_exists(DEFAULT_PARENT_FILE), "the revert must keep the rest"


@pytest.mark.smoke
@pytest.mark.xfail(
    strict=True,
    raises=AssertionError,
    reason="cherry-pick and revert drop the part of a revision inside a link",
)
@pytest.mark.parametrize("operation", ["cherry_pick", "revert"])
def test_cherry_pick_and_revert_apply_the_part_inside_a_link(new_lore_repo, operation):
    """Picking or reverting a revision that changed a file inside a link and one
    outside it applies both."""
    link_path = "linked/repo"
    parent, _source = make_parent_with_link(
        new_lore_repo, link_path, {"a.txt": "a\n"}, {"root.txt": "root\n"}
    )
    parent.branch_create("feature")
    parent.write_commit_push(
        "Edit inside and outside the link",
        {f"{link_path}/a.txt": "a on feature\n", "root.txt": "root on feature\n"},
    )
    revision = parent.branch_info().local_latest

    if operation == "cherry_pick":
        parent.branch_switch("main")
        parent.revision_cherry_pick(revision)
        outside, inside = "root on feature\n", "a on feature\n"
    else:
        parent.revision_revert(revision)
        outside, inside = "root\n", "a\n"

    fail_unless_holds(parent, "root.txt", outside)
    with parent.open_file(f"{link_path}/a.txt") as f:
        assert f.read() == inside, "the part inside the link must be applied"


@pytest.mark.smoke
@pytest.mark.parametrize("folder_on", ["main", "feature"])
def test_a_type_change_inside_a_link_conflicts_and_refuses_the_folder(
    new_lore_repo, folder_on
):
    """A folder on one branch meets a file at the same path inside a link on
    the other, and the merge conflicts there. A resolve that would place or
    keep the folder is refused. With the folder on `feature` taking the file on
    `main` commits it; with the folder on `main` aborting restores the link's
    pin and the folder."""
    link_path = "linked/repo"
    conflicted = f"{link_path}/x"
    parent, source = make_parent_with_link(
        new_lore_repo, link_path, {"base.txt": "base\n"}, {"root.txt": "root\n"}
    )
    folder = {f"{conflicted}/y.txt": "y\n"}
    file = {conflicted: "x is a file\n"}

    parent.branch_create("feature")
    parent.write_commit_push(
        "Add x inside the link", folder if folder_on == "feature" else file
    )
    parent.branch_switch("main")
    parent.write_commit_push(
        "Add x inside the link", folder if folder_on == "main" else file
    )
    pin_before = link_pin(parent, source.get_id())

    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {conflicted}, (
        "the merge must conflict at the type change inside the link"
    )

    with pytest.raises(NotSupportedError):
        parent.branch_merge_resolve_theirs(conflicted)
    assert parent.unresolved_paths() == {conflicted}, (
        "a refused resolve must leave the conflict unresolved"
    )

    if folder_on == "feature":
        parent.branch_merge_resolve_mine(conflicted)
        parent.commit("Merge feature")
        parent.push()

        clone = parent.clone(branch="main")
        with clone.open_file(conflicted, "r") as input_file:
            assert input_file.read() == "x is a file\n", (
                "a fresh clone of the merged revision must hold the file"
            )
    else:
        with pytest.raises(NotSupportedError):
            parent.branch_merge_resolve(conflicted)
        parent.branch_merge_abort()

        assert link_pin(parent, source.get_id()) == pin_before, (
            "aborting the merge must restore the link's pin"
        )
        assert parent.file_exists(f"{conflicted}/y.txt"), (
            "aborting the merge must restore the folder"
        )


@pytest.mark.smoke
def test_abort_restores_a_link_holding_a_nested_link(new_lore_repo):
    """Aborting a merge that conflicts inside a linked repository which holds a
    link of its own restores both pins and the conflicted file, and leaves the
    nested link's content in place."""
    b_mount, nested_mount = "b", "b/sub/c"
    parent, repo_b = make_parent_with_link(
        new_lore_repo, b_mount, {"b-root.txt": "base\n"}
    )
    repo_c = make_repo(new_lore_repo, {"c-data/inner.txt": "inner\n"})
    conflicted = f"{b_mount}/b-root.txt"

    # Branch creation does not make the branch in a nested linked repository, which pushing a
    # branch created over a nested link needs, so the nested link is added once branched.
    parent.branch_create("feature")
    parent.write_commit_push("Change B on feature", {conflicted: "theirs\n"})
    parent.branch_switch("main")
    parent.link_add(nested_mount, repo_c.get_id(), "/")
    parent.commit("Link C inside B")
    parent.push()
    parent.write_commit_push("Change B on main", {conflicted: "mine\n"})
    head_before = parent.branch_info().local_latest
    pins_before = {
        repo.get_id(): link_pin(parent, repo.get_id()) for repo in (repo_b, repo_c)
    }

    output = parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert _conflicted_count(output) == 1, (
        f"the merge must report the conflict inside the link, got:\n{output}"
    )

    parent.branch_merge_abort()

    assert parent.branch_info().local_latest == head_before, (
        "aborting the merge must leave the branch on its pre-merge revision"
    )
    pins_after = {repo_id: link_pin(parent, repo_id) for repo_id in pins_before}
    assert pins_after == pins_before, "aborting the merge must restore every pin"
    with parent.open_file(conflicted, "r") as input_file:
        assert input_file.read() == "mine\n", (
            "aborting the merge must restore the conflicted file"
        )
    assert_crr_clean(
        parent,
        expected_files_present=[f"{nested_mount}/c-data/inner.txt"],
        expected_files_absent=[],
        expected_link_registry=dict.fromkeys(pins_before, True),
    )


@pytest.mark.smoke
@pytest.mark.parametrize("link_only", [False, True], ids=["merge", "link"])
@pytest.mark.parametrize("resolve", [None, "theirs", "marked"])
def test_abort_restores_a_conflict_inside_a_link_mounted_from_a_folder(
    new_lore_repo, link_only, resolve
):
    """Aborting a merge, or only its link's merge, that conflicts inside a link mounted from a
    folder restores the conflicted file from the folder and the link's pin, and leaves no copy
    of either side, whether or not the conflict was resolved first."""
    link_path = "linked/repo"
    parent, source = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"src/data.txt": "base\n", "data.txt": "outside the mount\n"},
        source_path="src",
    )
    conflicted = f"{link_path}/data.txt"

    parent.branch_create("feature")
    parent.write_commit_push("Theirs", {conflicted: "theirs\n"})
    parent.branch_switch("main")
    parent.write_commit_push("Mine", {conflicted: "mine\n"})
    pin_before = link_pin(parent, source.get_id())

    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {conflicted}, (
        "the merge must conflict on the file both branches changed inside the link"
    )
    if resolve == "theirs":
        parent.branch_merge_resolve_theirs(conflicted)
    elif resolve == "marked":
        parent.write_files({conflicted: "merged\n"})
        parent.branch_merge_resolve(conflicted)

    parent.branch_merge_abort(link=link_path if link_only else None)

    assert link_pin(parent, source.get_id()) == pin_before, (
        "aborting the merge must restore the link's pin"
    )
    assert sorted(os.listdir(os.path.join(parent.path, link_path))) == ["data.txt"]
    with parent.open_file(conflicted, "r") as input_file:
        assert input_file.read() == "mine\n", (
            "aborting the merge must restore the conflicted file"
        )


@pytest.mark.smoke
@pytest.mark.parametrize("staging", [True, False], ids=["stage", "mark"])
def test_resolving_the_whole_tree_reaches_into_links(new_lore_repo, staging):
    """Resolving the whole tree, staging a side or marking what is on disk
    resolved, resolves a conflict inside a link mounted from a folder and, once
    the merge is started again to merge the parent, one in the parent, and
    passes over a link the merge did not touch."""
    link_path = "linked/repo"
    parent, _source = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"src/data.txt": "base\n", "data.txt": "outside the mount\n"},
        {"root.txt": "base\n"},
        source_path="src",
    )
    untouched = make_repo(new_lore_repo, {"other.txt": "other\n"})
    parent.link_add("vendor/other", untouched.get_id(), "/")
    parent.commit("Add a link no branch changes")
    parent.push()
    untouched_pin = link_pin(parent, untouched.get_id())
    conflicted = ["root.txt", f"{link_path}/data.txt"]
    expected = "mine\n" if staging else "merged\n"

    def resolve_whole_tree(path: str) -> None:
        if staging:
            parent.branch_merge_resolve_mine(".")
        else:
            parent.write_files({path: expected})
            parent.branch_merge_resolve(".")

    parent.branch_create("feature")
    parent.write_commit_push("Theirs", dict.fromkeys(conflicted, "theirs\n"))
    parent.branch_switch("main")
    parent.write_commit_push("Mine", dict.fromkeys(conflicted, "mine\n"))

    # A conflicted link merge holds back the parent's merge until it is resolved.
    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {f"{link_path}/data.txt"}, (
        "the merge must conflict on the file both branches changed inside the link"
    )
    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {f"{link_path}/data.txt"}, (
        "starting the merge again must stop while the link holds a conflict"
    )
    resolve_whole_tree(f"{link_path}/data.txt")

    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {"root.txt"}, (
        "starting the merge again must conflict on the file both branches changed"
    )
    resolve_whole_tree("root.txt")
    parent.commit("Merge feature")
    parent.push()

    clone = parent.clone(branch="main")
    for path in conflicted:
        with clone.open_file(path, "r") as input_file:
            assert input_file.read() == expected, (
                f"the merged revision must hold the resolution of {path}"
            )
    assert link_pin(clone, untouched.get_id()) == untouched_pin, (
        "the untouched link must keep its pin"
    )


@pytest.mark.smoke
def test_resolving_a_file_against_a_folder_holding_a_link_is_refused(new_lore_repo):
    """A file on one branch meets a folder holding a link on the other. Taking
    the file, which would remove the link with the folder, is refused and keeps
    the link's content at its mount; aborting keeps the link."""
    parent = make_repo(new_lore_repo, {"root.txt": "root\n"})
    source = make_repo(new_lore_repo, {"link-file.txt": "from link\n"})

    parent.branch_create("feature")
    parent.write_commit_push("Add a file", {"thing": "a plain file\n"})

    parent.branch_switch("main")
    parent.link_add("thing/lib", source.get_id(), "/")
    parent.commit("Add a link inside a folder")
    parent.push()

    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {"thing"}, (
        "the merge must conflict at the path both branches added"
    )

    with pytest.raises(NotSupportedError):
        parent.branch_merge_resolve_theirs("thing")
    assert parent.unresolved_paths() == {"thing"}, (
        "a refused resolve must leave the conflict unresolved"
    )
    assert parent.compare_file(source, "thing/lib/link-file.txt", "link-file.txt"), (
        "a refused resolve must leave the link's content at its mount"
    )

    parent.branch_merge_abort()
    assert source.get_id() in parent.link_list(), "aborting must keep the link"


@pytest.mark.smoke
def test_a_refused_resolve_keeps_the_resolution_inside_a_link_named_with_it(
    new_lore_repo,
):
    """Resolving a conflict inside a link together with a file meeting a link
    elsewhere, edited since, is refused as a whole: the conflict inside the link
    keeps the resolution it had and the link keeps the pin the merge staged."""
    link_path = "linked/repo"
    parent, source = make_parent_with_link(
        new_lore_repo, link_path, {"data.txt": "base\n"}, {"root.txt": "root\n"}
    )
    other = make_repo(new_lore_repo, {"other.txt": "other\n"})

    parent.branch_create("feature")
    parent.write_files({f"{link_path}/data.txt": "theirs\n"})
    parent.stage(scan=True)
    parent.link_add("other", other.get_id(), "/")
    parent.commit("Change a file inside the link and add another link")
    parent.push()

    parent.branch_switch("main")
    parent.write_files({f"{link_path}/data.txt": "mine\n", "other": "a plain file\n"})
    parent.stage(scan=True)
    parent.commit("Change the same file and add a file where the other link lives")
    parent.push()

    # A conflicted link merge holds back the parent's merge until it is resolved.
    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {f"{link_path}/data.txt"}, (
        "the merge must conflict on the file both branches changed inside the link"
    )
    parent.branch_merge_resolve_mine(link_path)

    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {"other"}, (
        "starting the merge again must conflict at the path both branches added"
    )
    pin_before = link_pin(parent, source.get_id())
    with parent.open_file(f"{link_path}/data.txt", "r") as input_file:
        content_before = input_file.read()

    parent.write_files({"other": "a plain file edited\n"})
    with pytest.raises(LocalChanges):
        parent.branch_merge_resolve_theirs([f"{link_path}/data.txt", "other"])

    assert link_pin(parent, source.get_id()) == pin_before, (
        "a refused resolve must leave the link's pin as the merge staged it"
    )
    with parent.open_file(f"{link_path}/data.txt", "r") as input_file:
        assert input_file.read() == content_before, (
            "a refused resolve must leave the resolution inside the link on disk"
        )
    assert "other" in parent.unresolved_paths(), (
        "a refused resolve must leave the conflict unresolved"
    )


@pytest.mark.smoke
def test_link_merge_of_divergent_history_keeps_the_remote_history_off_a_local_file(
    new_lore_repo,
):
    """
    Both sides of a branch add a file inside a link after they diverge, and each takes the
    same node ID in the linked repository. Merging the branch's remote latest with links
    builds the link's merge from the local pin, and the remote's addition is not recorded
    on the local file, whose history is its own addition.
    """
    link_path = "linked/repo"
    repo, link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"a.txt": "a\n"}, {"p.txt": "p\n"}
    )
    clone = repo.clone()

    with repo.open_file(f"{link_path}/remote.txt", "w+") as f:
        f.write("remote\n")
    repo.stage(f"{link_path}/remote.txt")
    repo.commit("Add a remote file in the link", non_interactive=True)
    repo.push()

    with clone.open_file(f"{link_path}/local.txt", "w+") as f:
        f.write("local\n")
    clone.stage(f"{link_path}/local.txt")
    clone.commit("Add a local file in the link", non_interactive=True, local=True)
    local_pin = link_pin(clone, link_repo.get_id())

    clone.branch_merge(
        "main", message="Merge divergent history with links", non_interactive=True
    )

    history = parse_jsonl(
        clone.file_history(f"{link_path}/local.txt", json=True), "fileHistory"
    )
    assert [(e["revision"], e["path"], e["action"]) for e in history] == [
        (local_pin, "local.txt", "add")
    ]


@pytest.mark.smoke
def test_sync_of_divergent_history_records_a_link_the_local_history_repins(new_lore_repo):
    """
    A sync merging the remote latest into divergent local history that repins a link records the
    link's new pin, its change from the first parent, as a change of the link node.
    """
    link_path = "linked/repo"
    repo, link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"a.txt": "a\n"}, {"p.txt": "p\n"}
    )
    clone = repo.clone()
    link_clone = link_repo.clone()
    link_clone.write_commit_push("Advance the linked repository", {"b.txt": "b\n"})
    repo.write_commit_push("Add a remote file", {"remote.txt": "remote\n"})

    clone.link_update(link_path, pin=link_clone.branch_info().local_latest)
    clone.commit("Repin the link", non_interactive=True, local=True)
    clone.sync()

    deltas = parse_jsonl(
        clone.revision_info(delta=True, json=True, offline=True), "revisionInfoDelta"
    )
    assert [
        (delta["action"], delta["path"], delta["flagModify"])
        for delta in deltas
        if delta["path"] == link_path
    ] == [("keep", link_path, True)], deltas


@pytest.mark.smoke
def test_link_merge_of_divergent_history_records_the_change_from_the_remote_pin(
    new_lore_repo,
):
    """
    Merging a branch's divergent history with links builds the link's merge revision from the
    local pin, and the link's merge revision records, as its delta, its change from its first
    parent, the pin the remote holds: the local addition, and not the remote's, which that pin
    holds.
    """
    link_path = "linked/repo"
    repo, link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"a.txt": "a\n"}, {"p.txt": "p\n"}
    )
    clone = repo.clone()

    repo.write_files({f"{link_path}/remote.txt": "remote\n"})
    repo.stage(f"{link_path}/remote.txt")
    repo.commit("Add a remote file in the link", non_interactive=True)
    repo.push()

    clone.write_files({f"{link_path}/local.txt": "local\n"})
    clone.stage(f"{link_path}/local.txt")
    clone.commit("Add a local file in the link", non_interactive=True, local=True)
    clone.branch_merge(
        "main", message="Merge divergent history with links", non_interactive=True
    )
    clone.push()

    link_repo.sync()
    deltas = parse_jsonl(
        link_repo.revision_info(delta=True, json=True, offline=True), "revisionInfoDelta"
    )
    assert [
        (delta["action"], delta["path"]) for delta in deltas if delta["flagFile"]
    ] == [("add", "local.txt")], deltas


# ---------------------------------------------------------------------------
# Local work inside a link: a link merge verifies each path where the mount
# materializes it, and refuses to overwrite it as a plain merge does.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
@pytest.mark.parametrize(
    "nested, source_path, link_files, incoming, local",
    [
        pytest.param(
            False,
            "/",
            {"a.txt": "base\n"},
            {"a.txt": "theirs\n"},
            "a.txt",
            id="changed-file",
        ),
        pytest.param(
            False,
            "sub",
            {"sub/a.txt": "base\n"},
            {"a.txt": "theirs\n"},
            "a.txt",
            id="changed-file-subtree-mount",
        ),
        pytest.param(
            True,
            "/",
            {"a.txt": "base\n"},
            {"a.txt": "theirs\n"},
            "a.txt",
            id="changed-file-nested-link",
        ),
        pytest.param(
            False,
            "/",
            {"item/inner.txt": "base\n"},
            {"item": "theirs\n"},
            "item/inner.txt",
            id="folder-replaced-by-file",
        ),
        pytest.param(
            False,
            "/",
            {"keep.txt": "keep\n"},
            {"b.txt": "theirs\n"},
            "b.txt",
            id="untracked-file-added",
        ),
    ],
)
def test_link_merge_start_refuses_local_work_inside_the_link(
    new_lore_repo, nested, source_path, link_files, incoming, local
):
    """`merge start` refuses a merge that would overwrite a local edit or an untracked file
    inside a link and starts nothing, so it merges once the local work is reset.

    `incoming` is written below the mount on main, the merged branch, replacing a folder
    where it names a file; `local` is the path below the mount edited on disk first.
    """
    if nested:
        repo, _ = make_parent_with_link(
            new_lore_repo, "linked/repo", {"outer.txt": "o\n"}
        )
        mount = "linked/repo/inner"
        inner = make_repo(new_lore_repo, link_files)
        repo.link_add(mount, inner.get_id(), source_path)
        repo.commit("Link a repository inside the link")
        repo.push()
    else:
        mount = "linked/repo"
        repo, _ = make_parent_with_link(
            new_lore_repo, mount, link_files, source_path=source_path
        )

    # The change lands on main from a clone, so `repo` never switches across it.
    repo.branch_create("feature-branch")
    upstream = repo.clone(branch="main")
    for path in incoming:
        upstream.rmtree(f"{mount}/{path}")
    upstream.write_files(
        {f"{mount}/{path}": content for path, content in incoming.items()}
    )
    upstream.stage(scan=True)
    upstream.commit("Change inside the link")
    upstream.push()

    edited = f"{mount}/{local}"
    repo.write_files({edited: "local edit\n"})

    with pytest.raises(LocalChanges):
        repo.branch_merge_start("main", message="Merge main")
    with repo.open_file(edited) as f:
        assert f.read() == "local edit\n", f"{edited} should keep the local edit"

    repo.reset(edited, purge=True)
    repo.branch_merge_start("main", no_commit=True)
    for path, content in incoming.items():
        with repo.open_file(f"{mount}/{path}") as f:
            assert f.read() == content, f"{mount}/{path} should hold the merge"


@pytest.mark.smoke
@pytest.mark.skipif(sys.platform == "win32", reason="Windows holds no executable bit")
def test_merge_refused_at_a_link_pin_leaves_the_bits(new_lore_repo):
    """`merge start` refused at its check of a fixed link's pin, for a local edit inside the
    link, changes no executable bit there: a file the pin's move gives the bit, whose content
    the working tree holds, keeps its mode. With the edit reset, the merge gives it the bit."""
    repo = make_repo(new_lore_repo, {"main.txt": "base\n"})
    fixed = make_repo(new_lore_repo, {"a.txt": "base\n", "tool.sh": "tool\n"})
    repo.link_add("linked/fixed", fixed.get_id(), "/", disable_branching=True)
    repo.commit("Add a fixed link")
    repo.push()

    repo.branch_create("feature-branch")
    fixed.write_files({"a.txt": "theirs\n"})
    os.chmod(os.path.join(fixed.path, "tool.sh"), 0o755)
    fixed.stage(["a.txt", "tool.sh"])
    fixed.commit("Change the fixed link")
    fixed.push()
    upstream = repo.clone(branch="main")
    upstream.link_update("linked/fixed")
    upstream.write_files({"main.txt": "theirs\n"})
    upstream.stage(["main.txt"])
    upstream.commit("Move the fixed link's pin")
    upstream.push()

    repo.write_files({"linked/fixed/a.txt": "local edit\n"})
    tool = os.path.join(repo.path, "linked/fixed/tool.sh")

    with pytest.raises(LocalChanges):
        repo.branch_merge_start("main", message="Merge main")
    mode = os.stat(tool).st_mode
    assert not mode & stat.S_IXUSR, f"{mode:o}"

    repo.reset("linked/fixed/a.txt", purge=True)
    repo.branch_merge_start("main", message="Merge main")
    mode = os.stat(tool).st_mode
    assert mode & stat.S_IXUSR, f"{mode:o}"


@pytest.mark.smoke
@pytest.mark.skipif(sys.platform == "win32", reason="Windows holds no executable bit")
@pytest.mark.parametrize("edited", ["one", "two"])
def test_merge_refused_at_another_link_pin_leaves_the_bits(new_lore_repo, edited):
    """`merge start` refused at its check of one fixed link's pin, for a local edit inside that
    link, changes no executable bit in another fixed link whose pin's move only gives a file the
    bit, whichever of the two the check reaches first. With the edit reset, the merge gives the
    file the bit."""
    repo = make_repo(new_lore_repo, {"main.txt": "base\n"})
    links = {
        name: make_repo(new_lore_repo, {"a.txt": "base\n", "tool.sh": "tool\n"})
        for name in ("one", "two")
    }
    for name, link in links.items():
        repo.link_add(f"linked/{name}", link.get_id(), "/", disable_branching=True)
    repo.commit("Add two fixed links")
    repo.push()

    repo.branch_create("feature-branch")
    moved = "two" if edited == "one" else "one"
    links[edited].write_commit_push("Edit the file", {"a.txt": "theirs\n"})
    os.chmod(os.path.join(links[moved].path, "tool.sh"), 0o755)
    links[moved].stage(["tool.sh"])
    links[moved].commit("Give the tool the executable bit")
    links[moved].push()
    upstream = repo.clone(branch="main")
    for name in links:
        upstream.link_update(f"linked/{name}")
    upstream.write_files({"main.txt": "theirs\n"})
    upstream.stage(["main.txt"])
    upstream.commit("Move both fixed links' pins")
    upstream.push()

    repo.write_files({f"linked/{edited}/a.txt": "local edit\n"})
    tool = os.path.join(repo.path, f"linked/{moved}/tool.sh")

    with pytest.raises(LocalChanges):
        repo.branch_merge_start("main", message="Merge main")
    mode = os.stat(tool).st_mode
    assert not mode & stat.S_IXUSR, f"{mode:o}"

    repo.reset(f"linked/{edited}/a.txt", purge=True)
    repo.branch_merge_start("main", message="Merge main")
    mode = os.stat(tool).st_mode
    assert mode & stat.S_IXUSR, f"{mode:o}"


@pytest.mark.smoke
@pytest.mark.parametrize(
    "edited",
    ["linked/b/a.txt", "main.txt", "linked/fixed/a.txt"],
    ids=["second-link", "main-repository", "fixed-link-pin"],
)
def test_merge_start_refuses_local_work_before_merging_any_link(new_lore_repo, edited):
    """`merge start` checks every link, the main repository and the pins it carries before
    merging any of them, so local work in one refuses the merge with nothing merged, and it
    merges once the local work is reset."""
    repo, _ = make_parent_with_link(
        new_lore_repo, "linked/a", {"a.txt": "base\n"}, {"main.txt": "base\n"}
    )
    second = make_repo(new_lore_repo, {"a.txt": "base\n"})
    repo.link_add("linked/b", second.get_id(), "/")
    fixed = make_repo(new_lore_repo, {"a.txt": "base\n"})
    repo.link_add("linked/fixed", fixed.get_id(), "/", disable_branching=True)
    repo.commit("Add a second link and a fixed link")
    repo.push()

    repo.branch_create("feature-branch")
    fixed.write_commit_push("Change the fixed link", {"a.txt": "theirs\n"})
    upstream = repo.clone(branch="main")
    upstream.link_update("linked/fixed")
    upstream.write_files(
        {path: "theirs\n" for path in ["linked/a/a.txt", "linked/b/a.txt", "main.txt"]}
    )
    upstream.stage(scan=True)
    upstream.commit("Change both links, the main repository and the fixed link's pin")
    upstream.push()
    changed = ["linked/a/a.txt", "linked/b/a.txt", "main.txt", "linked/fixed/a.txt"]

    repo.write_files({edited: "local edit\n"})

    with pytest.raises(LocalChanges):
        repo.branch_merge_start("main", message="Merge main")
    for path in changed:
        expected = "local edit\n" if path == edited else "base\n"
        with repo.open_file(path) as f:
            assert f.read() == expected, f"{path} should hold {expected!r}"

    repo.reset(edited, purge=True)
    repo.branch_merge_start("main", message="Merge main")
    for path in changed:
        with repo.open_file(path) as f:
            assert f.read() == "theirs\n", f"{path} should hold the merge"
