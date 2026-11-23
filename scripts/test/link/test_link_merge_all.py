# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT

import os
from dataclasses import dataclass

import pytest
from error_types import (
    ImproperArgumentsError,
    NotInProgress,
    NotSupportedError,
    UnresolvedConflictError,
)
from link_helpers import (
    LINK_MOVES,
    LINKS_TO_ONE_REPOSITORY,
    MERGE_LINKS,
    SUBTREE_LINK,
    assert_link_holds,
    assert_linked_main_merged_settings,
    assert_merge_settled,
    build_link_chain,
    build_nested_link_repos_at,
    change_through_both_links,
    diverged_link_branches,
    link_entry,
    link_pin,
    link_pin_at,
    link_pins,
    links_to_one_repository,
    make_parent_with_link,
    make_repo,
    merge_a_branch_moving_a_link,
    merge_a_branch_removing_a_link,
    mount_part_inside_the_second_link_on_feature,
    mount_listing,
    mounted_files,
    parent_with_a_link_merging_into_a_failure,
    repo_holding_links,
    resolve_the_only_conflict,
    setup_link_merge_conflict,
    start_the_merge_failing_at_the_second_link,
    subtree_link_with_outside_change,
    unmount_the_part_inside_the_second_link,
)
from lore_parsers import parse_jsonl, parse_status_json
from test_utils import registered_links, revision_metadata_keys

from lore import Lore

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)


@pytest.mark.smoke
def test_link_merge_all_conflict_in_a_link_exposing_a_subtree(new_lore_repo):
    """A default merge marks a conflict in a link exposing a subtree at the mount.

    The link draws `content/assets` and materializes it at `linked/repo`, so the linked
    repository's own path for the conflicted file is not the one on disk. The conflict is named
    by its node below the subtree the link exposes, and the markers are written from the mount.
    """
    urc, _link_repo, link_path = setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {
                "path": "shader.hlsl",
                "base": "base\n",
                "mine": "mine content\n",
                "theirs": "theirs content\n",
            }
        ],
        source_path="content/assets",
    )

    urc.branch_merge_start(
        "feature-branch", message="Merge with a subtree link conflict"
    )

    conflict_file = f"{link_path}/shader.hlsl"
    assert urc.file_exists(conflict_file), (
        "Expected the conflicted file to be marked at the mount"
    )
    assert not urc.file_exists(f"{link_path}/content/assets/shader.hlsl"), (
        "The linked repository's own spelling was realized below the mount"
    )

    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "<<<<<<<" in content or ">>>>>>>" in content, (
        f"Expected conflict markers at the mount path, got:\n{content}"
    )

    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["resolved through the mount\n"])

    urc.branch_merge_resolve(conflict_file)
    urc.commit("Merge with a conflict resolved in a subtree link")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        assert "resolved through the mount" in f.read()


@pytest.mark.smoke
@pytest.mark.parametrize("inside_change", [False, True], ids=["outside", "both"])
def test_link_merge_all_commits_changes_outside_a_subtree_link(
    new_lore_repo, inside_change
):
    """A merge of a link mounting a subtree commits what it merged outside the subtree into
    the linked repository, beside what it merged through the mount."""
    parent, linked = subtree_link_with_outside_change(
        new_lore_repo, inside_change, conflicting=False
    )

    parent.branch_merge_start("feature", message="Merge feature")
    parent.push()

    assert_linked_main_merged_settings(parent, linked)
    with parent.open_file(f"{SUBTREE_LINK}/a.txt") as f:
        assert f.read() == ("a on feature\n" if inside_change else "a base\n")


@pytest.mark.smoke
def test_link_commit_link_commits_changes_outside_a_subtree_link(new_lore_repo):
    """A commit of the link alone, during a merge, commits what the merge staged outside the
    subtree the link mounts."""
    parent, linked = subtree_link_with_outside_change(
        new_lore_repo, inside_change=False, conflicting=False
    )

    parent.branch_merge_start("feature", message="Merge feature", no_commit=True)
    parent.commit("Merge the linked feature branch", link=SUBTREE_LINK)
    parent.commit("Merge feature")
    parent.push()

    assert_linked_main_merged_settings(parent, linked)


@pytest.mark.smoke
def test_link_merge_all_refuses_a_conflict_outside_a_subtree_link(new_lore_repo):
    """A conflict a merge leaves outside the subtree a link mounts refuses the commit, and an
    abort restores the pin and the mount, leaving both repositories' branches where they were."""
    parent, linked = subtree_link_with_outside_change(
        new_lore_repo, inside_change=False, conflicting=True
    )
    pin_before = link_pin(parent, linked.get_id())
    latest_before = parent.branch_info().local_latest
    linked_latest_before = linked.branch_info("main").remote_latest

    parent.branch_merge_start("feature", message="Merge feature")
    with pytest.raises(UnresolvedConflictError):
        parent.commit("Merge feature")

    parent.branch_merge_abort()
    assert link_pin(parent, linked.get_id()) == pin_before
    assert parent.branch_info().local_latest == latest_before
    assert linked.branch_info("main").remote_latest == linked_latest_before
    assert parse_status_json(parent.status(json=True)) == []
    with parent.open_file(f"{SUBTREE_LINK}/a.txt") as f:
        assert f.read() == "a base\n"
    with parent.open_file("root.txt") as f:
        assert f.read() == "root\n"


@pytest.mark.smoke
@pytest.mark.parametrize(
    "link, outer",
    [(None, ""), (SUBTREE_LINK, ""), (None, "outer")],
    ids=["all", "link", "nested"],
)
def test_link_merge_all_refuses_deleting_the_subtree_a_link_mounts(
    new_lore_repo, link, outer
):
    """A merge, of every link or of the link alone, whose linked branch deletes the directory
    holding the subtree a link mounts is refused before it stages anything, leaving the working
    tree and the pin as they were. `outer` nests the link in a link mounted there."""
    linked_files = {
        "content/assets/a.txt": "a base\n",
        "config/settings.txt": "settings base\n",
    }
    if outer:
        parent, _ = make_parent_with_link(new_lore_repo, outer, {"root.txt": "root\n"})
        linked = make_repo(new_lore_repo, linked_files)
        p = f"{outer}/"
        parent.link_add(f"{p}{SUBTREE_LINK}", linked.get_id(), "content/assets")
        parent.commit("Add a nested link")
        parent.push()
    else:
        parent, linked = make_parent_with_link(
            new_lore_repo,
            SUBTREE_LINK,
            linked_files,
            {"root.txt": "root\n"},
            source_path="content/assets",
        )
        p = ""
    parent.branch_create("feature")
    parent.write_commit_push("Feature change", {f"{p}root.txt": "root on feature\n"})
    linked.branch_switch("feature")
    linked.rmtree("content")
    linked.stage(scan=True)
    linked.commit("Delete content")
    linked.push()
    parent.branch_switch("main")
    pin_before = link_pin(parent, linked.get_id())

    with pytest.raises(NotSupportedError):
        parent.branch_merge_start("feature", message="Merge feature", link=link)

    assert parse_status_json(parent.status(json=True)) == []
    assert link_pin(parent, linked.get_id()) == pin_before
    with parent.open_file(f"{p}{SUBTREE_LINK}/a.txt") as f:
        assert f.read() == "a base\n"
    with parent.open_file(f"{p}root.txt") as f:
        assert f.read() == "root\n"


