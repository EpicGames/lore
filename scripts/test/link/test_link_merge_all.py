# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT

import pytest
from error_types import ImproperArgumentsError
from link_helpers import (
    link_pin,
    make_parent_with_link,
    make_repo,
    setup_link_merge_conflict,
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
    """Abort --ignore-links strips merge metadata but preserves link pin updates."""
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
    urc.commit("Main branch changes")
    urc.push()

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
