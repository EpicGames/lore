# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
from link_helpers import setup_link_merge_conflict

from lore import Lore


def test_link_merge_specific(new_lore_repo):
    """Merge only a specific linked repository via --link."""
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create link repository with initial content
    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, add new files in both main and linked repos
    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature branch main repo addition\n"])

    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    # Switch back to main and add a different new file in main repo
    urc.branch_switch("main")

    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main branch only addition\n"])

    urc.stage(scan=True)
    urc.commit("Main branch addition")
    urc.push()

    # Merge only the specific linked repo (auto-commit)
    urc.branch_merge_start(
        "feature-branch",
        link=link_path,
        message="Merge feature-branch linked repo only",
    )
    urc.push()

    # Verify: linked repo additions from feature branch are applied
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature branch link repo file should be present after link-specific merge"
    )

    # Verify: main repo additions from feature branch are NOT present
    assert not urc.file_exists("feature-main-file.txt"), (
        "Feature branch main repo file should NOT be present after link-only merge"
    )

    # Verify link pin was updated via link list
    link_list_output = urc.link_list()
    assert link_repo.get_id() in link_list_output, (
        "Link should still be in the link list after merge"
    )

    # Verify post-merge state is clean
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after merge commit - Got:\n{status}"
    )


def test_link_merge_abort_specific(new_lore_repo):
    """Abort only a specific linked repository merge via --link."""
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create link repository with initial content
    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, add a new file in the linked repo
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch link addition")
    urc.push()

    # Switch back to main
    urc.branch_switch("main")

    # Merge only the linked repo with no_commit
    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Verify the linked repo file is present after merge
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should be present after merge"
    )

    # Abort the linked repo merge
    urc.branch_merge_abort(link=link_path)

    # Verify the linked repo file is rolled back
    assert not urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should not exist after link-specific abort"
    )

    # Verify main repo file is still present
    assert urc.file_exists("main-file.txt"), (
        "Main repo file should still exist after abort"
    )


def test_link_merge_preserves_tracked_branch(new_lore_repo):
    """After merge --link, the link's tracked branch is preserved (not overwritten by source)."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Snapshot the link list before merge
    link_list_before = repo.link_list()

    # Create feature branch and add content
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Feature branch addition")
    repo.push()

    repo.branch_switch("main")

    # Merge only the linked repo
    repo.branch_merge_start("feature-branch", link=link_path, message="Link-only merge")
    repo.push()

    # Verify: link list still shows "main" as tracked branch, not "feature-branch"
    link_list_after = repo.link_list()
    assert "feature-branch" not in link_list_after, (
        f"Link should track 'main' branch after merge, not 'feature-branch'.\n"
        f"Before: {link_list_before}\nAfter: {link_list_after}"
    )
    assert "main" in link_list_after, (
        f"Link should still track 'main' branch after merge.\nGot: {link_list_after}"
    )


def test_link_merge_sequential(new_lore_repo):
    """Two sequential link merges from the same feature branch work correctly."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch and add first file
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/first.txt", "w+") as f:
        f.writelines(["first feature file\n"])
    repo.stage(scan=True)
    repo.commit("First feature commit")
    repo.push()

    # First link merge
    repo.branch_switch("main")
    repo.branch_merge_start(
        "feature-branch", link=link_path, message="First link merge"
    )
    repo.push()

    assert repo.file_exists(f"{link_path}/first.txt"), "First file should be present"

    # Add second file on feature branch
    repo.branch_switch("feature-branch")
    with repo.open_file(f"{link_path}/second.txt", "w+") as f:
        f.writelines(["second feature file\n"])
    repo.stage(scan=True)
    repo.commit("Second feature commit")
    repo.push()

    # Second link merge
    repo.branch_switch("main")
    repo.branch_merge_start(
        "feature-branch", link=link_path, message="Second link merge"
    )
    repo.push()

    assert repo.file_exists(f"{link_path}/first.txt"), (
        "First file should still be present after second merge"
    )
    assert repo.file_exists(f"{link_path}/second.txt"), (
        "Second file should be present after second merge"
    )

    status = repo.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after second merge - Got:\n{status}"
    )


