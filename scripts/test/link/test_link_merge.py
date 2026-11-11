# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os
import re
import stat
import sys
from collections.abc import Callable

import pytest
from error_types import (
    LinkMountDivergedError,
    LinkPinDivergedError,
    LocalChanges,
    NotSupportedError,
    UnresolvedConflictError,
)
from link_helpers import (
    assert_crr_clean,
    assert_mount_exposes,
    assert_mount_source,
    link_pin,
    make_parent_with_link,
    make_parent_with_remount,
    make_repo,
    make_repo_renumbering_folder,
    remote_branch_entries,
    remount,
)
from lore_parsers import parse_jsonl, parse_status_json
from test_utils import unstaged_entries

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
    new_lore_repo, link_path: str, **link_add_options
) -> tuple[Lore, Lore]:
    """Parent repo on `main`, link added and committed on `feature-branch` only."""
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})

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
    counterpart in the linked repository, so the merge has to give it one, even where the
    repository is already mounted elsewhere without following.
    """
    link_path = "linked/repo"
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(
        new_lore_repo,
        {"shared/link-file.txt": "link source content\n", "pinned/p.txt": "p\n"},
    )
    repo.link_add(
        "vendor/pinned", source_repo.get_id(), "pinned", disable_branching=True
    )
    repo.commit("Mount the repository without following")
    repo.push()

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
    """`--ignore-links` creates no branch in a linked repository a merge brings in."""
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})

    repo.branch_create("feature-branch")
    repo.write_commit_push("Feature change", {"feature-file.txt": "feature\n"})
    repo.branch_switch("main")
    repo.link_add("linked/repo", source_repo.get_id(), "/")
    repo.commit("Add link on main")
    repo.push()

    repo.branch_switch("feature-branch")
    repo.branch_merge_start("main", message="Merge main", ignore_links=True)

    branches = sorted(entry["name"] for entry in remote_branch_entries(source_repo))
    assert branches == ["main"], (
        f"The linked repository should still hold only 'main'.\nGot: {branches}"
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

    link_output = repo.link_list()
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Flags:\s+DisableAutoFollow \(0x1\)",
        link_output,
        re.DOTALL,
    ), f"Merged link should keep the DisableAutoFollow flag.\nGot: {link_output}"
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

    link_output = repo.link_list()
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Source path:\s+/", link_output, re.DOTALL
    ), f"Merged link should still mount the linked repository root.\nGot: {link_output}"

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


@pytest.mark.smoke
def test_link_pin_update_divergent_does_not_silently_pick_a_side(new_lore_repo):
    """Both branches moving a link pin stops the merge instead of resolving it.

    Divergent rows are a parent-level conflict. Until they can be resolved in
    place the merge refuses, so neither side's row is adopted silently.
    """
    link_path = "linked/repo"
    repo, source_repo, revision_two, _revision_three = _diverge_link_pins(
        new_lore_repo, link_path
    )

    with pytest.raises(LinkPinDivergedError):
        repo.branch_merge_start("feature-branch", message="Merge divergent link pins")

    assert link_pin(repo, source_repo.get_id()) == revision_two, (
        "A refused merge should leave main's pin alone"
    )


@pytest.mark.smoke
def test_link_pin_update_divergent_creates_no_branch_for_a_merged_in_link(
    new_lore_repo,
):
    """A merge refused for a divergent pin leaves the linked repositories it brings in alone."""
    repo, _source_repo, _revision_two, _revision_three = _diverge_link_pins(
        new_lore_repo, "linked/repo"
    )
    other_repo = make_repo(new_lore_repo, {"other.txt": "other\n"})
    repo.link_add("linked/other", other_repo.get_id(), "/")
    repo.commit("Add a following link on main")
    repo.push()

    repo.branch_switch("feature-branch")
    with pytest.raises(LinkPinDivergedError):
        repo.branch_merge_start("main", message="Merge divergent link pins")

    branches = sorted(entry["name"] for entry in remote_branch_entries(other_repo))
    assert branches == ["main"], (
        f"The merged-in linked repository should still hold only 'main'.\nGot: {branches}"
    )


@pytest.mark.smoke
def test_link_pin_update_divergent_detected_with_ignore_links(new_lore_repo):
    """`--ignore-links` skips link content work but not the row comparison.

    The parent's link list is parent state, so a row both branches moved is a
    parent merge conflict that the flag does not suppress.
    """
    link_path = "linked/repo"
    repo, source_repo, revision_two, _revision_three = _diverge_link_pins(
        new_lore_repo, link_path
    )

    with pytest.raises(LinkPinDivergedError):
        repo.branch_merge_start(
            "feature-branch", message="Merge divergent link pins", ignore_links=True
        )

    assert link_pin(repo, source_repo.get_id()) == revision_two, (
        "A refused merge should leave main's pin alone"
    )

    repo.branch_merge_abort()


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
def test_link_remount_divergent_does_not_silently_pick_a_side(new_lore_repo):
    """Both branches remounting a link stops the merge instead of resolving it.

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

    with pytest.raises(LinkMountDivergedError):
        repo.branch_merge_start("feature-branch", message="Merge divergent remounts")

    assert_mount_exposes(repo, link_path, "first", ["a.txt"])