@pytest.mark.smoke
def test_link_merge_all(new_lore_repo):
    """Default merge (no flags) across main + linked repos.

    Both branches add non-conflicting files to the linked repo independently.
    Without proper multi-repo merge orchestration, the linked repo would only
    get one branch's files (whichever pin the main repo merge selects).
    With orchestration, the linked repo is independently merged and both
    branches' files appear.
    """
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link repo base content\n",
        },
        {
            "main-file.txt": "main repo base content\n",
        },
    )

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, add a file in the linked repo
    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature branch main repo addition\n"])

    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    # Switch back to main and add a DIFFERENT file in the linked repo
    urc.branch_switch("main")

    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main branch only addition\n"])

    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Main branch additions")
    urc.push()

    # Default merge (no --link flag) — should merge all repos
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch across all repos",
    )
    urc.push()

    # Verify: files from BOTH branches are present in the linked repo.
    # Without multi-repo merge orchestration, the linked repo's files
    # from the feature branch might appear locally (through the main repo's
    # tree merge) but the link pin would not point to a properly merged
    # linked repo revision.
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature branch link repo file should be present after default merge"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main branch link repo file should still be present after default merge"
    )

    # Verify: main repo files from both branches are present
    assert urc.file_exists("feature-main-file.txt"), (
        "Feature branch main repo file should be present after default merge"
    )
    assert urc.file_exists("main-only-file.txt"), (
        "Main branch main repo file should still be present"
    )

    # Verify the link pin was properly merged by syncing to the new revision.
    # A sync re-realizes from the committed state (using link pins), so if the
    # link pin doesn't point to a merged revision, the feature file would vanish.
    urc.sync()
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "After sync, feature branch link repo file should still be present (pin is correct)"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "After sync, main branch link repo file should still be present (pin is correct)"
    )

    # Verify post-merge state is clean
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after merge commit - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_abort_all(new_lore_repo):
    """Abort-all rolls back all link pins and main repo changes."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link repo base content\n",
        },
        {
            "main-file.txt": "main repo base content\n",
        },
    )

    urc.branch_create("feature-branch")

    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature branch main repo addition\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main branch only addition\n"])
    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main branch link repo addition\n"])
    urc.stage(scan=True)
    urc.commit("Main branch additions")
    urc.push()

    # Start merge with --no-commit so we can abort
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch",
        no_commit=True,
    )

    # Verify merge brought in feature branch files
    assert urc.file_exists("feature-main-file.txt")
    assert urc.file_exists(f"{link_path}/feature-link-file.txt")

    # Abort the merge (no --link flag — abort all)
    urc.branch_merge_abort()

    # Verify: feature branch files are gone from BOTH repos
    assert not urc.file_exists("feature-main-file.txt"), (
        "Feature main file should be gone after abort"
    )
    assert not urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should be gone after abort"
    )

    # Verify: main branch files are still present
    assert urc.file_exists("main-only-file.txt"), (
        "Main only file should still be present after abort"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main link file should still be present after abort"
    )


@pytest.mark.smoke
def test_link_merge_abort_ignore_links(new_lore_repo):
    """Abort --ignore-links strips merge metadata but preserves link pin updates, staged on top
    of the current revision without its metadata."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link repo base content\n",
        },
        {
            "shared-file.txt": "base content\n",
        },
    )

    urc.branch_create("feature-branch")

    # Feature branch: modify the shared file (will conflict) and add link file
    with urc.open_file("shared-file.txt", "w+") as f:
        f.writelines(["feature branch content\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")

    # Main branch: modify the same shared file (creates conflict)
    with urc.open_file("shared-file.txt", "w+") as f:
        f.writelines(["main branch content\n"])
    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main branch link addition\n"])
    urc.stage(scan=True)
    urc.revision_metadata_set(["reviewed-by", "first.reviewer@example.com"])
    urc.commit("Main branch changes")
    urc.push()
    main_latest = urc.revision_info()

    # Start merge — should have conflicts in main but link merges cleanly
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch",
        no_commit=True,
    )

    # Abort only main merge, keeping link pin updates
    urc.branch_merge_abort(ignore_links=True)

    # Verify: link files from both branches are still present
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should be preserved after --ignore-links abort"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main link file should be preserved after --ignore-links abort"
    )

    # Verify: the staged state is committable (no merge flags)
    urc.commit("Commit link-only changes after selective abort")
    committed = urc.revision_info()
    assert committed.parent == main_latest.signature
    assert committed.merge == ""
    assert "reviewed-by" not in revision_metadata_keys(urc)
    urc.push()

    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after commit - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_resume(new_lore_repo):
    """Resume detection: a merge with --no-commit leaves staged state with
    LinkMergeState entries. Re-running merge (after commit) shows that the
    link merge infrastructure correctly tracks merged links."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link repo base content\n",
        },
        {
            "main-file.txt": "main repo base content\n",
        },
    )

    urc.branch_create("feature-branch")

    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature main addition\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature link addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main only addition\n"])
    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main link addition\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Start merge with --no-commit to keep staged state
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch",
        no_commit=True,
    )

    # Verify the merge brought in both branches' files
    assert urc.file_exists("feature-main-file.txt")
    assert urc.file_exists(f"{link_path}/feature-link-file.txt")

    # Commit the merge manually (like the user would after reviewing)
    urc.commit("Manual merge commit")
    urc.push()

    # Verify everything persists through commit + sync
    urc.sync()
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should persist after manual commit and sync"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main link file should persist after manual commit and sync"
    )

    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_dry_run(new_lore_repo):
    """Dry-run previews changes across all repos without modifying state."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link repo base content\n",
        },
        {
            "main-file.txt": "main repo base content\n",
        },
    )

    urc.branch_create("feature-branch")

    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature branch main repo addition\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")

    # Run merge with --dry-run
    urc.branch_merge_start(
        "feature-branch",
        message="Dry run merge",
        dry_run=True,
    )

    # Verify: no files from feature branch appeared
    assert not urc.file_exists("feature-main-file.txt"), (
        "Feature main file should NOT exist after dry-run"
    )
    assert not urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should NOT exist after dry-run"
    )

    # Verify: no staged state remains
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after dry-run - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_all_file_conflict_resolve_in_place(new_lore_repo):
    """Default merge stages link file conflicts in place; resolve + commit finishes
    the merge without a `--link` re-entry."""

    link_path = "linked/repo"
    urc, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "shared-link-file.txt": "base content\n",
        },
        {
            "main-file.txt": "main repo base content\n",
        },
    )

    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/shared-link-file.txt", "w+") as f:
        f.writelines(["feature branch content\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/shared-link-file.txt", "w+") as f:
        f.writelines(["main branch content\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Capture the pre-merge link pin (main's committed link revision)
    # so we can verify it advances after the merge resolves.
    pin_before = link_pin(urc, link_repo.get_id())

    # Default merge no longer fails — it stages the conflict in place.
    # The merge command must succeed (return without exception) even though
    # the linked file has conflict markers; main's merge is skipped, the link
    # pin is updated to the merged-but-conflicted revision, and the parent
    # state carries StateFlags::Conflict so `merge resolve` knows what to do.
    urc.branch_merge_start("feature-branch", message="Merge with link conflict")

    # The conflicting file exists at the mount path with conflict markers
    conflict_file = f"{link_path}/shared-link-file.txt"
    assert urc.file_exists(conflict_file), (
        "Conflicted link file should exist at mount path"
    )
    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "<<<<<<<" in content or ">>>>>>>" in content, (
        f"Conflict markers should be present in {conflict_file} - Got:\n{content}"
    )

    # Resolve the conflict in place — no `--link` re-entry needed
    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["resolved content\n"])
    urc.branch_merge_resolve(conflict_file)

    # Commit finishes the merge: link committed first (commit_link_node),
    # then committing in the parent incorporates the new link pin.
    urc.commit("Merge feature-branch with resolved link conflict")
    urc.push()

    # Working tree clean
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after commit - Got:\n{status}"
    )

    # Resolved content survived the commit
    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "resolved content" in content, (
        f"File should have resolved content - Got: {content}"
    )

    # Link pin advanced to a new revision
    pin_after = link_pin(urc, link_repo.get_id())
    assert pin_before != pin_after, (
        f"Link pin should have advanced after merge.\n"
        f"Before: {pin_before}\nAfter: {pin_after}"
    )


@pytest.mark.smoke
def test_link_merge_all_file_conflict_abort(new_lore_repo):
    """Default merge with a link file conflict can be aborted, restoring everything."""

    link_path = "linked/repo"
    urc, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "shared-link-file.txt": "base content\n",
        },
        {
            "main-file.txt": "main repo base content\n",
        },
    )

    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/shared-link-file.txt", "w+") as f:
        f.writelines(["feature branch content\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/shared-link-file.txt", "w+") as f:
        f.writelines(["main branch content\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Capture pin AFTER main's commit (this is the value abort should restore to)
    pin_before = link_pin(urc, link_repo.get_id())

    # Default merge stages the conflict in place
    urc.branch_merge_start("feature-branch", message="Merge with link conflict")

    # Abort with no flags — should roll back the conflicted link's pin and
    # restore on-disk content from main's pre-merge state.
    urc.branch_merge_abort()

    # Working tree matches pre-merge state: main's content is back, no markers
    conflict_file = f"{link_path}/shared-link-file.txt"
    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "main branch content" in content, (
        f"After abort, file should have main's pre-merge content - Got: {content}"
    )
    assert "<<<<<<<" not in content, (
        f"After abort, conflict markers should be gone - Got: {content}"
    )

    # Link pin restored
    pin_after = link_pin(urc, link_repo.get_id())
    assert pin_before == pin_after, (
        f"Link pin should match pre-merge value after abort.\n"
        f"Before: {pin_before}\nAfter: {pin_after}"
    )

    # No staged state left behind
    status = urc.status()
    assert "merge" not in status.lower() or "in progress" not in status.lower(), (
        f"No merge should be in progress after abort - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_all_mixed_clean_and_conflict(new_lore_repo):
    """One link merges cleanly, another has a file conflict.
    The clean link's merge survives while the user resolves the conflict in place."""

    urc = make_repo(
        new_lore_repo,
        {
            "main-file.txt": "main base\n",
        },
    )

    # Link A — will merge cleanly (changes on different files per branch)
    link_a = make_repo(
        new_lore_repo,
        {
            "a-base.txt": "a base\n",
        },
    )

    # Link B — will conflict (both branches modify the same file)
    link_b = make_repo(
        new_lore_repo,
        {
            "b-shared.txt": "b base\n",
        },
    )

    path_a = "libs/a"
    path_b = "libs/b"
    urc.link_add(path_a, link_a.get_id(), "/")
    urc.link_add(path_b, link_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    # Feature branch: A gets a new file, B's shared file is changed
    urc.branch_create("feature-branch")
    with urc.open_file(f"{path_a}/feature-only.txt", "w+") as f:
        f.writelines(["feature only\n"])
    with urc.open_file(f"{path_b}/b-shared.txt", "w+") as f:
        f.writelines(["feature B content\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    # Main: A gets a different new file (no conflict with feature),
    # B's shared file is changed differently (conflict with feature)
    urc.branch_switch("main")
    with urc.open_file(f"{path_a}/main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    with urc.open_file(f"{path_b}/b-shared.txt", "w+") as f:
        f.writelines(["main B content\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Capture pre-merge pins after main's commit
    pin_a_before = link_pin(urc, link_a.get_id())
    pin_b_before = link_pin(urc, link_b.get_id())

    # Default merge: link A merges cleanly, link B conflicts.
    # Both link pins should be staged (A advanced to merged revision,
    # B advanced to merged-but-conflicted revision). Main is skipped.
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")

    # Both links' file additions visible (A's merged, B's conflicted file present)
    assert urc.file_exists(f"{path_a}/feature-only.txt")
    assert urc.file_exists(f"{path_a}/main-only.txt")
    assert urc.file_exists(f"{path_b}/b-shared.txt")

    # Resolve the B conflict
    with urc.open_file(f"{path_b}/b-shared.txt", "w+") as f:
        f.writelines(["resolved B content\n"])
    urc.branch_merge_resolve(f"{path_b}/b-shared.txt")

    urc.commit("Resolve B; merge done")
    urc.push()

    # Both link pins advanced from pre-merge state
    pin_a_after = link_pin(urc, link_a.get_id())
    pin_b_after = link_pin(urc, link_b.get_id())
    assert pin_a_before != pin_a_after, (
        "Link A pin should have advanced (clean merge preserved)"
    )
    assert pin_b_before != pin_b_after, (
        "Link B pin should have advanced (conflict resolved)"
    )

    # Final content reflects expected resolution
    with urc.open_file(f"{path_b}/b-shared.txt", "r") as f:
        b_content = f.read()
    assert "resolved B content" in b_content

    # Working tree clean
    status = urc.status()
    assert "local branch in sync with remote" in status.lower()


@pytest.mark.smoke
def test_link_merge_all_abort_specific_link(new_lore_repo):
    """Abort --link during a multi-repo merge rolls back only that link."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link repo base content\n",
        },
        {
            "main-file.txt": "main repo base content\n",
        },
    )

    urc.branch_create("feature-branch")

    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature main addition\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature link addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main only addition\n"])
    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main link addition\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Start merge with --no-commit
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch",
        no_commit=True,
    )

    # Verify merge brought in feature files
    assert urc.file_exists("feature-main-file.txt")
    assert urc.file_exists(f"{link_path}/feature-link-file.txt")

    # Abort only the link merge
    urc.branch_merge_abort(link=link_path)

    # Verify: link feature file is gone (link reverted)
    assert not urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should be gone after link-specific abort"
    )

    # Verify: main feature file is still present (main merge preserved)
    assert urc.file_exists("feature-main-file.txt"), (
        "Feature main file should still be present (main merge not aborted)"
    )

    # Verify: main branch link file is still present
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main link file should still be present"
    )


@pytest.mark.smoke
def test_link_merge_all_multiple_links(new_lore_repo):
    """Default merge works across main + multiple linked repos."""
    urc = make_repo(
        new_lore_repo,
        {
            "main-file.txt": "main base\n",
        },
    )

    link_a = make_repo(
        new_lore_repo,
        {
            "a-file.txt": "a base\n",
        },
    )

    link_b = make_repo(
        new_lore_repo,
        {
            "b-file.txt": "b base\n",
        },
    )

    path_a = "libs/a"
    path_b = "libs/b"
    urc.link_add(path_a, link_a.get_id(), "/")
    urc.link_add(path_b, link_b.get_id(), "/")
    urc.commit("Add both links")
    urc.push()

    # Feature branch: add file in main, link A, and link B
    urc.branch_create("feature-branch")
    with urc.open_file("feature-main.txt", "w+") as f:
        f.writelines(["feature main\n"])
    with urc.open_file(f"{path_a}/feature-a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    with urc.open_file(f"{path_b}/feature-b.txt", "w+") as f:
        f.writelines(["feature b\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions across all repos")
    urc.push()

    # Main branch: add different files in all three repos
    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    with urc.open_file(f"{path_a}/main-a.txt", "w+") as f:
        f.writelines(["main a\n"])
    with urc.open_file(f"{path_b}/main-b.txt", "w+") as f:
        f.writelines(["main b\n"])
    urc.stage(scan=True)
    urc.commit("Main branch additions across all repos")
    urc.push()

    # Default merge across all three repos
    urc.branch_merge_start("feature-branch", message="Merge all")
    urc.push()

    # All files from both branches, in all three repos, should be present
    for f in [
        "feature-main.txt",
        "main-only.txt",
        f"{path_a}/feature-a.txt",
        f"{path_a}/main-a.txt",
        f"{path_b}/feature-b.txt",
        f"{path_b}/main-b.txt",
    ]:
        assert urc.file_exists(f), f"{f} should be present after multi-link merge"

    # Sync to verify pins point at properly merged linked repo revisions
    urc.sync()
    assert urc.file_exists(f"{path_a}/feature-a.txt"), (
        "Link A feature file should persist after sync (pin is correct)"
    )
    assert urc.file_exists(f"{path_b}/feature-b.txt"), (
        "Link B feature file should persist after sync (pin is correct)"
    )


@pytest.mark.smoke
def test_link_merge_all_mixed_eligibility(new_lore_repo):
    """Default merge skips DisableAutoFollow links but still merges eligible ones."""
    urc = make_repo(
        new_lore_repo,
        {
            "main-file.txt": "main base\n",
        },
    )

    link_follow = make_repo(
        new_lore_repo,
        {
            "follow.txt": "follow base\n",
        },
    )

    link_fixed = make_repo(
        new_lore_repo,
        {
            "fixed.txt": "fixed base\n",
        },
    )

    path_follow = "libs/follow"
    path_fixed = "libs/fixed"
    urc.link_add(path_follow, link_follow.get_id(), "/")
    # disable_branching → no auto-follow, pin stays fixed even across branches
    urc.link_add(path_fixed, link_fixed.get_id(), "/", disable_branching=True)
    urc.commit("Add links (one follow, one fixed)")
    urc.push()

    # Record the fixed link's pin revision — should be unchanged after merge
    fixed_before = link_pin(urc, link_fixed.get_id())

    # Feature branch: add file in auto-follow link only
    urc.branch_create("feature-branch")
    with urc.open_file(f"{path_follow}/feature-follow.txt", "w+") as f:
        f.writelines(["feature follow\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch addition to follow link")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    urc.stage(scan=True)
    urc.commit("Main branch addition")
    urc.push()

    # Default merge — follow link should merge, fixed link should be skipped
    urc.branch_merge_start("feature-branch", message="Mixed eligibility merge")
    urc.push()

    # Follow link's feature file should be present
    assert urc.file_exists(f"{path_follow}/feature-follow.txt"), (
        "Auto-follow link should have been merged"
    )
    # Main repo's feature file should also be present
    assert urc.file_exists("main-only.txt"), "Main file should still be present"

    # Fixed link should still have only its base content
    assert urc.file_exists(f"{path_fixed}/fixed.txt"), (
        "Fixed link base file should persist"
    )

    # The fixed link's pin revision should be unchanged
    fixed_after = link_pin(urc, link_fixed.get_id())
    assert fixed_before == fixed_after, (
        f"Fixed (DisableAutoFollow) link pin should not change.\n"
        f"Before: {fixed_before}\nAfter: {fixed_after}"
    )


@pytest.mark.smoke
def test_link_merge_all_preserves_tracked_branches(new_lore_repo):
    """After default merge, each link's tracked branch stays on main (not feature-branch)."""
    urc = make_repo(
        new_lore_repo,
        {
            "main-file.txt": "main base\n",
        },
    )

    link_a = make_repo(
        new_lore_repo,
        {
            "a.txt": "a base\n",
        },
    )

    link_b = make_repo(
        new_lore_repo,
        {
            "b.txt": "b base\n",
        },
    )

    urc.link_add("libs/a", link_a.get_id(), "/")
    urc.link_add("libs/b", link_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    urc.branch_create("feature-branch")
    with urc.open_file("libs/a/feature-a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    with urc.open_file("libs/b/feature-b.txt", "w+") as f:
        f.writelines(["feature b\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")
    urc.branch_merge_start("feature-branch", message="Default merge")
    urc.push()

    # After merge on main, both links should still track main (not feature-branch)
    link_list_after = urc.link_list()
    assert "feature-branch" not in link_list_after, (
        f"No link should track 'feature-branch' after merge.\nGot: {link_list_after}"
    )
    # "main" should appear (as tracked branch of each link)
    assert "main" in link_list_after, (
        f"Links should track 'main'.\nGot: {link_list_after}"
    )


@pytest.mark.smoke
def test_link_merge_abort_all_restores_link_pins(new_lore_repo):
    """After branch merge abort, each link's pin/branch matches the pre-merge snapshot."""
    urc = make_repo(
        new_lore_repo,
        {
            "main-file.txt": "main base\n",
        },
    )

    link_a = make_repo(
        new_lore_repo,
        {
            "a.txt": "a base\n",
        },
    )

    link_b = make_repo(
        new_lore_repo,
        {
            "b.txt": "b base\n",
        },
    )

    urc.link_add("libs/a", link_a.get_id(), "/")
    urc.link_add("libs/b", link_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    # Snapshot before any feature branch activity
    pins_before = {
        repo.get_id(): link_pin(urc, repo.get_id()) for repo in (link_a, link_b)
    }

    urc.branch_create("feature-branch")
    with urc.open_file("libs/a/feature-a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    with urc.open_file("libs/b/feature-b.txt", "w+") as f:
        f.writelines(["feature b\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")
    urc.branch_merge_start("feature-branch", message="Default merge", no_commit=True)

    # Verify feature files are present during the pending merge
    assert urc.file_exists("libs/a/feature-a.txt")
    assert urc.file_exists("libs/b/feature-b.txt")

    urc.branch_merge_abort()

    # Files should be rolled back for both links
    assert not urc.file_exists("libs/a/feature-a.txt"), (
        "Link A feature file should be gone after abort"
    )
    assert not urc.file_exists("libs/b/feature-b.txt"), (
        "Link B feature file should be gone after abort"
    )

    # Both pins should match the pre-merge snapshot
    for repo_id, pin_before in pins_before.items():
        pin_after = link_pin(urc, repo_id)
        assert pin_before == pin_after, (
            f"Link {repo_id} pin should be restored after abort.\n"
            f"Before: {pin_before}\nAfter: {pin_after}"
        )

    # No merge should still be in progress
    status = urc.status()
    assert "pending merge" not in status.lower(), (
        f"No merge should be in progress after abort - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_all_no_link_changes(new_lore_repo):
    """Default merge succeeds when only the main repo has diverged and linked repo is untouched."""
    link_path = "linked/repo"
    urc, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link content\n",
        },
        {
            "main-file.txt": "main base\n",
        },
    )

    # Capture link pin revision before any divergence — should stay unchanged
    before = link_pin(urc, link_repo.get_id())

    # Feature branch: modify main repo only, don't touch the link
    urc.branch_create("feature-branch")
    with urc.open_file("feature-main.txt", "w+") as f:
        f.writelines(["feature main\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch main-only addition")
    urc.push()

    # Main branch: modify main repo only too, don't touch the link
    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    urc.stage(scan=True)
    urc.commit("Main branch main-only addition")
    urc.push()

    # Default merge — linked repo has no divergence, should succeed cleanly
    urc.branch_merge_start("feature-branch", message="No-link-divergence merge")
    urc.push()

    # Main files from both branches present
    assert urc.file_exists("feature-main.txt")
    assert urc.file_exists("main-only.txt")

    # Link pin should not have changed (no divergence → no new linked revision)
    after = link_pin(urc, link_repo.get_id())
    assert before == after, (
        f"Link pin should be unchanged when linked repo hasn't diverged.\n"
        f"Before: {before}\nAfter: {after}"
    )


@pytest.mark.smoke
def test_link_merge_all_sequential(new_lore_repo):
    """Two sequential default merges from the same feature branch succeed."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link base\n",
        },
        {
            "main-file.txt": "main base\n",
        },
    )

    # Round 1: feature branch → main, both via default merge
    urc.branch_create("feature-branch")
    with urc.open_file("round1-main.txt", "w+") as f:
        f.writelines(["round1 main\n"])
    with urc.open_file(f"{link_path}/round1-link.txt", "w+") as f:
        f.writelines(["round1 link\n"])
    urc.stage(scan=True)
    urc.commit("Round 1 additions")
    urc.push()

    urc.branch_switch("main")
    urc.branch_merge_start("feature-branch", message="Round 1 merge")
    urc.push()

    assert urc.file_exists("round1-main.txt")
    assert urc.file_exists(f"{link_path}/round1-link.txt")

    # Round 2: sync feature from main, add more, merge again
    urc.branch_switch("feature-branch")
    urc.sync()
    urc.branch_merge_start("main", message="Pull main into feature")
    urc.push()

    with urc.open_file("round2-main.txt", "w+") as f:
        f.writelines(["round2 main\n"])
    with urc.open_file(f"{link_path}/round2-link.txt", "w+") as f:
        f.writelines(["round2 link\n"])
    urc.stage(scan=True)
    urc.commit("Round 2 additions")
    urc.push()

    urc.branch_switch("main")
    urc.branch_merge_start("feature-branch", message="Round 2 merge")
    urc.push()

    # All files from both rounds should be present
    for f in [
        "round1-main.txt",
        "round2-main.txt",
        f"{link_path}/round1-link.txt",
        f"{link_path}/round2-link.txt",
    ]:
        assert urc.file_exists(f), f"{f} should be present after sequential merges"

    # Verify the pin is correct (sync realizes from link pin)
    urc.sync()
    assert urc.file_exists(f"{link_path}/round1-link.txt")
    assert urc.file_exists(f"{link_path}/round2-link.txt")


@pytest.mark.smoke
def test_link_merge_all_push_and_clone(new_lore_repo):
    """After default merge and push, a fresh clone correctly follows link pins."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link base\n",
        },
        {
            "main-file.txt": "main base\n",
        },
    )

    # Divergent content in the linked repo across branches
    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/feature-only.txt", "w+") as f:
        f.writelines(["feature only\n"])
    urc.stage(scan=True)
    urc.commit("Feature addition in linked repo")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    urc.stage(scan=True)
    urc.commit("Main addition in linked repo")
    urc.push()

    urc.branch_merge_start("feature-branch", message="Default merge")
    urc.push()

    # A fresh clone of the parent should see both branches' files in the linked repo
    clone = urc.clone()
    assert clone.file_exists(f"{link_path}/feature-only.txt"), (
        "Clone should have feature branch linked file (pin points at merged revision)"
    )
    assert clone.file_exists(f"{link_path}/main-only.txt"), (
        "Clone should have main branch linked file (pin points at merged revision)"
    )


@pytest.mark.smoke
def test_link_merge_abort_ignore_links_no_conflicts(new_lore_repo):
    """abort --ignore-links on a clean merge keeps link pin updates as staged changes."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link base\n",
        },
        {
            "main-file.txt": "main base\n",
        },
    )

    # No conflicts: both branches add independent files in the linked repo
    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/feature-link.txt", "w+") as f:
        f.writelines(["feature link\n"])
    with urc.open_file("feature-main.txt", "w+") as f:
        f.writelines(["feature main\n"])
    urc.stage(scan=True)
    urc.commit("Feature additions")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/main-link.txt", "w+") as f:
        f.writelines(["main link\n"])
    urc.stage(scan=True)
    urc.commit("Main addition")
    urc.push()

    # Start merge with --no-commit; no conflicts in main or link
    urc.branch_merge_start(
        "feature-branch", message="Clean default merge", no_commit=True
    )

    # Before selective abort: feature files are present
    assert urc.file_exists(f"{link_path}/feature-link.txt")
    assert urc.file_exists("feature-main.txt")

    # Selective main abort — keeps link pin update, drops main merge
    urc.branch_merge_abort(ignore_links=True)

    # Link merge result should remain
    assert urc.file_exists(f"{link_path}/feature-link.txt"), (
        "Feature link file should be preserved after --ignore-links abort"
    )
    assert urc.file_exists(f"{link_path}/main-link.txt"), (
        "Main link file should still be present after --ignore-links abort"
    )

    # Main merge should be reverted
    assert not urc.file_exists("feature-main.txt"), (
        "Feature main file should be gone after --ignore-links abort"
    )

    # The remaining staged state should be committable without merge flags
    urc.commit("Commit only the link pin updates")
    urc.push()

    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after commit - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_abort_ignore_links_with_link_conflicts(new_lore_repo):
    """A link merge that produced file conflicts, then aborted with --ignore-links,
    must not leave .mine/.theirs/.base sidecars or marker bytes orphaned in the
    link mount. The parent has no merge metadata after the abort+re-pin, so any
    leftover artifacts would be unattributable."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "shared.txt": "link base\n",
        },
        {
            "main-file.txt": "main base\n",
        },
    )

    # Feature branch: modify the shared file via mount
    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/shared.txt", "w+") as f:
        f.writelines(["feature content\n"])
    urc.stage(scan=True)
    urc.commit("Feature modifies shared.txt")
    urc.push()

    # Main branch: modify the same file differently via mount
    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/shared.txt", "w+") as f:
        f.writelines(["main content\n"])
    urc.stage(scan=True)
    urc.commit("Main modifies shared.txt")
    urc.push()

    # Default merge — link conflicts, parent state set as merge in conflict
    urc.branch_merge_start(
        "feature-branch", message="Conflicting merge", no_commit=True
    )

    file_path = f"{link_path}/shared.txt"
    mine_sidecar = f"{file_path}.mine"
    theirs_sidecar = f"{file_path}.theirs"
    base_sidecar = f"{file_path}.base"

    # Sanity: after the merge, the file should have either inline markers (for
    # text conflicts the text merger handled) OR sidecars (for unmergeable
    # binary conflicts). Either form is an artifact that abort must clean up.
    with urc.open_file(file_path, "r") as f:
        mid_merge_content = f.read()
    has_inline_markers = ("<<<<<<<" in mid_merge_content) or (
        ">>>>>>>" in mid_merge_content
    )
    has_sidecars = (
        urc.file_exists(mine_sidecar)
        or urc.file_exists(theirs_sidecar)
        or urc.file_exists(base_sidecar)
    )
    assert has_inline_markers or has_sidecars, (
        f"Expected mid-merge artifacts (inline markers or sidecars). Content was:\n{mid_merge_content}"
    )

    # Abort with --ignore-links: parent merge metadata stripped, link pins kept.
    # The on-disk markers/sidecars must not be left orphaned.
    urc.branch_merge_abort(ignore_links=True)

    assert not urc.file_exists(mine_sidecar), (
        f"Mine sidecar {mine_sidecar} should be cleaned after abort --ignore-links"
    )
    assert not urc.file_exists(theirs_sidecar), (
        f"Theirs sidecar {theirs_sidecar} should be cleaned after abort --ignore-links"
    )
    assert not urc.file_exists(base_sidecar), (
        f"Base sidecar {base_sidecar} should be cleaned after abort --ignore-links"
    )

    # The file's inline markers should also be gone — abort --ignore-links keeps
    # the link pin, but it should not leave merge-conflict markers in committed
    # link content. The link's pin should resolve to a clean state, not a
    # merged-but-conflicted one with markers.
    if urc.file_exists(file_path):
        with urc.open_file(file_path, "r") as f:
            post_abort_content = f.read()
        assert (
            "<<<<<<<" not in post_abort_content and ">>>>>>>" not in post_abort_content
        ), (
            f"Inline conflict markers should be cleaned. Content was:\n{post_abort_content}"
        )


@pytest.mark.smoke
def test_link_merge_start_ignore_links(new_lore_repo):
    """`merge start --ignore-links` merges only the parent repo. The link pin
    is unchanged and the source-side link files do not appear at the mount.
    """

    link_path = "linked/repo"
    urc, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link base\n",
        },
        {
            "main-file.txt": "main base\n",
        },
    )

    urc.branch_create("feature-branch")
    with urc.open_file("feature-only.txt", "w+") as f:
        f.writelines(["feature only main\n"])
    with urc.open_file(f"{link_path}/feature-link.txt", "w+") as f:
        f.writelines(["feature only in link\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    urc.stage(scan=True)
    urc.commit("Main branch additions")
    urc.push()

    pin_before = link_pin(urc, link_repo.get_id())

    # Merge with --ignore-links — should merge only the parent
    urc.branch_merge_start(
        "feature-branch", message="Merge main only", ignore_links=True
    )

    # Parent's feature file is now on main, and main's own file remains
    assert urc.file_exists("feature-only.txt")
    assert urc.file_exists("main-only.txt")

    # Link pin should be unchanged — link wasn't touched
    pin_after = link_pin(urc, link_repo.get_id())
    assert pin_before == pin_after, (
        f"Link pin should be unchanged with --ignore-links.\n"
        f"Before: {pin_before}\nAfter: {pin_after}"
    )

    # The link's feature-side content should NOT have been realized at the
    # mount path (link merge skipped entirely).
    assert not urc.file_exists(f"{link_path}/feature-link.txt"), (
        "Feature-side link file should not appear at mount with --ignore-links"
    )

    urc.push()
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after --ignore-links merge - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_start_ignore_links_link_conflict(new_lore_repo):
    """`merge start --ignore-links` succeeds even when the link would have a
    file conflict, because the link is not consulted at all.
    """

    link_path = "linked/repo"
    urc, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "shared.txt": "base\n",
        },
        {
            "main-file.txt": "main base\n",
        },
    )

    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/shared.txt", "w+") as f:
        f.writelines(["feature\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch link change")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/shared.txt", "w+") as f:
        f.writelines(["main\n"])
    urc.stage(scan=True)
    urc.commit("Main branch link change")
    urc.push()

    pin_before = link_pin(urc, link_repo.get_id())

    # Merge with --ignore-links — the link's would-be conflict is ignored
    # since the link is skipped entirely. The merge succeeds with no
    # conflicts to resolve.
    urc.branch_merge_start(
        "feature-branch",
        message="Merge main only despite link conflict",
        ignore_links=True,
    )

    # No conflict markers anywhere — link wasn't touched
    with urc.open_file(f"{link_path}/shared.txt", "r") as f:
        content = f.read()
    assert "<<<<<<<" not in content, (
        f"No conflict markers expected with --ignore-links - Got: {content}"
    )

    pin_after = link_pin(urc, link_repo.get_id())
    assert pin_before == pin_after

    urc.push()
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean - Got:\n{status}"
    )


@pytest.mark.smoke
def test_link_merge_into_ignore_links(new_lore_repo):
    """`merge into <branch> <message> --ignore-links` skips link content and
    merges only the parent repo, with the auto-commit semantics of `merge into`.
    """

    link_path = "linked/repo"
    urc, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link base\n",
        },
        {
            "main-file.txt": "main base\n",
        },
    )

    # Capture main's link pin BEFORE branching — this is what `--ignore-links`
    # `merge into main` should leave unchanged on the main branch.
    main_pin_before = link_pin(urc, link_repo.get_id())

    urc.branch_create("feature-branch")
    with urc.open_file("feature-only.txt", "w+") as f:
        f.writelines(["feature only\n"])
    with urc.open_file(f"{link_path}/feature-link.txt", "w+") as f:
        f.writelines(["feature link\n"])
    urc.stage(scan=True)
    urc.commit("Feature changes")
    urc.push()

    # `merge into main --ignore-links` from feature-branch — folds
    # feature-branch's parent changes into main without touching the link.
    urc.branch_merge_into(
        "main", "Merge feature into main, ignore links", ignore_links=True
    )

    # We're still on feature-branch (merge into pushes into the OTHER branch
    # without switching). Switch to main to verify what landed.
    urc.branch_switch("main")
    urc.sync()

    # Feature's parent file is on main
    assert urc.file_exists("feature-only.txt")

    # Link's source-side file is NOT realized at the mount path on main
    assert not urc.file_exists(f"{link_path}/feature-link.txt"), (
        "Feature-side link file should not appear with --ignore-links"
    )

    # Link pin on main is unchanged from its pre-branch state — `--ignore-links`
    # didn't merge any link revisions.
    main_pin_after = link_pin(urc, link_repo.get_id())
    assert main_pin_before == main_pin_after, (
        f"Main's link pin should be unchanged after --ignore-links merge_into.\n"
        f"Before: {main_pin_before}\nAfter: {main_pin_after}"
    )


@pytest.mark.smoke
def test_link_merge_start_ignore_links_link_mutex(new_lore_repo):
    """Clap rejects the `--ignore-links --link <path>` combination at the CLI
    layer."""
    link_path = "linked/repo"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link\n",
        },
        {
            "main-file.txt": "main\n",
        },
    )

    urc.branch_create("feature-branch")
    with urc.open_file("f.txt", "w+") as f:
        f.writelines(["f\n"])
    urc.stage(scan=True)
    urc.commit("Feature change")
    urc.push()

    urc.branch_switch("main")

    # Clap rejects the two flags together before the merge starts.
    with pytest.raises(ImproperArgumentsError):
        urc.branch_merge_start(
            "feature-branch",
            message="Should fail",
            link=link_path,
            ignore_links=True,
        )


@pytest.mark.smoke
def test_link_merge_abort_all_then_resume_multiple_links(new_lore_repo):
    """Default abort with multiple links cleanly rolls everything back, and a
    subsequent re-attempted merge succeeds — exercises the abort-then-resume
    composition with N>1 links."""
    urc = make_repo(
        new_lore_repo,
        {
            "main-file.txt": "main base\n",
        },
    )

    link_a = make_repo(
        new_lore_repo,
        {
            "a-file.txt": "a base\n",
        },
    )

    link_b = make_repo(
        new_lore_repo,
        {
            "b-file.txt": "b base\n",
        },
    )

    path_a = "libs/a"
    path_b = "libs/b"
    urc.link_add(path_a, link_a.get_id(), "/")
    urc.link_add(path_b, link_b.get_id(), "/")
    urc.commit("Add both links")
    urc.push()

    # Feature branch: add file in main, link A, and link B
    urc.branch_create("feature-branch")
    with urc.open_file("feature-main.txt", "w+") as f:
        f.writelines(["feature main\n"])
    with urc.open_file(f"{path_a}/feature-a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    with urc.open_file(f"{path_b}/feature-b.txt", "w+") as f:
        f.writelines(["feature b\n"])
    urc.stage(scan=True)
    urc.commit("Feature additions across all repos")
    urc.push()

    # Main branch: add different files in all three repos
    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    with urc.open_file(f"{path_a}/main-a.txt", "w+") as f:
        f.writelines(["main a\n"])
    with urc.open_file(f"{path_b}/main-b.txt", "w+") as f:
        f.writelines(["main b\n"])
    urc.stage(scan=True)
    urc.commit("Main additions across all repos")
    urc.push()

    # Start merge with --no-commit so we can abort
    urc.branch_merge_start(
        "feature-branch",
        message="First attempt",
        no_commit=True,
    )

    # Both links and main staged the feature files
    assert urc.file_exists("feature-main.txt")
    assert urc.file_exists(f"{path_a}/feature-a.txt")
    assert urc.file_exists(f"{path_b}/feature-b.txt")

    # Default abort — rolls back main and BOTH link pins
    urc.branch_merge_abort()

    # Feature files gone from main and both links
    assert not urc.file_exists("feature-main.txt"), (
        "Feature main file should be gone after abort"
    )
    assert not urc.file_exists(f"{path_a}/feature-a.txt"), (
        "Feature link A file should be gone after abort"
    )
    assert not urc.file_exists(f"{path_b}/feature-b.txt"), (
        "Feature link B file should be gone after abort"
    )

    # Main files still present in all three repos
    assert urc.file_exists("main-only.txt")
    assert urc.file_exists(f"{path_a}/main-a.txt")
    assert urc.file_exists(f"{path_b}/main-b.txt")

    # Re-attempt the merge — must succeed and produce the same end state
    urc.branch_merge_start("feature-branch", message="Second attempt")
    urc.push()

    for f in [
        "feature-main.txt",
        "main-only.txt",
        f"{path_a}/feature-a.txt",
        f"{path_a}/main-a.txt",
        f"{path_b}/feature-b.txt",
        f"{path_b}/main-b.txt",
    ]:
        assert urc.file_exists(f), f"{f} should be present after re-attempted merge"

    urc.sync()
    assert urc.file_exists(f"{path_a}/feature-a.txt"), (
        "Link A feature file should persist after sync (pin is correct)"
    )
    assert urc.file_exists(f"{path_b}/feature-b.txt"), (
        "Link B feature file should persist after sync (pin is correct)"
    )


@pytest.mark.smoke
@pytest.mark.parametrize(
    "link_scoped, message_paths",
    [(False, MERGE_LINKS), (True, ["./linked/repo/"])],
    ids=["all-links", "link-scoped-unnormalized-path"],
)
def test_link_merge_per_link_message(new_lore_repo, link_scoped, message_paths):
    """An auto-committed merge gives each linked repository its own message."""
    urc, link_repos = diverged_link_branches(new_lore_repo)
    link_messages = {path: f"Merge into {path}" for path in message_paths}

    urc.branch_merge_start(
        "feature-branch",
        message="Merge parent",
        link=MERGE_LINKS[0] if link_scoped else None,
        link_messages=link_messages,
    )
    urc.push()

    assert urc.revision_info(check=True, no_pager=True).message == "Merge parent"
    for link_repo, message in zip(link_repos, link_messages.values()):
        link_repo.sync()
        assert link_repo.revision_info(check=True, no_pager=True).message == message
        assert link_repo.file_exists("feature.txt") and link_repo.file_exists(
            "main.txt"
        )


@pytest.mark.smoke
@pytest.mark.parametrize("message_path", ["not/a/link", "main.txt"])
def test_link_merge_rejects_unknown_link_message_path(new_lore_repo, message_path):
    """A per-link message for a path that is not a link fails before merging."""
    urc, _link_repos = diverged_link_branches(new_lore_repo)
    revision_before = urc.revision_info(check=True, no_pager=True).revision

    with pytest.raises(ImproperArgumentsError):
        urc.branch_merge_start(
            "feature-branch",
            message="Merge parent",
            link_messages={message_path: "Merge link"},
        )

    assert urc.revision_info(check=True, no_pager=True).revision == revision_before


@pytest.mark.smoke
@pytest.mark.parametrize("pinned", [False, True], ids=["tracking", "pinned"])
def test_link_merge_all_of_a_branch_moving_a_link_to_another_path_with_incoming_changes(
    new_lore_repo, pinned
):
    """A default merge of a branch that removes a link and links its repository at another
    path merges the repository there, so the link holds main's changes to the repository as
    well."""
    urc, link_repo = merge_a_branch_removing_a_link(new_lore_repo, True, pinned)
    urc.push()

    assert registered_links(urc) == {"libs/new": link_repo.get_id()}
    exposed = ["feature.txt", "linked.txt", "main.txt", "shared.txt"]
    for repo in [urc, urc.clone()]:
        assert not repo.path_exists("libs/old"), "the merge should remove the old mount"
        assert mount_listing(repo, "libs/new") == exposed, (
            "the added link should expose the revision the merged branch pins"
        )


def _resolve_the_conflict_inside_the_removed_link(urc):
    """Resolves the conflict the merge of the link at `libs/old` stopped on, and starts the
    merge again to merge the parent, which conflicts at the mount the merged branch removed."""
    assert urc.unresolved_paths() == {"libs/old/shared.txt"}
    urc.write_files({"libs/old/shared.txt": "resolved\n"})
    urc.branch_merge_resolve("libs/old/shared.txt")
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")


@pytest.mark.smoke
@pytest.mark.parametrize("conflict", [None, "shared.txt"], ids=["clean", "conflicted"])
def test_link_merge_all_of_a_branch_removing_a_link_changed_here_takes_the_removal(
    new_lore_repo, conflict
):
    """A link the merged branch removes while this branch changed its repository through it
    conflicts at the mount, once a conflict inside it is resolved. Taking the merged side
    removes the link with its merge, which taking this branch's side again can't restore, and
    the merge commits."""
    urc, _ = merge_a_branch_removing_a_link(
        new_lore_repo, added=False, pinned=False, conflict=conflict
    )
    if conflict:
        _resolve_the_conflict_inside_the_removed_link(urc)
    assert urc.unresolved_paths() == {"libs/old"}

    urc.branch_merge_resolve_theirs("libs/old")
    with pytest.raises(NotSupportedError, match="whose repository the merge merged"):
        urc.branch_merge_resolve_mine("libs/old")
    urc.commit("Merge feature-branch")
    urc.push()

    for repo in [urc, urc.clone()]:
        assert registered_links(repo) == {}
        assert not repo.path_exists("libs/old"), "the merge should remove the mount"


@pytest.mark.smoke
def test_link_merge_all_of_a_branch_removing_a_link_changed_here_keeps_the_link(
    new_lore_repo,
):
    """Keeping a link the merged branch removed, once a conflict inside it is resolved, keeps
    its repository's merge: the link holds both branches' changes to it, and its merge lands on
    linked main with the parent's."""
    urc, link_repo = merge_a_branch_removing_a_link(
        new_lore_repo, added=False, pinned=False, conflict="shared.txt"
    )
    _resolve_the_conflict_inside_the_removed_link(urc)

    urc.branch_merge_resolve_mine("libs/old")
    urc.commit("Merge feature-branch")
    urc.push()

    exposed = ["feature.txt", "linked.txt", "main.txt", "shared.txt"]
    for repo in [urc, urc.clone()]:
        assert registered_links(repo) == {"libs/old": link_repo.get_id()}
        assert mount_listing(repo, "libs/old") == exposed
    link_repo.sync()
    assert link_repo.file_exists("feature.txt") and link_repo.file_exists("main.txt")


def _link_row(repo, link_path: str) -> tuple:
    """What a merge carries of the row of the link at `link_path`: its repository and flags,
    the branch it names unless it follows its parent's, and its pin unless it follows it, as
    the linked branch it lands on takes its pin from a merge."""
    entry = link_entry(repo, link_path)
    tracking = entry["tracking"]
    return (
        entry["link"],
        entry["flags"],
        None if tracking else entry["branch"],
        None if tracking else entry["revision"],
    )


def _link_removed_on_feature_and_changed_on_main(
    new_lore_repo, change: str, removed: str
):
    """Returns (urc, link_repo, row, files) with main checked out, where `feature-branch`
    removed the link at `libs/old`, then the `directory` `libs` holding it where `removed`
    says so, and main made `change` to it: `edit_inside` the link, the same after
    `feature-branch` edited inside it too (`edit_inside_both`), a `pin_update` or a
    `branch_change` of a fixed link, or a `flag_change` re-adding it fixed. `row` is main's row
    of the link, as `_link_row` reads it, and `files` what a link holding both branches'
    changes serves."""
    urc = make_repo(
        new_lore_repo, {"main-file.txt": "main base\n", "libs/readme.txt": "libs\n"}
    )
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked base\n"})
    fixed = change in ("pin_update", "branch_change")
    urc.link_add("libs/old", link_repo.get_id(), "/", disable_branching=fixed)
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")
    feature_files = {}
    if change == "edit_inside_both":
        feature_files = {"feature.txt": "f\n"}
        urc.write_commit_push("Feature changes the link", {"libs/old/feature.txt": "f\n"})
    urc.link_remove("libs/old")
    urc.commit("Feature removes the link")
    if removed == "directory":
        urc.rmtree("libs")
        urc.stage("libs", scan=True)
        urc.commit("Feature removes the directory that held the link")
    urc.push()

    urc.branch_switch("main")
    if change.startswith("edit_inside"):
        urc.write_commit_push("Main changes the link", {"libs/old/main.txt": "m\n"})
    elif change == "pin_update":
        link_repo.write_commit_push("Second revision", {"linked.txt": "two\n"})
        urc.link_update("libs/old", pin=link_repo.branch_info().local_latest)
        urc.commit("Main moves the pin")
        urc.push()
    elif change == "branch_change":
        link_repo.branch_create("other")
        link_repo.write_commit_push("On another branch", {"linked.txt": "other\n"})
        urc.link_update("libs/old", pin="other@LATEST")
        urc.commit("Main follows another branch")
        urc.push()
    else:
        urc.link_remove("libs/old")
        urc.commit("Main removes the link")
        urc.link_add("libs/old", link_repo.get_id(), "/", disable_branching=True)
        urc.commit("Main links it again fixed")
        urc.push()
    files = {**mounted_files(urc, "libs/old"), **feature_files}
    return urc, link_repo, _link_row(urc, "libs/old"), files


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("change", "into", "resolve", "removed"),
    [
        ("edit_inside", "main", "mine", "link"),
        ("edit_inside", "feature-branch", "theirs", "link"),
        ("edit_inside", "feature-branch", "bare", "link"),
        ("edit_inside", "main", "theirs", "directory"),
        ("edit_inside", "feature-branch", "theirs", "directory"),
        ("edit_inside_both", "main", "mine", "link"),
        ("edit_inside_both", "feature-branch", "theirs", "link"),
        ("pin_update", "main", "mine", "link"),
        ("pin_update", "feature-branch", "mine", "link"),
        ("branch_change", "feature-branch", "theirs", "link"),
        ("flag_change", "main", "theirs", "link"),
        ("flag_change", "feature-branch", "theirs", "link"),
    ],
)
def test_link_merge_of_a_link_removed_on_one_branch_and_changed_on_the_other_conflicts(
    new_lore_repo, change, into, resolve, removed
):
    """A link one branch removed, alone or with the directory holding it, and the other
    changed, through its mount or in its row, conflicts at the mount in both directions of the
    merge. The side holding the link keeps main's row with both branches' changes to its
    repository, which a following link lands on its linked branch with, and the removing side,
    bare resolve on it among them, leaves no link; the directory's other files go either way.
    The revision verifies, clones the same, and a merge after it has nothing left."""
    urc, link_repo, row, files = _link_removed_on_feature_and_changed_on_main(
        new_lore_repo, change, removed
    )
    merged = "main" if into == "feature-branch" else "feature-branch"
    urc.branch_switch(into)
    urc.branch_merge_start(merged, message=f"Merge {merged}")
    assert urc.unresolved_paths() == {"libs/old"}

    if resolve == "bare":
        urc.branch_merge_resolve("libs/old")
    else:
        getattr(urc, f"branch_merge_resolve_{resolve}")("libs/old")
    urc.commit(f"Merge {merged}")

    holds_link = (into == "main") == (resolve != "theirs")

    def assert_holds_the_side_named(repo: Lore):
        if holds_link:
            assert _link_row(repo, "libs/old") == row
            assert mounted_files(repo, "libs/old") == files
        else:
            assert registered_links(repo) == {}
            assert not repo.path_exists("libs/old")
        if removed == "directory":
            assert not repo.path_exists("libs/readme.txt"), (
                "the removed directory's other files must go"
            )

    assert_merge_settled(urc, merged, assert_holds_the_side_named)
    if holds_link and row[2] is None:
        linked = link_repo.clone(branch=into)
        assert linked.branch_info().local_latest == link_pin(urc, link_repo.get_id()), (
            "the link must pin the latest of the linked branch it follows"
        )


@pytest.mark.smoke
@pytest.mark.parametrize("other", ["unrelated", "removed"])
def test_link_merge_of_a_link_removed_on_one_branch_and_left_or_removed_on_the_other_removes_it(
    new_lore_repo, other
):
    """A link one branch removed is removed by a merge with a branch that changed something
    else, or removed it too, and commits, leaving out a per-link message naming it."""
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked base\n"})
    urc.link_add("libs/old", link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()
    urc.branch_create("feature-branch")
    urc.link_remove("libs/old")
    urc.commit("Feature removes the link")
    urc.push()
    urc.branch_switch("main")
    if other == "removed":
        urc.link_remove("libs/old")
        urc.commit("Main removes the link")
        urc.push()
    else:
        urc.write_commit_push("Main changes the parent", {"main-file.txt": "main\n"})

    urc.branch_merge(
        "feature-branch",
        message="Merge feature-branch",
        link_messages={"libs/old": "Merge the removed link"} if other == "unrelated" else None,
    )

    assert parse_status_json(urc.status(json=True)) == [], "the merge should commit"
    assert registered_links(urc) == {}
    assert not urc.path_exists("libs/old")


@pytest.mark.smoke
@pytest.mark.parametrize("pinned", [False, True], ids=["tracking", "pinned"])
@pytest.mark.parametrize(
    "added", [False, True], ids=["removed", "removed and added elsewhere"]
)
def test_link_merge_abort_all_of_a_branch_removing_a_link(new_lore_repo, pinned, added):
    """Aborting a merge that removes a link puts the link back where it was."""
    urc, link_repo = merge_a_branch_removing_a_link(
        new_lore_repo, added, pinned, conflict="main-file.txt"
    )

    urc.branch_merge_abort()

    assert registered_links(urc) == {"libs/old": link_repo.get_id()}
    assert sorted(os.listdir(os.path.join(urc.path, "libs"))) == ["old"]
    assert sorted(os.listdir(os.path.join(urc.path, "libs/old"))) == [
        "linked.txt",
        "main.txt",
        "shared.txt",
    ]
    assert parse_status_json(urc.status(json=True)) == [], (
        "the abort should leave nothing staged"
    )


@pytest.mark.smoke
def test_link_merge_all_resumed_after_the_merged_branch_removes_a_conflicted_link(
    new_lore_repo,
):
    """A merge stopped on a conflict in a link, resumed once the merged branch has removed that
    link, conflicts at the mount, and taking the merged side carries the removal."""
    urc, _, link_path = setup_link_merge_conflict(new_lore_repo, link_path="libs/old")
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    conflicted = f"{link_path}/data.txt"
    assert urc.unresolved_paths() == {conflicted}

    remover = urc.clone(branch="feature-branch")
    remover.link_remove(link_path)
    remover.commit("Feature removes the link")
    remover.push()

    urc.write_files({conflicted: "resolved\n"})
    urc.branch_merge_resolve(conflicted)
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    assert urc.unresolved_paths() == {link_path}

    urc.branch_merge_resolve_theirs(link_path)
    urc.commit("Merge feature-branch")

    assert registered_links(urc) == {}
    assert not urc.path_exists(link_path)
    assert parse_status_json(urc.status(json=True)) == [], (
        "the merge should commit with nothing left staged"
    )


@pytest.mark.smoke
@pytest.mark.parametrize("side", ["mine", "theirs"])
def test_link_merge_all_of_a_branch_linking_another_repository_at_a_removed_link(
    new_lore_repo, side
):
    """A branch that removes a link this branch changed, and links another repository at its
    path, conflicts at the mount. Either side commits whole: the link with both branches'
    changes to its repository, or the other repository at the pin the merged branch gives it."""
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked base\n"})
    other_repo = make_repo(new_lore_repo, {"other.txt": "other\n"})
    urc.link_add("libs/old", link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")
    urc.write_commit_push("Feature changes the link", {"libs/old/feature.txt": "f\n"})
    urc.link_remove("libs/old")
    urc.link_add("libs/old", other_repo.get_id(), "/")
    other_pin = link_pin(urc, other_repo.get_id())
    urc.commit("Feature links another repository at the path")
    urc.push()

    urc.branch_switch("main")
    urc.write_commit_push("Main changes the link", {"libs/old/main.txt": "m\n"})
    urc.branch_merge("feature-branch", message="Merge feature-branch")
    assert urc.unresolved_paths() == {"libs/old"}

    getattr(urc, f"branch_merge_resolve_{side}")("libs/old")
    urc.commit("Merge feature-branch")
    urc.push()

    kept, exposed = (
        (link_repo, ["feature.txt", "linked.txt", "main.txt"])
        if side == "mine"
        else (other_repo, ["other.txt"])
    )
    assert registered_links(urc) == {"libs/old": kept.get_id()}
    if side == "theirs":
        assert link_pin(urc, other_repo.get_id()) == other_pin
    for repo in [urc, urc.clone()]:
        assert mount_listing(repo, "libs/old") == exposed


@pytest.mark.smoke
def test_link_merge_abort_of_a_merge_resumed_after_the_merged_branch_removes_a_link(
    new_lore_repo,
):
    """A merge resumed after the merged branch removed a conflicted link, which then stops on
    that removal and a conflict of the parent's own, aborts back to the link where it was."""
    urc, link_repo, link_path = setup_link_merge_conflict(
        new_lore_repo, link_path="libs/old"
    )
    urc.write_commit_push("Main changes the parent", {"main-file.txt": "main\n"})
    remover = urc.clone(branch="feature-branch")
    remover.write_commit_push(
        "Feature changes the parent", {"main-file.txt": "feature\n"}
    )
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    conflicted = f"{link_path}/data.txt"
    assert urc.unresolved_paths() == {conflicted}

    remover.link_remove(link_path)
    remover.commit("Feature removes the link")
    remover.push()
    urc.write_files({conflicted: "resolved\n"})
    urc.branch_merge_resolve(conflicted)
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    assert urc.unresolved_paths() == {link_path, "main-file.txt"}

    urc.branch_merge_abort()

    assert registered_links(urc) == {link_path: link_repo.get_id()}
    with urc.open_file(conflicted) as f:
        assert f.read() == "mine\n", (
            "the abort should restore the link's pre-merge content"
        )
    assert parse_status_json(urc.status(json=True)) == [], (
        "the abort should leave nothing staged"
    )


@pytest.mark.smoke
def test_link_merge_all_resumed_after_a_failed_link_merges_the_links_merged_before_it(
    new_lore_repo,
):
    """A merge stopped on a conflict in its first link and resolved, resumed, merges a link the
    merged branch has changed since, conflicting, and fails at another. Resumed again once the
    merged branch has taken back the mount that failed it, it merges the conflicted link again and
    stops on its conflict, keeping the first link's resolution."""
    urc, nested = parent_with_a_link_merging_into_a_failure(new_lore_repo)
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    resolve_the_only_conflict(urc, "libs/first/data.txt")

    feature = mount_part_inside_the_second_link_on_feature(urc, nested)
    feature.write_commit_push(
        "Feature changes the other link",
        {"libs/other/feature.txt": "feature\n", "libs/other/other.txt": "feature\n"},
    )
    start_the_merge_failing_at_the_second_link(urc)
    unmount_the_part_inside_the_second_link(feature)

    urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    resolve_the_only_conflict(urc, "libs/other/other.txt")
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    urc.push()

    for repo in [urc, urc.clone()]:
        assert_link_holds(
            repo,
            "libs/first",
            {
                "data.txt": "resolved\n",
                "feature.txt": "feature\n",
                "main.txt": "main\n",
            },
        )
        assert_link_holds(
            repo,
            "libs/other",
            {
                "feature.txt": "feature\n",
                "main.txt": "main\n",
                "other.txt": "resolved\n",
            },
        )


@pytest.mark.smoke
@pytest.mark.parametrize("pinned", [False, True], ids=["tracking", "pinned"])
@pytest.mark.parametrize("how", LINK_MOVES.keys())
def test_link_merge_all_of_a_branch_moving_a_link(new_lore_repo, pinned, how):
    """A link the merged branch renames in case, or carries along with a directory renamed in
    case, is merged in its own right where it moves to. The diff holds a directory moved elsewhere
    as a delete and an add, so the merge carries the removal of the link inside it, and merges
    the repository of the link the branch adds there."""
    moved, destination, exposed = LINK_MOVES[how]
    urc, link_repo = merge_a_branch_moving_a_link(new_lore_repo, pinned, moved)
    urc.push()

    assert registered_links(urc) == {destination: link_repo.get_id()}
    for repo in [urc, urc.clone()]:
        assert sorted(os.listdir(os.path.join(repo.path, destination))) == exposed


@pytest.mark.smoke
@pytest.mark.parametrize("pinned", [False, True], ids=["tracking", "pinned"])
@pytest.mark.parametrize("how", LINK_MOVES.keys())
def test_link_merge_abort_of_a_branch_moving_a_link(new_lore_repo, pinned, how):
    """Aborting a merge that moves a link, or the directory holding it, puts the link back where
    it was."""
    moved = LINK_MOVES[how][0]
    urc, link_repo = merge_a_branch_moving_a_link(
        new_lore_repo, pinned, moved, conflict=True
    )
    assert urc.unresolved_paths() == {"main-file.txt"}

    urc.branch_merge_abort()

    assert registered_links(urc) == {"libs/old": link_repo.get_id()}
    root = os.listdir(urc.path)
    assert "libs" in root and "Libs" not in root and "vendor" not in root
    assert os.listdir(os.path.join(urc.path, "libs")) == ["old"]
    assert sorted(os.listdir(os.path.join(urc.path, "libs/old"))) == [
        "linked.txt",
        "main.txt",
    ]
    assert parse_status_json(urc.status(json=True)) == [], (
        "the abort should leave nothing staged"
    )


def assert_files_hold(repo, files: dict[str, str]):
    assert files.items() <= repo.read_files().items()


NESTED_LINK = "vendor/b/deps/c"


@pytest.mark.smoke
@pytest.mark.parametrize(
    "link",
    [None, "vendor/b", "mine", NESTED_LINK],
    ids=["all", "link", "mine", "nested"],
)
def test_link_merge_all_merges_a_nested_link(new_lore_repo, link):
    """A merge, of every link, of the outer link alone or of the nested link alone, merges the
    repository of a link nested inside another link, so each branch's changes inside the merged
    repositories arrive, each repository pins the merge of the one it links, and the merge pushes
    and verifies. Merging a link again changes nothing. A conflict in the outer link resolves to
    one side beside the nested link's merge."""
    urc, repo_b, repo_c, outer, nested = build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "deps/c"
    )
    urc.branch_create("feature")
    urc.write_commit_push(
        "Feature changes both links",
        {f"{nested}/c-data/inner.txt": "feature\n", f"{outer}/b-root.txt": "feature\n"},
    )
    urc.branch_switch("main")
    main_changes = {f"{nested}/main.txt": "main\n", f"{outer}/main.txt": "main\n"}
    if link == "mine":
        main_changes[f"{outer}/b-root.txt"] = "main\n"
    urc.write_commit_push("Main changes both links", main_changes)

    if link == "mine":
        urc.branch_merge_start("feature", message="Merge feature")
        assert urc.unresolved_paths() == {f"{outer}/b-root.txt"}
        urc.branch_merge_resolve_mine()
        urc.commit("Merge feature")
    else:
        urc.branch_merge_start("feature", message="Merge feature", link=link)
    urc.push()

    b_root = {"mine": "main\n", NESTED_LINK: "content from repository B\n"}
    for repo in [urc, urc.clone()]:
        assert_files_hold(
            repo,
            {
                "a-root.txt": "content from repository A\n",
                f"{nested}/c-data/inner.txt": "feature\n",
                f"{nested}/main.txt": "main\n",
                f"{outer}/b-root.txt": b_root.get(link, "feature\n"),
                f"{outer}/main.txt": "main\n",
            },
        )
    for linked in [repo_b, repo_c]:
        linked.sync()
        assert link_pins(urc, linked.get_id()) == [linked.branch_info().local_latest]
    assert_files_hold(repo_c, {"c-data/inner.txt": "feature\n", "main.txt": "main\n"})
    urc.repository_verify()

    if link in ["vendor/b", NESTED_LINK]:
        latest = urc.branch_info().local_latest
        urc.branch_merge_start("feature", message="Merge feature again", link=link)
        assert urc.branch_info().local_latest == latest
        assert parse_status_json(urc.status(json=True)) == []


@dataclass
class NestedConflictMerge:
    urc: Lore
    conflicted: str
    """The path in conflict."""
    merged_outer: dict[str, str]
    """The files the merge of the outer repository adds."""
    pins: dict[str, str]
    """The pins of both linked repositories before the merge."""
    nested_id: str
    """The repository the nested link links."""
    latest: str
    """The latest revision before the merge."""


def merge_with_a_conflict_in_a_nested_link(new_lore_repo, link) -> NestedConflictMerge:
    """A merge of every link, of the outer link alone or of the nested link alone, which marks a
    conflict inside the link nested in the outer one at its mount."""
    urc, repo_b, repo_c, _, nested = build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "deps/c"
    )
    beside = make_repo(new_lore_repo, {"beside.txt": "base\n"})
    urc.link_add("vendor/a", beside.get_id(), "/")
    urc.commit("Link a repository beside")
    urc.push()
    conflicted = f"{nested}/c-data/inner.txt"
    urc.branch_create("feature")
    urc.write_commit_push(
        "Feature changes the links",
        {
            conflicted: "feature\n",
            "vendor/a/feature.txt": "feature\n",
            "vendor/b/feature.txt": "feature\n",
        },
    )
    urc.branch_switch("main")
    urc.write_commit_push(
        "Main changes the links", {conflicted: "main\n", "vendor/a/main.txt": "main\n"}
    )
    latest = urc.branch_info().local_latest
    merged_outer = {} if link == NESTED_LINK else {"vendor/b/feature.txt": "feature\n"}
    pins = {repo.get_id(): link_pin(urc, repo.get_id()) for repo in [repo_b, repo_c]}

    urc.branch_merge_start("feature", message="Merge feature", link=link)
    assert urc.unresolved_paths() == {conflicted}
    listed = parse_jsonl(
        urc.branch_merge_conflicts_list(json=True), "branchMergeConflictFile"
    )
    assert [entry["path"] for entry in listed] == [conflicted]
    with urc.open_file(conflicted) as f:
        assert "<<<<<<<" in f.read()
    return NestedConflictMerge(urc, conflicted, merged_outer, pins, repo_c.get_id(), latest)


@pytest.mark.smoke
@pytest.mark.parametrize(
    "link, through",
    [
        (None, None),
        ("vendor/b", None),
        (NESTED_LINK, None),
        (NESTED_LINK, NESTED_LINK),
        (NESTED_LINK, "vendor/b"),
    ],
    ids=["all", "link", "nested", "nested-commit-link", "nested-commit-outer-link"],
)
def test_link_merge_all_conflict_in_a_nested_link(new_lore_repo, link, through):
    """A conflict inside a link nested in another link, which a merge of every link, of the outer
    link alone or of the nested link alone marks, refuses a commit and a resolve to one side.
    Resolved with no path named, the merge commits, first through the link at `through` where it
    is set, and pushes. A merge of every link is refused while the nested link's merge alone is in
    progress."""
    merge = merge_with_a_conflict_in_a_nested_link(new_lore_repo, link)
    urc, conflicted = merge.urc, merge.conflicted
    with pytest.raises(UnresolvedConflictError):
        urc.commit("Merge feature")
    with pytest.raises(NotSupportedError):
        urc.branch_merge_resolve_mine(conflicted)
    urc.write_files({conflicted: "resolved\n"})
    urc.branch_merge_resolve()
    if link == NESTED_LINK:
        with pytest.raises(NotSupportedError):
            urc.branch_merge_start("feature", message="Merge feature")
    if through:
        urc.commit("Merge feature", link=through)
    urc.commit("Merge feature")
    urc.push()

    merged_beside = {} if link else {"vendor/a/feature.txt": "feature\n"}
    for repo in [urc, urc.clone()]:
        assert_files_hold(
            repo,
            {
                conflicted: "resolved\n",
                "vendor/a/main.txt": "main\n",
                **merged_beside,
                **merge.merged_outer,
            },
        )


@pytest.mark.smoke
@pytest.mark.parametrize(
    "link, through",
    [
        (None, None),
        (NESTED_LINK, None),
        (NESTED_LINK, NESTED_LINK),
        (NESTED_LINK, "vendor/b"),
        ("vendor/b", NESTED_LINK),
    ],
    ids=["all", "nested", "nested-link", "nested-outer-link", "link-nested-link"],
)
def test_link_merge_all_abort_of_a_conflict_in_a_nested_link(new_lore_repo, link, through):
    """An abort of a merge holding a conflict inside a link nested in another link, once the
    conflict is resolved, restores the nested link and its pin, aborting the link at `through`
    first where it is set. The abort of either link ends a merge of the nested link alone."""
    merge = merge_with_a_conflict_in_a_nested_link(new_lore_repo, link)
    urc, conflicted = merge.urc, merge.conflicted
    urc.write_files({conflicted: "resolved\n"})
    urc.branch_merge_resolve(conflicted)
    if through:
        urc.branch_merge_abort(link=through)
        assert link_pin(urc, merge.nested_id) == merge.pins[merge.nested_id]
        assert_files_hold(urc, {conflicted: "main\n", **merge.merged_outer})
    if through and link == NESTED_LINK:
        with pytest.raises(NotInProgress):
            urc.branch_merge_abort()
    else:
        urc.branch_merge_abort()
    assert urc.branch_info().local_latest == merge.latest
    assert parse_status_json(urc.status(json=True)) == []
    assert {repo: link_pin(urc, repo) for repo in merge.pins} == merge.pins
    assert_files_hold(urc, {conflicted: "main\n"})


@pytest.mark.smoke
def test_link_merge_link_nested_two_links_deep(new_lore_repo):
    """A merge of a link nested two links deep merges its repository, which the links above it
    pin, and commits, pushes and verifies."""
    urc, deepest = build_link_chain(new_lore_repo, 3)
    urc.branch_create("feature")
    urc.write_commit_push("Feature", {f"{deepest}/a.txt": "feature\n"})
    urc.branch_switch("main")
    urc.write_commit_push("Main", {f"{deepest}/b.txt": "main\n", "l1/b.txt": "main\n"})

    urc.branch_merge_start("feature", message="Merge feature", link=deepest)
    urc.push()

    for repo in [urc, urc.clone()]:
        assert_files_hold(
            repo,
            {
                f"{deepest}/a.txt": "feature\n",
                f"{deepest}/b.txt": "main\n",
                "l1/b.txt": "main\n",
            },
        )
    urc.repository_verify()


@pytest.mark.smoke
@pytest.mark.parametrize("finish", ["commit", "abort"])
def test_link_merge_link_nested_in_a_link_mounting_a_folder(new_lore_repo, finish):
    """A merge of a link nested in a link that mounts a folder of its repository, which holds the
    nested link below that folder, commits and pushes, or aborts back to its pin and files with
    nothing left staged."""
    repo_c = make_repo(new_lore_repo, {"c.txt": "base\n"})
    repo_b = make_repo(new_lore_repo, {"sub/b.txt": "b\n", "outside.txt": "outside\n"})
    repo_b.link_add("sub/deps/c", repo_c.get_id(), "/")
    repo_b.commit("Link C in the folder")
    repo_b.push()
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    urc.link_add("vendor/b", repo_b.get_id(), "/sub")
    urc.commit("Link a folder of B")
    urc.push()
    nested = "vendor/b/deps/c"
    urc.branch_create("feature")
    urc.write_commit_push("Feature", {f"{nested}/feature.txt": "feature\n"})
    urc.branch_switch("main")
    urc.write_commit_push("Main", {f"{nested}/main.txt": "main\n"})
    pins = {repo.get_id(): link_pin(urc, repo.get_id()) for repo in [repo_b, repo_c]}

    urc.branch_merge_start(
        "feature", message="Merge feature", link=nested, no_commit=finish == "abort"
    )
    merged = {f"{nested}/feature.txt": "feature\n", f"{nested}/main.txt": "main\n"}
    assert_files_hold(urc, merged)
    if finish == "abort":
        urc.branch_merge_abort(link=nested)
        assert {repo: link_pin(urc, repo) for repo in pins} == pins
        assert sorted(os.listdir(os.path.join(urc.path, nested))) == ["c.txt", "main.txt"]
        assert parse_status_json(urc.status(json=True)) == []
        return
    urc.push()
    for repo in [urc, urc.clone()]:
        assert_files_hold(repo, {**merged, "vendor/b/b.txt": "b\n"})
    urc.repository_verify()


@pytest.mark.smoke
@pytest.mark.parametrize(
    "outer, abort",
    [
        ("", "link"),
        ("", "all"),
        ("", "dry-run"),
        ("outer", "link"),
        ("outer", "all"),
        ("outer", "dry-run"),
        ("outer", "holder"),
    ],
    ids=[
        "top-level-link",
        "top-level-all",
        "top-level-dry-run",
        "nested-link",
        "nested-all",
        "nested-dry-run",
        "nested-holder",
    ],
)
def test_link_merge_abort_of_two_link_merges_in_one_repository(new_lore_repo, outer, abort):
    """Two merges of a link each, left uncommitted in the same repository, the main one or one a
    link mounts, abort back to each link's pin and files, link by link, at once or through the
    link holding them, leaving nothing staged. A dry run of either abort leaves both merges in
    place."""
    urc, prefix = repo_holding_links(new_lore_repo, outer)
    links = {
        f"{prefix}libs/{name}": make_repo(new_lore_repo, {"data.txt": "base\n"})
        for name in ["x", "y"]
    }
    for path, repo in links.items():
        urc.link_add(path, repo.get_id(), "/")
    urc.commit("Link two repositories")
    urc.push()
    urc.branch_create("feature")
    urc.write_commit_push("Feature", {f"{path}/feature.txt": "feature\n" for path in links})
    urc.branch_switch("main")
    urc.write_commit_push("Main", {f"{path}/main.txt": "main\n" for path in links})
    pins = {repo.get_id(): link_pin(urc, repo.get_id()) for repo in links.values()}

    for path in links:
        urc.branch_merge_start("feature", message="Merge feature", link=path, no_commit=True)
    merged = {repo: link_pin(urc, repo) for repo in pins}
    if abort == "dry-run":
        urc.branch_merge_abort(link=next(iter(links)), dry_run=True)
        urc.branch_merge_abort(dry_run=True)
        assert {repo: link_pin(urc, repo) for repo in pins} == merged
        for path in links:
            assert sorted(os.listdir(os.path.join(urc.path, path))) == [
                "data.txt",
                "feature.txt",
                "main.txt",
            ]
        return
    if abort == "link":
        for path in links:
            urc.branch_merge_abort(link=path)
    elif abort == "holder":
        urc.branch_merge_abort(link=outer)
    else:
        urc.branch_merge_abort()

    assert {repo: link_pin(urc, repo) for repo in pins} == pins
    for path in links:
        assert sorted(os.listdir(os.path.join(urc.path, path))) == ["data.txt", "main.txt"]
    assert parse_status_json(urc.status(json=True)) == []
    with pytest.raises(NotInProgress):
        urc.branch_merge_abort()


@pytest.mark.smoke
@pytest.mark.parametrize(
    "staged, abort",
    [
        ("main-file", "all"),
        ("main-file-and-conflict", "all"),
        ("main-file", "link"),
        ("during-a-conflict-after-a-merge-commit", "all"),
        ("during-a-conflict-of-every-link-after-a-merge-commit", "all"),
        ("in-the-holding-link", "link"),
        ("in-the-holding-link-during-the-merge", "link"),
        ("link-pin", "link"),
    ],
)
def test_link_merge_abort_keeps_what_else_is_staged(new_lore_repo, staged, abort):
    """An abort of a merge of a link alone, whole or of the link, keeps what was staged beside
    it, before the merge or during it: a change of the main repository's, with the merge in
    conflict or not and on a merge commit or not, a change of the repository holding the link, and
    a pin of the link itself, which leaves the link staged. The abort ends the merge."""
    outer = "outer" if staged.startswith("in-the-holding-link") else ""
    conflict = "conflict" in staged
    urc, prefix = repo_holding_links(new_lore_repo, outer)
    path = f"{prefix}libs/x"
    linked = make_repo(new_lore_repo, {"data.txt": "base\n"})
    urc.link_add(path, linked.get_id(), "/")
    urc.commit("Link a repository")
    urc.push()
    urc.branch_create("side")
    urc.write_commit_push("Side", {"side.txt": "side\n"})
    urc.branch_switch("main")
    urc.branch_create("feature")
    urc.write_commit_push("Feature", {f"{path}/data.txt": "feature\n"})
    urc.branch_switch("main")
    first = link_pin(urc, linked.get_id())
    changed = "data.txt" if conflict else "main.txt"
    urc.write_commit_push("Main", {f"{path}/{changed}": "main\n"})
    if "merge-commit" in staged:
        urc.branch_merge_start("side", message="Merge side")
    pin = link_pin(urc, linked.get_id())
    beside = f"{prefix}main-file.txt"
    during = "during" in staged
    if staged == "link-pin":
        urc.link_update(path, pin=first)
        pin = first
    elif not during:
        urc.write_files({beside: "staged\n"})
        urc.stage(beside)

    urc.branch_merge_start(
        "feature",
        message="Merge feature",
        link=None if "every-link" in staged else path,
        no_commit=True,
        check=not conflict,
    )
    if during:
        urc.write_files({beside: "staged\n"})
        urc.stage(beside)
    if abort == "all":
        urc.branch_merge_abort()
    else:
        urc.branch_merge_abort(link=path)

    assert link_pin(urc, linked.get_id()) == pin
    with pytest.raises(NotInProgress):
        urc.branch_merge_abort()
    listed = {entry["path"] for entry in parse_status_json(urc.status(json=True))}
    if staged == "link-pin":
        assert listed == {path}
        return
    assert beside in listed
    assert path not in listed
    data = "main\n" if conflict else "base\n"
    assert_files_hold(urc, {beside: "staged\n", f"{path}/data.txt": data})


@pytest.mark.smoke
def test_link_merge_on_a_merge_revision_commits_one_parent(new_lore_repo):
    """A merge of a link alone on a merge revision commits a revision whose one parent is that
    merge revision."""
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    path = "libs/x"
    linked = make_repo(new_lore_repo, {"data.txt": "base\n"})
    urc.link_add(path, linked.get_id(), "/")
    urc.commit("Link a repository")
    urc.push()
    for branch in ["side", "feature"]:
        urc.branch_create(branch)
        urc.write_commit_push(branch, {f"{path}/{branch}.txt": f"{branch}\n"})
        urc.branch_switch("main")
    urc.write_commit_push("Main", {"main-file.txt": "main\n"})
    urc.branch_merge_start("side", message="Merge side")
    merged = urc.branch_info().local_latest
    assert urc.revision_info(merged).merge

    urc.branch_merge_start("feature", message="Merge feature", link=path)

    committed = urc.revision_info()
    assert committed.signature != merged
    assert committed.parent == merged
    assert not committed.merge


@pytest.mark.smoke
def test_link_merge_abort_of_a_nested_link_keeps_the_merge_of_the_link_holding_it(
    new_lore_repo,
):
    """An abort of the merge of a link nested in a link merged in its own right, whose merge
    changes nothing beside the nested link, keeps the merge of the link holding it."""
    urc, _, repo_c, _, nested = build_nested_link_repos_at(new_lore_repo, "vendor/b", "deps/c")
    urc.branch_create("feature")
    urc.write_commit_push("Feature", {f"{nested}/feature.txt": "feature\n"})
    urc.branch_switch("main")
    urc.write_commit_push("Main", {f"{nested}/main.txt": "main\n"})
    pin = link_pin(urc, repo_c.get_id())

    urc.branch_merge_start("feature", message="Merge feature", link="vendor/b", no_commit=True)
    urc.branch_merge_abort(link=nested)

    assert link_pin(urc, repo_c.get_id()) == pin
    assert sorted(os.listdir(os.path.join(urc.path, nested))) == ["c-data", "main.txt"]
    urc.branch_merge_abort()
    assert parse_status_json(urc.status(json=True)) == []


@pytest.mark.smoke
@pytest.mark.parametrize(
    "outer, abort",
    [("", "libs/x"), ("outer", "outer/libs/x"), ("outer", "outer")],
    ids=["top-level", "nested", "nested-holder"],
)
def test_link_merge_commit_link_ends_the_merge_it_commits(new_lore_repo, outer, abort):
    """`commit --link` of a link whose merge alone is in progress, the main repository's own or
    nested in a link, commits the merge and ends it. `branch merge abort`, with `--link` of the
    link or of the link holding it, or with `--ignore-links`, finds no merge, so the pins the
    commit made stay for the next commit to take."""
    urc, prefix = repo_holding_links(new_lore_repo, outer)
    path = f"{prefix}libs/x"
    linked = make_repo(new_lore_repo, {"data.txt": "base\n"})
    urc.link_add(path, linked.get_id(), "/")
    urc.commit("Link a repository")
    urc.push()
    urc.branch_create("feature")
    urc.write_commit_push("Feature", {f"{path}/feature.txt": "feature\n"})
    urc.branch_switch("main")
    urc.write_commit_push("Main", {f"{path}/main.txt": "main\n"})
    links = [path, outer] if outer else [path]
    before = {link: link_pin_at(urc, link) for link in links}

    urc.branch_merge_start("feature", message="Merge feature", link=path, no_commit=True)
    urc.commit("Merge feature", link=path)
    pins = {link: link_pin_at(urc, link) for link in links}
    assert all(pins[link] != before[link] for link in links)

    with pytest.raises(NotInProgress):
        urc.branch_merge_abort(link=abort)
    with pytest.raises(NotInProgress):
        urc.branch_merge_abort()
    with pytest.raises(NotInProgress):
        urc.branch_merge_abort(ignore_links=True)
    assert {link: link_pin_at(urc, link) for link in links} == pins
    urc.commit("Take the merged link")
    urc.push()
    merged = {f"{path}/feature.txt": "feature\n", f"{path}/main.txt": "main\n"}
    for repo in [urc, urc.clone()]:
        assert_files_hold(repo, merged)


@pytest.mark.smoke
@pytest.mark.parametrize("abort", ["link", "all", "dry-run"])
def test_link_merge_commit_link_of_one_of_two_link_merges(new_lore_repo, abort):
    """`commit --link` of one of two links whose merges alone are in progress leaves the other's
    merge in progress. An abort of the other link's merge, or of the merge, rolls that link back
    and keeps the committed pin. A dry run of the commit leaves the link's merge to abort."""
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    links = {f"libs/{name}": make_repo(new_lore_repo, {"data.txt": "base\n"}) for name in "xy"}
    for path, repo in links.items():
        urc.link_add(path, repo.get_id(), "/")
    urc.commit("Link two repositories")
    urc.push()
    urc.branch_create("feature")
    urc.write_commit_push("Feature", {f"{path}/feature.txt": "feature\n" for path in links})
    urc.branch_switch("main")
    urc.write_commit_push("Main", {f"{path}/main.txt": "main\n" for path in links})
    before = {path: link_pin_at(urc, path) for path in links}
    for path in links:
        urc.branch_merge_start("feature", message="Merge feature", link=path, no_commit=True)
    merged = {path: link_pin_at(urc, path) for path in links}

    if abort == "dry-run":
        urc.commit("Merge feature", link="libs/x", dry_run=True)
        urc.branch_merge_abort(link="libs/x")
        assert link_pin_at(urc, "libs/x") == before["libs/x"]
        return
    urc.commit("Merge feature", link="libs/x")
    committed = link_pin_at(urc, "libs/x")
    assert committed not in [before["libs/x"], merged["libs/x"]]
    urc.branch_merge_abort(link="libs/y" if abort == "link" else None)

    assert {path: link_pin_at(urc, path) for path in links} == {
        "libs/x": committed,
        "libs/y": before["libs/y"],
    }
    with pytest.raises(NotInProgress):
        urc.branch_merge_abort()


@pytest.mark.smoke
@pytest.mark.parametrize("finish", ["commit", "ignore-links", "link", "all"])
def test_link_merge_all_commit_link_during_the_main_repositorys_merge(new_lore_repo, finish):
    """`commit --link` of a link a merge of every link merged, while the main repository's merge
    holds a conflict, leaves that merge in progress. Its commit, with both parents, its abort but
    for the links and the abort of the link keep the pin the link's commit made. Its whole abort
    puts the pin of the current revision back."""
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    path = "libs/x"
    urc.link_add(path, make_repo(new_lore_repo, {"data.txt": "base\n"}).get_id(), "/")
    urc.commit("Link a repository")
    urc.push()
    urc.branch_create("feature")
    urc.write_commit_push(
        "Feature", {f"{path}/feature.txt": "feature\n", "main-file.txt": "feature\n"}
    )
    feature = urc.branch_info().local_latest
    urc.branch_switch("main")
    urc.write_commit_push("Main", {f"{path}/main.txt": "main\n", "main-file.txt": "main\n"})
    latest = urc.branch_info().local_latest
    before = link_pin_at(urc, path)

    urc.branch_merge_start("feature", message="Merge feature", check=False)
    assert urc.unresolved_paths() == {"main-file.txt"}
    urc.commit("Merge the link", link=path)
    committed = link_pin_at(urc, path)

    if finish == "link":
        urc.branch_merge_abort(link=path)
        assert link_pin_at(urc, path) == committed
        assert urc.unresolved_paths() == {"main-file.txt"}
        return
    if finish != "commit":
        urc.branch_merge_abort(ignore_links=finish == "ignore-links")
        assert link_pin_at(urc, path) == (committed if finish == "ignore-links" else before)
        assert_files_hold(urc, {"main-file.txt": "main\n"})
        return
    urc.write_files({"main-file.txt": "resolved\n"})
    urc.branch_merge_resolve()
    urc.commit("Merge feature")
    assert urc.revision_info().merge.split() == [latest, feature]
    assert link_pin_at(urc, path) == committed


@pytest.mark.smoke
def test_link_commit_link_of_a_link_nested_in_a_fixed_link_commits_on_its_branch(new_lore_repo):
    """`commit --link` of a link following the branch of a fixed link it is nested in commits on
    that link's branch, not on the main repository's."""
    repo_c = make_repo(new_lore_repo, {"c.txt": "c\n"})
    repo_b = make_repo(new_lore_repo, {"b.txt": "b\n"})
    urc = make_repo(new_lore_repo, {"a.txt": "a\n"})
    urc.link_add("outer", repo_b.get_id(), "/", disable_branching=True)
    urc.commit("Link B on its main branch")
    urc.link_add("outer/c", repo_c.get_id(), "/")
    urc.commit("Link C inside B")
    urc.push()
    urc.branch_create("feature")

    urc.write_files({"outer/c/new.txt": "new\n"})
    urc.stage("outer/c/new.txt")
    urc.commit("Change C", link="outer/c")
    urc.commit("Take the link's commit")
    urc.push()

    repo_c.sync()
    assert repo_c.branch_info().local_latest == link_pin_at(urc, "outer/c")
    assert_files_hold(repo_c, {"new.txt": "new\n"})


@pytest.mark.smoke
def test_link_merge_commit_link_refuses_a_conflict_in_the_link_holding_it(new_lore_repo):
    """`commit --link` of a link nested in a link whose merge holds a pin both branches moved, of
    another link nested beside it, is refused before it commits anything, naming that link."""
    urc, _ = make_parent_with_link(new_lore_repo, "outer", {"b.txt": "b\n"})
    path = "outer/libs/x"
    fixed = make_repo(new_lore_repo, {"f.txt": "one\n"})
    urc.link_add(path, make_repo(new_lore_repo, {"data.txt": "base\n"}).get_id(), "/")
    urc.link_add("outer/libs/fixed", fixed.get_id(), "/", disable_branching=True)
    urc.commit("Link two repositories inside the link")
    urc.push()
    pins = {}
    for content in ("two", "three"):
        fixed.write_commit_push(content, {"f.txt": f"{content}\n"})
        pins[content] = fixed.branch_info().local_latest
    for branch, pin in [("feature", "three"), ("main", "two")]:
        if branch == "feature":
            urc.branch_create(branch)
        else:
            urc.branch_switch(branch)
        urc.link_update("outer/libs/fixed", pin=pins[pin])
        urc.write_commit_push(branch, {f"{path}/{branch}.txt": f"{branch}\n"})

    urc.branch_merge_start("feature", message="Merge feature", link="outer", check=False)
    assert "Link conflict: outer/libs/fixed" in urc.branch_merge_conflicts_list()
    with pytest.raises(UnresolvedConflictError) as refused:
        urc.commit("Merge feature", link=path)
    assert "outer/libs/fixed" in str(refused.value)


@pytest.mark.smoke
def test_link_merge_abort_of_a_link_two_links_deep_keeps_work_in_the_first(
    new_lore_repo,
):
    """An abort of the merge of a link nested two links deep, whose outermost linked repository
    holds work staged beside the merge, restores the links below that repository to their pins
    and leaves them unstaged, keeping the work."""
    urc, deepest = build_link_chain(new_lore_repo, 3)
    urc.branch_create("feature")
    urc.write_commit_push("Feature", {f"{deepest}/feature.txt": "feature\n"})
    urc.branch_switch("main")
    urc.write_commit_push("Main", {f"{deepest}/main.txt": "main\n"})
    pins = {path: link_pin_at(urc, path) for path in ["l1/l2", deepest]}
    urc.write_files({"l1/a.txt": "staged\n"})
    urc.stage("l1/a.txt")

    urc.branch_merge_start(
        "feature", message="Merge feature", link=deepest, no_commit=True
    )
    urc.branch_merge_abort(link=deepest)

    assert {path: link_pin_at(urc, path) for path in pins} == pins
    assert sorted(os.listdir(os.path.join(urc.path, deepest))) == [
        "a.txt",
        "b.txt",
        "main.txt",
    ]
    listed = {entry["path"] for entry in parse_status_json(urc.status(json=True))}
    assert "l1/a.txt" in listed
    assert not listed & set(pins)
    with pytest.raises(NotInProgress):
        urc.branch_merge_abort()


@pytest.mark.smoke
@pytest.mark.parametrize("finish", ["resume", "abort"])
def test_link_merge_all_failing_partway_on_a_merge_revision(new_lore_repo, finish):
    """A merge of every link on a merge revision, which fails at a link once it merged one before
    it, resumes to a merge whose parents are that merge revision and the merged branch, or aborts
    back to it with nothing staged."""
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    nested = make_repo(new_lore_repo, {"sub/nested.txt": "nested\n"})
    for name in ["first", "second"]:
        urc.link_add(
            f"libs/{name}",
            make_repo(new_lore_repo, {"data.txt": "base\n"}).get_id(),
            "/",
        )
    urc.commit("Link two repositories")
    urc.push()
    for branch in ["side", "feature-branch"]:
        urc.branch_create(branch)
        urc.write_commit_push(branch, {f"libs/first/{branch}.txt": f"{branch}\n"})
        urc.branch_switch("main")
    urc.write_commit_push("Main", {"libs/first/main.txt": "main\n"})
    urc.link_add("libs/second/whole", nested.get_id(), "/")
    urc.commit("Main mounts a repository inside the second link")
    urc.push()
    urc.branch_merge_start("side", message="Merge side")
    merged = urc.branch_info().local_latest
    assert urc.revision_info(merged).merge
    feature = mount_part_inside_the_second_link_on_feature(urc, nested)

    start_the_merge_failing_at_the_second_link(urc)

    if finish == "abort":
        urc.branch_merge_abort()
        assert urc.branch_info().local_latest == merged
        assert parse_status_json(urc.status(json=True)) == []
        assert sorted(os.listdir(os.path.join(urc.path, "libs/first"))) == [
            "data.txt",
            "main.txt",
            "side.txt",
        ]
        with pytest.raises(NotInProgress):
            urc.branch_merge_abort()
        return
    unmount_the_part_inside_the_second_link(feature)
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    parents = urc.revision_info().merge.split()
    assert parents == [merged, feature.branch_info().local_latest]
    urc.push()
    assert_files_hold(
        urc.clone(),
        {
            "libs/first/feature-branch.txt": "feature-branch\n",
            "libs/first/main.txt": "main\n",
            "libs/first/side.txt": "side\n",
        },
    )


@pytest.mark.smoke
@pytest.mark.parametrize(
    "how", ["removed-and-added-elsewhere", "link-in-case", "directory-in-case", "removed"]
)
def test_link_merge_all_of_a_branch_removing_or_moving_a_nested_link(new_lore_repo, how):
    """A link nested in a link, which the merged branch removes and adds elsewhere, renames in
    case, or carries along with a directory it renames in case, is merged where it ends up, as a
    link of the main repository is. One it removes while this branch changed it is removed, as
    a removal inside a linked repository still stands."""
    if how == "removed":
        urc, _ = merge_a_branch_removing_a_link(
            new_lore_repo, added=False, pinned=False, outer="outer"
        )
        urc.push()
        for repo in [urc, urc.clone()]:
            assert set(registered_links(repo)) == {"outer"}
            assert mount_listing(repo, "outer/libs") == []
        return
    if how in LINK_MOVES:
        moved, destination, exposed = LINK_MOVES[how]
        urc, link_repo = merge_a_branch_moving_a_link(
            new_lore_repo, pinned=False, moved=moved, outer="outer"
        )
    else:
        urc, link_repo = merge_a_branch_removing_a_link(
            new_lore_repo, added=True, pinned=False, outer="outer"
        )
        destination = "libs/new"
        exposed = ["feature.txt", "linked.txt", "main.txt", "shared.txt"]
    urc.push()

    assert registered_links(urc)[f"outer/{destination}"] == link_repo.get_id()
    for repo in [urc, urc.clone()]:
        assert mount_listing(repo, f"outer/{destination}") == exposed


@pytest.mark.smoke
@pytest.mark.parametrize(
    "side, link, file_conflict",
    [
        ("mine", None, False),
        ("theirs", None, False),
        ("mine", "outer", False),
        (None, None, True),
    ],
    ids=["mine", "theirs", "link", "with-a-file-conflict"],
)
def test_link_merge_all_pin_conflict_in_a_nested_link(
    new_lore_repo, side, link, file_conflict
):
    """A pin both branches moved, of a fixed link nested in a link, is listed as a link conflict
    and refuses a commit, of every link or of the outer one, while the main repository's merge
    lands. Resolved to either side, or with a conflict in the outer link in one plain resolve,
    the merge commits and pushes that side's pin and the resolution."""
    urc, _ = make_parent_with_link(new_lore_repo, "outer", {"b.txt": "b\n"})
    fixed = make_repo(new_lore_repo, {"f.txt": "one\n"})
    nested = "outer/linked/fixed"
    urc.link_add(nested, fixed.get_id(), "/", disable_branching=True)
    urc.commit("Add a fixed link inside the link")
    urc.push()
    pins = {}
    for content in ("two", "three"):
        fixed.write_commit_push(content, {"f.txt": f"{content}\n"})
        pins[content] = fixed.branch_info().local_latest
    urc.branch_create("feature")
    urc.link_update(nested, pin=pins["three"])
    urc.write_commit_push(
        "Pin the nested link to three",
        {"main-feature.txt": "feature\n", "outer/b.txt": "feature\n"},
    )
    urc.branch_switch("main")
    urc.link_update(nested, pin=pins["two"])
    urc.write_commit_push(
        "Pin the nested link to two", {"outer/b.txt": "main\n"} if file_conflict else {}
    )

    output = urc.branch_merge_start("feature", message="Merge feature", link=link)
    assert f"Link conflict: {nested}" in output
    assert f"Link conflict: {nested}" in urc.branch_merge_conflicts_list()
    with pytest.raises(UnresolvedConflictError):
        urc.commit("Merge feature", link=link)
    with pytest.raises(UnresolvedConflictError):
        urc.commit("Merge feature", link=nested)

    if file_conflict:
        urc.write_files({"outer/b.txt": "resolved\n"})
        urc.branch_merge_resolve()
    else:
        getattr(urc, f"branch_merge_resolve_{side}")(nested)
    urc.commit("Merge feature")
    urc.push()

    expected = {
        nested + "/f.txt": "three\n" if side == "theirs" else "two\n",
        "outer/b.txt": "resolved\n" if file_conflict else "feature\n",
    }
    if not link and not file_conflict:
        expected["main-feature.txt"] = "feature\n"
    for repo in [urc, urc.clone()]:
        assert_files_hold(repo, expected)


def diverge_one_repository_two_links_reach(urc, shape: str, a_conflict: bool = False):
    """Changes the repository both links of `shape` reach on `feature` and on main, and `a.txt` on
    both where `a_conflict`, leaving main checked out."""
    urc.branch_create("feature")
    change_through_both_links(urc, shape, {"d.txt": "feature\n", "feature.txt": "feature\n"})
    if a_conflict:
        urc.write_commit_push("Feature changes a", {"a.txt": "feature\n"})
    urc.branch_switch("main")
    change_through_both_links(urc, shape, {"main.txt": "main\n"})
    if a_conflict:
        urc.write_commit_push("Main changes a", {"a.txt": "main\n"})


@pytest.mark.smoke
@pytest.mark.parametrize("shape", LINKS_TO_ONE_REPOSITORY)
def test_link_merge_all_merges_a_repository_two_links_reach_once(new_lore_repo, shape):
    """A repository two links reach at one pin is merged once: its main takes one merge revision,
    which both links pin and both mounts hold, in a clone too, and a second merge leaves it as it
    is."""
    urc, d, _ = links_to_one_repository(new_lore_repo, shape)
    first, second = LINKS_TO_ONE_REPOSITORY[shape]
    diverge_one_repository_two_links_reach(urc, shape)
    d_main = d.branch_info("main").remote_latest

    urc.branch_merge_start("feature", message="Merge feature")
    urc.push()

    d.branch_switch("main")
    merged = d.revision_info()
    assert merged.merge.split()[0] == d_main
    assert [link_pin_at(urc, first), link_pin_at(urc, second)] == [merged.signature] * 2
    urc.repository_verify()
    merged_files = {"d.txt": "feature\n", "feature.txt": "feature\n", "main.txt": "main\n"}
    holds = {
        first: merged_files,
        second: {"d.txt": "base\n"} if shape == "subtrees" else merged_files,
    }
    for repo in [urc, urc.clone()]:
        for mount, files in holds.items():
            assert_link_holds(repo, mount, files)
    urc.branch_merge_start("feature", message="Merge feature again")
    assert d.branch_info("main").remote_latest == merged.signature
    assert [link_pin_at(urc, first), link_pin_at(urc, second)] == [merged.signature] * 2


@pytest.mark.smoke
def test_link_merge_abort_of_a_repository_two_links_reach(new_lore_repo):
    """An abort of a merge that pins two links at one merge of their repository restores both
    links' pins and mounts."""
    shape = "top-level"
    urc, _, _ = links_to_one_repository(new_lore_repo, shape)
    mounts = LINKS_TO_ONE_REPOSITORY[shape]
    diverge_one_repository_two_links_reach(urc, shape, a_conflict=True)
    pins = [link_pin_at(urc, mount) for mount in mounts]

    urc.branch_merge_start("feature", message="Merge feature")
    assert urc.unresolved_paths() == {"a.txt"}
    staged = {link_pin_at(urc, mount) for mount in mounts}
    assert len(staged) == 1 and staged.isdisjoint(pins)
    urc.branch_merge_abort()

    assert [link_pin_at(urc, mount) for mount in mounts] == pins
    assert parse_status_json(urc.status(json=True)) == []
    for mount in mounts:
        assert_link_holds(urc, mount, {"d.txt": "base\n", "main.txt": "main\n"})


@pytest.mark.smoke
@pytest.mark.parametrize(
    "shape, refused",
    [
        ("nested", "pins"),
        ("nested", "conflict"),
        ("subtrees", "conflict"),
    ],
)
def test_link_merge_all_refuses_a_repository_two_links_reach_unless_it_merges_alike(
    new_lore_repo, shape, refused
):
    """A merge of a repository two links reach at different pins, or whose merge conflicts, inside
    one mount or in each, is refused before anything is written: nothing is staged and no branch
    moves."""
    urc, d, holders = links_to_one_repository(new_lore_repo, shape)
    first, _ = LINKS_TO_ONE_REPOSITORY[shape]
    urc.branch_create("feature")
    change_through_both_links(urc, shape, {"d.txt": "feature\n"})
    urc.branch_switch("main")
    if refused == "pins":
        urc.write_commit_push("Change d through one link", {f"{first}/main.txt": "main\n"})
    else:
        change_through_both_links(urc, shape, {"d.txt": "main\n"})
    repos = [urc, d, *holders]
    latests = [repo.branch_info(b).remote_latest for repo in repos for b in ("main", "feature")]

    error = ImproperArgumentsError if refused == "pins" else NotSupportedError
    with pytest.raises(error):
        urc.branch_merge_start("feature", message="Merge feature")

    assert parse_status_json(urc.status(json=True)) == []
    assert [
        repo.branch_info(b).remote_latest for repo in repos for b in ("main", "feature")
    ] == latests


@pytest.mark.smoke
def test_link_merge_all_of_a_repository_two_links_reach_takes_one_message_for_both(new_lore_repo):
    """The links pinning one merge of a repository take the same per-link message: a message at
    one of them alone is refused before the merge commits, and one given at both is the message
    of the repository's merge revision."""
    shape = "nested"
    urc, d, _ = links_to_one_repository(new_lore_repo, shape)
    first, second = LINKS_TO_ONE_REPOSITORY[shape]
    diverge_one_repository_two_links_reach(urc, shape)
    d_main = d.branch_info("main").remote_latest

    with pytest.raises(ImproperArgumentsError):
        urc.branch_merge_start(
            "feature", message="Merge feature", link_messages={first: "Merge d"}
        )
    assert d.branch_info("main").remote_latest == d_main

    urc.commit("Merge feature", link_messages={first: "Merge d", second: "Merge d"})
    urc.push()
    d.branch_switch("main")
    assert d.revision_info().message == "Merge d"


@pytest.mark.smoke
@pytest.mark.parametrize("shape", ["subtrees", "top-level"])
def test_link_commit_of_edits_under_two_links_to_one_repository_is_refused(
    new_lore_repo, shape
):
    """A commit of edits staged under two links to one repository, at one pin, is refused before
    it writes anything: both edits stay staged, and a push leaves every branch as it was."""
    urc, d, _ = links_to_one_repository(new_lore_repo, shape)
    first, second = LINKS_TO_ONE_REPOSITORY[shape]
    edits = {f"{first}/d.txt": "first\n", f"{second}/second.txt": "second\n"}
    urc.write_files(edits)
    urc.stage(list(edits))
    latests = [urc.branch_info().remote_latest, d.branch_info("main").remote_latest]

    with pytest.raises(NotSupportedError):
        urc.commit("Edit d through both links")

    assert set(edits) <= {entry["path"] for entry in parse_status_json(urc.status(json=True))}
    urc.push()
    assert [urc.branch_info().remote_latest, d.branch_info("main").remote_latest] == latests


@pytest.mark.smoke
def test_link_commit_of_one_link_to_a_repository_two_links_reach_is_refused(new_lore_repo):
    """A commit of one link alone, of two links that pin one merge of their repository, is
    refused, and the commit of the whole merge commits the repository once for both."""
    shape = "top-level"
    urc, d, _ = links_to_one_repository(new_lore_repo, shape)
    first, second = LINKS_TO_ONE_REPOSITORY[shape]
    diverge_one_repository_two_links_reach(urc, shape)
    d_main = d.branch_info("main").remote_latest
    urc.branch_merge_start("feature", message="Merge feature", no_commit=True)

    with pytest.raises(NotSupportedError):
        urc.commit("Merge d alone", link=first)
    assert d.branch_info("main").remote_latest == d_main

    urc.commit("Merge feature")
    urc.push()
    d.branch_switch("main")
    merged = d.revision_info()
    assert merged.merge.split()[0] == d_main
    assert [link_pin_at(urc, first), link_pin_at(urc, second)] == [merged.signature] * 2