def test_link_update_after_merge(new_lore_repo):
    """Link update works correctly after a link merge (tracked branch is intact)."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch, add content, merge the link
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Feature commit")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge_start("feature-branch", link=link_path, message="Link merge")
    repo.push()

    # Now push a new commit to the linked repo directly (on main branch)
    # Sync first since the link merge advanced the linked repo's branch
    link_repo.sync()
    with link_repo.open_file("direct-update.txt", "w+") as f:
        f.writelines(["direct update content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Direct update to linked repo")
    link_repo.push()

    # Link update should follow the link's tracked branch (main), not the feature branch
    repo.link_update(link_path)

    assert repo.file_exists(f"{link_path}/direct-update.txt"), (
        "Link update should pick up changes from the link's main branch"
    )

    repo.commit("Update link after merge")


def test_link_merge_abort_restores_link_state(new_lore_repo):
    """After merge --link abort, link list shows original branch and revision."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Snapshot link state before merge
    link_list_before = repo.link_list()

    # Create feature branch with linked content
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Feature commit")
    repo.push()

    # Start link merge with no_commit to leave it pending
    repo.branch_switch("main")
    repo.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Verify file is present during pending merge
    assert repo.file_exists(f"{link_path}/feature-file.txt"), (
        "Feature file should be present during pending merge"
    )

    # Abort the link merge
    repo.branch_merge_abort(link=link_path)

    # Verify file is rolled back
    assert not repo.file_exists(f"{link_path}/feature-file.txt"), (
        "Feature file should not exist after abort"
    )

    # Verify link state is restored to pre-merge state
    link_list_after = repo.link_list()
    assert "main" in link_list_after, (
        f"Link should still track 'main' branch after abort.\nGot: {link_list_after}"
    )

    # Verify no merge is in progress — status should not say "pending merge"
    status = repo.status()
    assert "pending merge" not in status.lower(), (
        f"No merge should be in progress after abort - Got:\n{status}"
    )