# ---------------------------------------------------------------------------
# A link replacing a committed folder, merged in both directions.
# ---------------------------------------------------------------------------


def _folder_replaced_by_a_link(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent on `main` holding `shared/`, plus a `feature` branch where a link
    replaced that folder with a repository of its own.

    The linked copy is byte-identical to the folder it replaces, which is what
    moving a folder out into its own repository leaves behind.
    """
    parent = make_repo(
        new_lore_repo,
        {
            "root.txt": "root\n",
            "shared/a.txt": "a original\n",
            "shared/b.txt": "b original\n",
        },
    )

    source = make_repo(
        new_lore_repo,
        {
            "a.txt": "a original\n",
            "b.txt": "b original\n",
        },
    )

    parent.branch_create("feature")
    parent.rmtree("shared")
    parent.stage("shared", scan=True)
    parent.link_add("shared", source.get_id(), "/")
    parent.commit("Replace shared/ with a link")
    parent.push()

    return parent, source


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


def _change_inside_the_folder(parent: Lore) -> None:
    """Modify a file inside `shared/` and add another."""
    with parent.open_file("shared/a.txt", "w+") as output_file:
        output_file.writelines(["a changed on main\n"])
    with parent.open_file("shared/c.txt", "w+") as output_file:
        output_file.writelines(["c added on main\n"])
    parent.stage(scan=True)
    parent.commit("Change files under shared/")
    parent.push()


def _conflicted_paths(parent: Lore) -> list[str]:
    """The paths `status` reports as conflicted."""
    return [
        entry["path"]
        for entry in parse_status_json(parent.status(json=True))
        if entry.get("flagConflict")
    ]


def _assert_mount_intact(parent: Lore, source: Lore, pin: str) -> None:
    """The mount is still a link on the pin it held, serving its own content."""
    info = parent.link_info("shared")
    assert source.get_id() in info, (
        f"the mount must still name the linked repository:\n{info}"
    )
    assert "Link path: shared" in info, f"the mount must still be a link:\n{info}"
    assert f"Revision: {pin}" in info, f"the mount must still hold its pin:\n{info}"
    assert parent.compare_file(source, "shared/b.txt", "b.txt"), (
        "a file only the linked repository holds must still be served at the mount"
    )


def _assert_mount_conflict(
    parent: Lore,
    merged_branch: str,
    message: str,
    while_conflicted: Callable[[], None] | None = None,
) -> None:
    """Merging `merged_branch` conflicts at the mount, commits nothing, and aborts cleanly.

    `while_conflicted` runs once the conflict stands, before the commit is attempted.
    """
    head_before = parent.branch_info().local_latest

    output = parent.branch_merge_start(merged_branch, message=message, check=False)

    assert _conflicted_count(output) == 1, (
        f"the merge must report the replaced mount as its one conflict, got:\n{output}"
    )
    assert _conflicted_paths(parent) == ["shared"], (
        f"the conflict must be reported at the mount, got {_conflicted_paths(parent)}"
    )
    assert parent.branch_info().local_latest == head_before, (
        "the merge must leave the branch on its pre-merge revision"
    )

    if while_conflicted:
        while_conflicted()

    with pytest.raises(UnresolvedConflictError):
        parent.commit(message)

    parent.branch_merge_abort()

    assert parent.branch_info().local_latest == head_before, (
        "aborting the merge must leave the branch on its pre-merge revision"
    )


@pytest.mark.smoke
def test_branch_diff_reports_a_link_replacing_a_folder(new_lore_repo):
    """Replacing a committed folder with a link is a type change at the mount,
    and a diff reports it in both directions of the replacement."""
    parent, _source = _folder_replaced_by_a_link(new_lore_repo)

    folder_to_link = parent.branch_diff("main", source="feature")
    assert _change_actions(folder_to_link, "shared") == {"A", "D"}, (
        "a folder replaced by a link must be reported as replaced, "
        f"got:\n{folder_to_link}"
    )

    parent.branch_create("restored")
    parent.link_remove("shared")
    parent.commit("Remove the link")
    parent.make_dirs("shared")
    with parent.open_file("shared/a.txt", "w+") as output_file:
        output_file.writelines(["a restored\n"])
    with parent.open_file("shared/b.txt", "w+") as output_file:
        output_file.writelines(["b restored\n"])
    parent.stage(scan=True)
    parent.commit("Restore the folder")
    parent.push()

    link_to_folder = parent.branch_diff("feature", source="restored")
    assert _change_actions(link_to_folder, "shared") == {"A", "D"}, (
        "a link replaced by a folder must be reported as replaced, "
        f"got:\n{link_to_folder}"
    )


@pytest.mark.smoke
def test_merge_outside_the_folder_keeps_a_link_replacing_it(new_lore_repo):
    """A branch that touched nothing under the folder merges cleanly into the
    branch where a link replaced it, and the mount survives with its pin."""
    parent, source = _folder_replaced_by_a_link(new_lore_repo)
    pin = link_pin(parent, source.get_id())

    parent.branch_switch("main")
    with parent.open_file("root.txt", "w+") as output_file:
        output_file.writelines(["root changed on main\n"])
    parent.stage(scan=True)
    parent.commit("Change a file outside shared/")
    parent.push()

    parent.branch_switch("feature")
    parent.branch_merge_start("main", message="Merge main")
    parent.push()

    _assert_mount_intact(parent, source, pin)
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
    parent, source = _folder_replaced_by_a_link(new_lore_repo)
    pin = link_pin(parent, source.get_id())

    parent.branch_switch("main")
    parent.branch_merge_start("feature", message="Merge feature")
    parent.push()

    _assert_mount_intact(parent, source, pin)
    assert "Verified repository state integrity" in parent.repository_verify(), (
        "the merged revision must verify"
    )

    clone = parent.clone(branch="main")
    assert source.get_id() in clone.link_info("shared"), (
        "a fresh clone of the merged revision must hold the link"
    )


@pytest.mark.smoke
def test_merge_of_a_link_replacing_a_deleted_folder_conflicts(new_lore_repo):
    """A branch that deleted the folder outright still meets the replacement at
    the mount, where neither side holds what the other names."""
    parent, _source = _folder_replaced_by_a_link(new_lore_repo)

    parent.branch_switch("main")
    parent.rmtree("shared")
    parent.stage("shared", scan=True)
    parent.commit("Delete shared/")
    parent.push()

    _assert_mount_conflict(parent, "feature", "Merge feature")

    assert not parent.path_exists("shared"), (
        "aborting the merge must leave the deleted folder deleted"
    )


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("resolve", "merged_branch"),
    [
        pytest.param(
            lambda parent: parent.branch_merge_resolve_theirs("shared"),
            "main",
            id="stage_at_the_mount",
        ),
        pytest.param(
            lambda parent: parent.branch_merge_resolve_theirs("shared/a.txt"),
            "main",
            id="stage_below_the_mount",
        ),
        pytest.param(
            lambda parent: parent.branch_merge_resolve_mine("."),
            "feature",
            id="stage_walk",
        ),
        pytest.param(
            lambda parent: parent.branch_merge_resolve("shared"),
            "feature",
            id="merge_at_the_mount",
        ),
        pytest.param(
            lambda parent: parent.branch_merge_resolve(),
            "main",
            id="merge_walk",
        ),
    ],
)
def test_resolving_a_mount_conflict_is_refused_and_leaves_the_merge_open(
    new_lore_repo, resolve, merged_branch
):
    """Merging folder changes with a link replacing the folder conflicts at the
    mount in both directions of the replacement. A resolve naming the mount, a
    path below it or walking to it, staging a side or marking it resolved, is
    refused with nothing but the error, keeps what the branch holds at the mount,
    and leaves the merge open to abort."""
    parent, source = _folder_replaced_by_a_link(new_lore_repo)
    pin = link_pin(parent, source.get_id())

    parent.branch_switch("main")
    _change_inside_the_folder(parent)
    if merged_branch == "main":
        parent.branch_switch("feature")

    def assert_branch_side_kept():
        if merged_branch == "main":
            _assert_mount_intact(parent, source, pin)
        else:
            with parent.open_file("shared/a.txt", "r") as input_file:
                assert "a changed on main" in input_file.read(), (
                    "the folder must keep the content the branch committed"
                )

    def refused_resolve():
        with pytest.raises(NotSupportedError) as refused:
            resolve(parent)
        output = str(refused.value).splitlines()[1]
        assert output.startswith("[Error]"), (
            f"a refused resolve must report the error first, got:\n{refused.value}"
        )
        assert parent.unresolved_paths() == {"shared"}, (
            "a refused resolve must leave the conflict at the mount unresolved"
        )
        assert_branch_side_kept()

    _assert_mount_conflict(
        parent,
        merged_branch,
        f"Merge {merged_branch}",
        while_conflicted=refused_resolve,
    )

    assert_branch_side_kept()


@pytest.mark.smoke
def test_resolving_a_cherry_picked_mount_conflict_is_refused(new_lore_repo):
    """Cherry-picking folder changes onto the branch where a link replaced the
    folder conflicts at the mount. Taking the picked side is refused, keeps the
    link, and leaves the cherry-pick open to abort."""
    parent, source = _folder_replaced_by_a_link(new_lore_repo)
    pin = link_pin(parent, source.get_id())

    parent.branch_switch("main")
    _change_inside_the_folder(parent)
    picked = parent.branch_info().local_latest
    parent.branch_switch("feature")
    head_before = parent.branch_info().local_latest

    parent.revision_cherry_pick(picked, check=False)
    assert parent.unresolved_paths() == {"shared"}, (
        "the cherry-pick must conflict at the mount"
    )

    with pytest.raises(NotSupportedError):
        parent.revision_cherry_pick_resolve_theirs("shared")
    assert parent.unresolved_paths() == {"shared"}, (
        "a refused resolve must leave the conflict at the mount unresolved"
    )
    _assert_mount_intact(parent, source, pin)

    parent.revision_cherry_pick_abort()
    assert parent.branch_info().local_latest == head_before, (
        "aborting the cherry-pick must leave the branch on its revision"
    )
    _assert_mount_intact(parent, source, pin)


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
def test_abort_restores_a_conflict_inside_a_link_mounted_from_a_folder(new_lore_repo):
    """Aborting a merge that conflicts inside a link mounted from a folder
    restores the conflicted file from the folder and the link's pin."""
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

    parent.branch_merge_abort()

    assert link_pin(parent, source.get_id()) == pin_before, (
        "aborting the merge must restore the link's pin"
    )
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
def test_resolving_the_whole_tree_reaches_into_each_mount_of_one_repository(
    new_lore_repo,
):
    """A linked repository mounted twice, from two folders, conflicts inside
    each mount. Resolving the whole tree resolves the conflicts of both."""
    source = make_repo(new_lore_repo, {"a/data.txt": "base\n", "b/data.txt": "base\n"})
    parent = make_repo(new_lore_repo, {"root.txt": "root\n"})
    parent.link_add("first", source.get_id(), "a")
    parent.link_add("second", source.get_id(), "b")
    parent.commit("Mount two folders of one repository")
    parent.push()
    conflicted = ["first/data.txt", "second/data.txt"]
    parent.branch_create("feature")
    parent.push()

    # Two mounts of one repository cannot both commit through the parent, so the
    # linked repository changes on its own and each branch of the parent repins both.
    for branch, content in (("feature", "theirs\n"), ("main", "mine\n")):
        source.branch_switch(branch)
        source.write_commit_push(
            f"Change both folders on {branch}",
            {"a/data.txt": content, "b/data.txt": content},
        )
        parent.branch_switch(branch)
        for mount in ("first", "second"):
            parent.link_update(mount)
            parent.commit(f"Repin {mount} on {branch}")
            parent.push()

    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == set(conflicted), (
        "the merge must conflict inside each mount"
    )

    parent.write_files(dict.fromkeys(conflicted, "merged\n"))
    parent.branch_merge_resolve(".")
    assert parent.unresolved_paths() == set(), (
        "resolving the whole tree must resolve the conflict inside each mount"
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
def test_resolving_a_file_against_a_link_takes_the_file_and_refuses_the_link(
    new_lore_repo,
):
    """A file on one branch meets a link at the same path on the other. Taking
    the link is refused and leaves the file and the conflict in place; taking
    the file commits a revision that verifies and clones with the file."""
    parent = make_repo(new_lore_repo, {"root.txt": "root\n"})
    source = make_repo(new_lore_repo, {"link-file.txt": "from link\n"})

    parent.branch_create("feature")
    parent.link_add("linked/repo", source.get_id(), "/")
    parent.commit("Add a link")
    parent.push()

    parent.branch_switch("main")
    parent.write_files({"linked/repo": "a plain file\n"})
    parent.stage(scan=True)
    parent.commit("Add a file where the link lives")
    parent.push()

    parent.branch_merge_start("feature", message="Merge feature", check=False)
    assert parent.unresolved_paths() == {"linked/repo"}, (
        "the merge must conflict at the path both branches added"
    )

    with pytest.raises(NotSupportedError):
        parent.branch_merge_resolve_theirs("linked/repo")
    assert parent.unresolved_paths() == {"linked/repo"}, (
        "a refused resolve must leave the conflict unresolved"
    )
    with parent.open_file("linked/repo", "r") as input_file:
        assert input_file.read() == "a plain file\n", (
            "a refused resolve must leave the file on disk"
        )

    parent.branch_merge_resolve_mine("linked/repo")
    parent.commit("Merge feature")
    parent.push()

    assert source.get_id() not in parent.link_list(), (
        "taking the file must not register the link"
    )
    assert "Verified repository state integrity" in parent.repository_verify(), (
        "the merged revision must verify"
    )
    clone = parent.clone(branch="main")
    with clone.open_file("linked/repo", "r") as input_file:
        assert input_file.read() == "a plain file\n", (
            "a fresh clone of the merged revision must hold the file"
        )


@pytest.mark.smoke
def test_a_refused_resolve_keeps_the_resolution_inside_a_link_named_with_it(
    new_lore_repo,
):
    """Resolving a conflict inside a link together with a file meeting a link
    elsewhere is refused as a whole: the conflict inside the link keeps the
    resolution it had and the link keeps the pin the merge staged."""
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

    with pytest.raises(NotSupportedError):
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
    if nested:
        # A merge in a nested link doesn't realize its files yet.
        return
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
