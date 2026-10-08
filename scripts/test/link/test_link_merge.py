# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import re

import pytest
from error_types import LinkPinDivergedError, UnresolvedConflictError
from link_helpers import assert_crr_clean, commit_initial_main, make_link_source
from lore_parsers import parse_status_json

from lore import Lore

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
    repo = commit_initial_main(new_lore_repo, "main-file.txt")
    source_repo, _ = make_link_source(new_lore_repo, ["link-file.txt"])

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

    pin_before = re.search(
        rf"{source_repo.get_id()}.*?Revision:\s*(\w+)", repo.link_list(), re.DOTALL
    )
    assert pin_before, "Merged link should have a pinned revision"

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

    pin_after = re.search(
        rf"{source_repo.get_id()}.*?Revision:\s*(\w+)", repo.link_list(), re.DOTALL
    )
    assert pin_after, "Link should still be in the registry after committing into it"
    assert pin_after.group(1) != pin_before.group(1), (
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
    repo = commit_initial_main(new_lore_repo, "main-file.txt")
    source_repo, _ = make_link_source(new_lore_repo, ["link-file.txt"])

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


def _link_pin(repo: Lore, source_id: str) -> str:
    """The revision `source_id` is pinned at, read from `link list`."""
    output = repo.link_list()
    match = re.search(rf"Link\s+{source_id}.*?Revision:\s*(\w+)", output, re.DOTALL)
    assert match, f"No pin for {source_id} in link list:\n{output}"
    return match.group(1)


def _fixed_link_pinned_on_main(new_lore_repo, link_path: str) -> tuple[Lore, Lore, str]:
    """Parent on main with a fixed link, and a newer revision to move it to.

    The link is added with `--disable-branching`, so its registry entry names a
    branch of the linked repository and its pin is plain revision data that a
    merge has to carry. The linked repository then gets a second revision, so
    the pin can move with nothing staged through the mount path.
    """
    repo = commit_initial_main(new_lore_repo, "main-file.txt")
    source_repo, _ = make_link_source(new_lore_repo, ["link-file.txt"])

    repo.link_add(link_path, source_repo.get_id(), "/", disable_branching=True)
    repo.commit("Add fixed link on main")
    repo.push()

    with source_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link source revision two\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Second source revision")
    source_repo.push()

    return repo, source_repo, _link_pin(repo, source_repo.get_id())


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

    pin_feature = _link_pin(repo, source_repo.get_id())
    assert pin_feature != pin_base, "link update should have moved the pin"

    repo.branch_switch("main")
    assert _link_pin(repo, source_repo.get_id()) == pin_base, (
        "Main should still hold the original pin before the merge"
    )

    repo.branch_merge_start("feature-branch", message="Merge link pin update")
    repo.push()

    assert _link_pin(repo, source_repo.get_id()) == pin_feature, (
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
    pin_feature = _link_pin(repo, source_repo.get_id())

    repo.branch_merge_into("main", message="Merge link pin update into main")
    repo.branch_switch("main")

    assert _link_pin(repo, source_repo.get_id()) == pin_feature, (
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
    repo = commit_initial_main(new_lore_repo, "main-file.txt")
    source_repo, _ = make_link_source(new_lore_repo, ["link-file.txt"])

    link_path = "linked/repo"
    repo.link_add(link_path, source_repo.get_id(), "/")
    repo.commit("Add auto-following link on main")
    repo.push()
    pin_main = _link_pin(repo, source_repo.get_id())

    # Advance the link through the mount path, which moves its pin onto the
    # linked repository's mirror of the feature branch.
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Add a file inside the link on feature branch")
    repo.push()
    assert _link_pin(repo, source_repo.get_id()) != pin_main, (
        "Committing into the link should have moved its pin on the feature branch"
    )

    repo.branch_merge_into("main", message="Merge feature branch into main")
    repo.branch_switch("main")

    assert _link_pin(repo, source_repo.get_id()) == pin_main, (
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

    assert _link_pin(repo, source_repo.get_id()) == revision_two, (
        "A refused merge should leave main's pin alone"
    )

    repo.branch_merge_abort()
    assert _link_pin(repo, source_repo.get_id()) == revision_two, (
        "Aborting should leave main's pin alone"
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

    assert _link_pin(repo, source_repo.get_id()) == revision_two, (
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
    assert _link_pin(repo, source_repo.get_id()) == pin_base, (
        "--ignore-links should not move the link pin"
    )


# ---------------------------------------------------------------------------
# A link replacing a committed folder, merged in both directions.
# ---------------------------------------------------------------------------


def _folder_replaced_by_a_link(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent on `main` holding `shared/`, plus a `feature` branch where a link
    replaced that folder with a repository of its own.

    The linked copy is byte-identical to the folder it replaces, which is what
    moving a folder out into its own repository leaves behind.
    """
    parent: Lore = new_lore_repo()
    parent.make_dirs("shared")
    with parent.open_file("root.txt", "w+") as output_file:
        output_file.writelines(["root\n"])
    with parent.open_file("shared/a.txt", "w+") as output_file:
        output_file.writelines(["a original\n"])
    with parent.open_file("shared/b.txt", "w+") as output_file:
        output_file.writelines(["b original\n"])
    parent.stage(scan=True)
    parent.commit("Initial main")
    parent.push()

    source: Lore = new_lore_repo()
    with source.open_file("a.txt", "w+") as output_file:
        output_file.writelines(["a original\n"])
    with source.open_file("b.txt", "w+") as output_file:
        output_file.writelines(["b original\n"])
    source.stage(scan=True)
    source.commit("Initial source")
    source.push()

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


def _assert_mount_conflict(parent: Lore, merged_branch: str, message: str) -> None:
    """Merging `merged_branch` conflicts at the mount, commits nothing, and aborts cleanly."""
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
def test_merge_of_folder_changes_into_a_link_conflicts_at_the_mount(new_lore_repo):
    """Merging a branch that changed files under the folder into the branch
    where a link replaced it conflicts at the mount and commits nothing."""
    parent, source = _folder_replaced_by_a_link(new_lore_repo)
    pin = _link_pin(parent, source.get_id())

    parent.branch_switch("main")
    _change_inside_the_folder(parent)

    parent.branch_switch("feature")

    _assert_mount_conflict(parent, "main", "Merge main")

    _assert_mount_intact(parent, source, pin)


@pytest.mark.smoke
def test_merge_outside_the_folder_keeps_a_link_replacing_it(new_lore_repo):
    """A branch that touched nothing under the folder merges cleanly into the
    branch where a link replaced it, and the mount survives with its pin."""
    parent, source = _folder_replaced_by_a_link(new_lore_repo)
    pin = _link_pin(parent, source.get_id())

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
def test_merge_of_a_link_replacing_a_folder_lands_the_link(new_lore_repo):
    """Merging the branch where a link replaced the folder into the branch that
    still holds the folder applies the replacement."""
    parent, source = _folder_replaced_by_a_link(new_lore_repo)
    pin = _link_pin(parent, source.get_id())

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
def test_merge_of_a_link_replacing_a_changed_folder_conflicts(new_lore_repo):
    """Merging the branch where a link replaced the folder into a branch that
    changed files under it conflicts at the mount and commits nothing."""
    parent, source = _folder_replaced_by_a_link(new_lore_repo)

    parent.branch_switch("main")
    _change_inside_the_folder(parent)

    _assert_mount_conflict(parent, "feature", "Merge feature")

    with parent.open_file("shared/a.txt", "r") as input_file:
        assert "a changed on main" in input_file.read(), (
            "the folder must keep the content the branch committed"
        )


@pytest.mark.smoke
def test_merge_of_a_link_replacing_a_deleted_folder_conflicts(new_lore_repo):
    """A branch that deleted the folder outright still meets the replacement at
    the mount, where neither side holds what the other names."""
    parent, source = _folder_replaced_by_a_link(new_lore_repo)

    parent.branch_switch("main")
    parent.rmtree("shared")
    parent.stage("shared", scan=True)
    parent.commit("Delete shared/")
    parent.push()

    _assert_mount_conflict(parent, "feature", "Merge feature")

    assert not parent.path_exists("shared"), (
        "aborting the merge must leave the deleted folder deleted"
    )