def test_link_merge_abort_preserves_parent_staged_state(new_lore_repo):
    """Aborting a link merge must not destroy pre-existing staged changes in the parent repo."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch with linked content
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Feature commit")
    repo.push()

    # Switch back to main and stage a parent-level change BEFORE the merge
    repo.branch_switch("main")
    with repo.open_file("parent-staged.txt", "w+") as f:
        f.writelines(["staged parent content\n"])
    repo.stage(scan=True)

    # Verify parent file is staged
    status_before = repo.status()
    assert "parent-staged.txt" in status_before, (
        f"Parent file should be staged before merge - Got:\n{status_before}"
    )

    # Start link merge with no_commit
    repo.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Abort the link merge
    repo.branch_merge_abort(link=link_path)

    # The parent's staged change must survive the abort
    assert repo.file_exists("parent-staged.txt"), (
        "Parent staged file should still exist on disk after abort"
    )
    status_after = repo.status()
    assert "parent-staged.txt" in status_after, (
        f"Parent file should still be staged after link merge abort - Got:\n{status_after}"
    )

    # The merge state should be cleared
    assert "pending merge" not in status_after.lower(), (
        f"No merge should be in progress after abort - Got:\n{status_after}"
    )

    # Should be able to commit the parent change normally
    repo.commit("Commit parent staged change after abort")
    repo.push()

    assert repo.file_exists("parent-staged.txt"), (
        "Parent file should be present after commit"
    )


def test_link_merge_file_conflict_resolve(new_lore_repo):
    """File conflict in linked repo is resolvable from the main repo."""
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create link repository with a file that will be conflicted
    link_repo = new_lore_repo()

    with link_repo.open_file("shared-data.txt", "w+") as f:
        f.writelines(["base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, modify the shared file through the main repo's mount path
    with urc.open_file(f"{link_path}/shared-data.txt", "w+") as f:
        f.writelines(["feature branch content\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch modifies shared data")
    urc.push()

    # Switch to main and modify the same file differently through the mount path
    urc.branch_switch("main")

    with urc.open_file(f"{link_path}/shared-data.txt", "w+") as f:
        f.writelines(["main branch content\n"])

    urc.stage(scan=True)
    urc.commit("Main branch modifies shared data")
    urc.push()

    # Merge with --link — should produce conflicts, not fail
    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Verify the conflict file exists at the mount path
    conflict_file = f"{link_path}/shared-data.txt"
    assert urc.file_exists(conflict_file), "Conflicted file should exist at mount path"

    # Resolve the conflict by writing the desired content and marking as resolved
    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["manually resolved content\n"])

    urc.branch_merge_resolve(conflict_file)

    # Commit and push
    urc.commit("Merge with resolved conflict in linked repo")
    urc.push()

    # Verify post-merge state
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after merge commit - Got:\n{status}"
    )

    # Verify the resolved content is present
    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "manually resolved content" in content, (
        f"File should have manually resolved content - Got: {content}"
    )


def test_link_merge_file_conflict_in_subdirectory(new_lore_repo):
    """File conflict in a subdirectory of a linked repo."""
    urc, _link_repo, link_path = setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {
                "path": "src/module.rs",
                "base": "base\n",
                "mine": "mine content\n",
                "theirs": "theirs content\n",
            }
        ],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    conflict_file = f"{link_path}/src/module.rs"
    assert urc.file_exists(conflict_file), "Conflict file should exist at mount path"

    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["resolved subdirectory content\n"])

    urc.branch_merge_resolve(conflict_file)
    urc.commit("Merge with resolved subdirectory conflict")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        assert "resolved subdirectory content" in f.read()


def test_link_merge_file_conflict_in_nested_subdirectory(new_lore_repo):
    """File conflict in a deeply nested subdirectory of a linked repo."""
    urc, _link_repo, link_path = setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {
                "path": "src/core/engine/config.txt",
                "base": "base config\n",
                "mine": "mine config\n",
                "theirs": "theirs config\n",
            }
        ],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    conflict_file = f"{link_path}/src/core/engine/config.txt"
    assert urc.file_exists(conflict_file), "Deeply nested conflict file should exist"

    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["resolved deep config\n"])

    urc.branch_merge_resolve(conflict_file)
    urc.commit("Merge with resolved deep nested conflict")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        assert "resolved deep config" in f.read()


def test_link_merge_multiple_file_conflicts_across_directories(new_lore_repo):
    """Multiple file conflicts at different depths, resolved independently."""
    urc, _link_repo, link_path = setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {
                "path": "readme.txt",
                "base": "base readme\n",
                "mine": "mine readme\n",
                "theirs": "theirs readme\n",
            },
            {
                "path": "src/lib.rs",
                "base": "base lib\n",
                "mine": "mine lib\n",
                "theirs": "theirs lib\n",
            },
            {
                "path": "src/util/helpers.rs",
                "base": "base helpers\n",
                "mine": "mine helpers\n",
                "theirs": "theirs helpers\n",
            },
        ],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # All three conflict files should exist
    f1 = f"{link_path}/readme.txt"
    f2 = f"{link_path}/src/lib.rs"
    f3 = f"{link_path}/src/util/helpers.rs"
    assert urc.file_exists(f1), "Root conflict file should exist"
    assert urc.file_exists(f2), "Subdirectory conflict file should exist"
    assert urc.file_exists(f3), "Nested subdirectory conflict file should exist"

    # Resolve each with different content
    with urc.open_file(f1, "w+") as f:
        f.writelines(["resolved readme\n"])
    with urc.open_file(f2, "w+") as f:
        f.writelines(["resolved lib\n"])
    with urc.open_file(f3, "w+") as f:
        f.writelines(["resolved helpers\n"])

    urc.branch_merge_resolve([f1, f2, f3])
    urc.commit("Merge with multiple resolved conflicts")
    urc.push()

    with urc.open_file(f1, "r") as f:
        assert "resolved readme" in f.read()
    with urc.open_file(f2, "r") as f:
        assert "resolved lib" in f.read()
    with urc.open_file(f3, "r") as f:
        assert "resolved helpers" in f.read()


def test_link_merge_directory_level_resolve(new_lore_repo):
    """Resolve multiple conflicts by specifying the directory path."""
    urc, _link_repo, link_path = setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {
                "path": "src/a.txt",
                "base": "base a\n",
                "mine": "mine a\n",
                "theirs": "theirs a\n",
            },
            {
                "path": "src/b.txt",
                "base": "base b\n",
                "mine": "mine b\n",
                "theirs": "theirs b\n",
            },
        ],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Manually resolve both files
    with urc.open_file(f"{link_path}/src/a.txt", "w+") as f:
        f.writelines(["resolved a\n"])
    with urc.open_file(f"{link_path}/src/b.txt", "w+") as f:
        f.writelines(["resolved b\n"])

    # Resolve by directory path
    urc.branch_merge_resolve(f"{link_path}/src")
    urc.commit("Merge with directory-level resolve")
    urc.push()

    with urc.open_file(f"{link_path}/src/a.txt", "r") as f:
        assert "resolved a" in f.read()
    with urc.open_file(f"{link_path}/src/b.txt", "r") as f:
        assert "resolved b" in f.read()


def test_link_merge_delete_vs_modify_in_link(new_lore_repo):
    """Delete-vs-modify file conflict inside a linked repo. Feature branch deletes
    the file; main branch modifies it. The default merge must surface the conflict
    in a recoverable way: either the file remains on disk with conflict markers,
    or `.mine` / `.theirs` / `.base` sidecars are present. The user must not see
    the file silently vanish."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("doomed.txt", "w+") as f:
        f.writelines(["link base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Feature branch: delete the link file via mount path
    urc.branch_create("feature-branch")
    urc.remove_file(f"{link_path}/doomed.txt")
    urc.stage(scan=True)
    urc.commit("Feature branch deletes doomed.txt")
    urc.push()

    # Main branch: modify the same file via mount path
    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/doomed.txt", "w+") as f:
        f.writelines(["main modified\n"])
    urc.stage(scan=True)
    urc.commit("Main branch modifies doomed.txt")
    urc.push()

    # Default merge — must report the conflict, not auto-commit
    urc.branch_merge_start(
        "feature-branch", message="Merge feature-branch", no_commit=True
    )

    # Either the file is on disk with markers OR sidecars exist. Either is
    # acceptable; silent disappearance is not.
    file_path = f"{link_path}/doomed.txt"
    mine_sidecar = f"{file_path}.mine"
    theirs_sidecar = f"{file_path}.theirs"
    base_sidecar = f"{file_path}.base"
    has_file = urc.file_exists(file_path)
    has_any_sidecar = (
        urc.file_exists(mine_sidecar)
        or urc.file_exists(theirs_sidecar)
        or urc.file_exists(base_sidecar)
    )
    assert has_file or has_any_sidecar, (
        f"Delete-vs-modify must leave recoverable artifacts in link mount; "
        f"found neither {file_path} nor sidecars."
    )

    # Resolving via "mine" (the modify side) restores the file on disk.
    urc.branch_merge_resolve_mine(file_path)
    assert urc.file_exists(file_path), (
        "After resolve_mine, the modified version should be on disk."
    )

    urc.commit("Merge with delete-vs-modify resolved as mine")
    urc.push()


def test_link_merge_mixed_conflict_and_clean(new_lore_repo):
    """Linked repo merge with both conflicting and cleanly merged files."""
    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo content\n"])
    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("conflict.txt", "w+") as f:
        f.writelines(["base conflict\n"])
    with link_repo.open_file("clean.txt", "w+") as f:
        f.writelines(["base clean\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")

    # Feature branch: modify both files, and add a new file
    with urc.open_file(f"{link_path}/conflict.txt", "w+") as f:
        f.writelines(["theirs conflict\n"])
    with urc.open_file(f"{link_path}/clean.txt", "w+") as f:
        f.writelines(["clean modified by feature\n"])
    with urc.open_file(f"{link_path}/new-feature-file.txt", "w+") as f:
        f.writelines(["new file from feature\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    # Main branch: only modify the conflicting file
    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/conflict.txt", "w+") as f:
        f.writelines(["mine conflict\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Merge
    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Clean file should be merged automatically
    with urc.open_file(f"{link_path}/clean.txt", "r") as f:
        content = f.read()
    assert "clean modified by feature" in content, (
        f"Clean file should have feature branch content after auto-merge - Got: {content}"
    )

    # New file from feature branch should be present
    assert urc.file_exists(f"{link_path}/new-feature-file.txt"), (
        "New file from feature branch should be present"
    )

    # Conflict file needs manual resolution
    with urc.open_file(f"{link_path}/conflict.txt", "w+") as f:
        f.writelines(["resolved conflict\n"])
    urc.branch_merge_resolve(f"{link_path}/conflict.txt")

    urc.commit("Merge with mixed conflict and clean changes")
    urc.push()

    # Verify all files present and correct
    with urc.open_file(f"{link_path}/conflict.txt", "r") as f:
        assert "resolved conflict" in f.read()
    with urc.open_file(f"{link_path}/clean.txt", "r") as f:
        assert "clean modified by feature" in f.read()
    assert urc.file_exists(f"{link_path}/new-feature-file.txt")


def test_link_merge_file_conflict_resolve_mine(new_lore_repo):
    """File conflict in linked repo resolved with mine."""
    urc, _link_repo, link_path = setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {
                "path": "data.txt",
                "base": "base content\n",
                "mine": "mine content\n",
                "theirs": "theirs content\n",
            }
        ],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    conflict_file = f"{link_path}/data.txt"
    assert urc.file_exists(conflict_file), "Conflict file should exist"

    urc.branch_merge_resolve_mine(conflict_file)

    urc.commit("Merge with mine resolution")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "mine content" in content, (
        f"File should have mine content after resolve mine - Got: {content}"
    )


def test_link_merge_file_conflict_resolve_theirs(new_lore_repo):
    """File conflict in linked repo resolved with theirs."""
    urc, _link_repo, link_path = setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {
                "path": "data.txt",
                "base": "base content\n",
                "mine": "mine content\n",
                "theirs": "theirs content\n",
            }
        ],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    conflict_file = f"{link_path}/data.txt"
    assert urc.file_exists(conflict_file), "Conflict file should exist"

    urc.branch_merge_resolve_theirs(conflict_file)

    urc.commit("Merge with theirs resolution")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "theirs content" in content, (
        f"File should have theirs content after resolve theirs - Got: {content}"
    )


def test_link_merge_into_specific(new_lore_repo):
    """Merge current linked repo branch into target branch via --link."""
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create link repository with initial content
    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, add a file in the linked repo through mount path
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch link addition")
    urc.push()

    # Merge the feature branch's linked repo into main via merge_into --link.
    # This merges the linked repo's feature branch into its main branch on the remote,
    # then updates the main repo's link pin on the feature branch.
    urc.branch_merge_into("main", "Merge feature linked repo into main", link=link_path)

    # Verify we're still on feature branch with the file present
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature branch link file should still be present"
    )
    assert urc.file_exists("main-file.txt"), "Main repo file should still exist"


def test_link_merge_into_scope_isolation(new_lore_repo):
    """merge into --link only merges linked repo changes, not main repo changes."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch with changes in BOTH main repo and linked repo
    repo.branch_create("feature-branch")

    with repo.open_file("feature-main-only.txt", "w+") as f:
        f.writelines(["feature main content\n"])
    with repo.open_file(f"{link_path}/feature-link-only.txt", "w+") as f:
        f.writelines(["feature link content\n"])

    repo.stage(scan=True)
    repo.commit("Feature branch additions in both repos")
    repo.push()

    # Merge into main scoped to link only
    repo.branch_merge_into("main", "Merge only linked repo into main", link=link_path)

    # Switch to main and sync to see what landed
    repo.branch_switch("main")
    repo.sync()

    # Linked repo file should be present on main
    assert repo.file_exists(f"{link_path}/feature-link-only.txt"), (
        "Link file from feature branch should be on main after merge into --link"
    )

    # Main repo feature file should NOT be on main
    assert not repo.file_exists("feature-main-only.txt"), (
        "Main repo file from feature branch should NOT be on main after link-only merge into"
    )


def test_link_merge_into_sequential(new_lore_repo):
    """Two sequential merge into --link operations from the same feature branch."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch, add first link file
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/first.txt", "w+") as f:
        f.writelines(["first feature file\n"])
    repo.stage(scan=True)
    repo.commit("First feature commit")
    repo.push()

    # First merge into main
    repo.branch_merge_into("main", "First link merge into main", link=link_path)

    # Sync and merge main into feature branch (main advanced from the merge_into)
    repo.sync()
    repo.branch_merge_start("main", message="Merge main into feature")
    repo.push()

    # Add second link file on feature branch
    with repo.open_file(f"{link_path}/second.txt", "w+") as f:
        f.writelines(["second feature file\n"])
    repo.stage(scan=True)
    repo.commit("Second feature commit")
    repo.push()

    # Second merge into main
    repo.branch_merge_into("main", "Second link merge into main", link=link_path)

    # Verify both files landed on main
    repo.branch_switch("main")
    repo.sync()

    assert repo.file_exists(f"{link_path}/first.txt"), (
        "First file should be on main after sequential merge into"
    )
    assert repo.file_exists(f"{link_path}/second.txt"), (
        "Second file should be on main after sequential merge into"
    )
