# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os

import pytest
from link_helpers import make_repo
from test_utils import unstaged_entries, working_tree_files


def _build_nested_link_repos_at(new_lore_repo, b_mount, c_inner_mount):
    """Build A -> B -> C with configurable mount paths.

    A links B at `b_mount`; C is linked at `<b_mount>/<c_inner_mount>` (i.e.
    `c_inner_mount` is C's path *relative to B's root*). Returns
    (repo_a, repo_b, repo_c, b_mount, nested_mount).

    Lets tests control the relative depths of the outer and inner mounts, which
    matters for the tracker's inner-first ordering.
    """

    repo_c = make_repo(
        new_lore_repo, {"c-data/inner.txt": "content from repository C\n"}, "C initial"
    )
    repo_b = make_repo(
        new_lore_repo, {"b-root.txt": "content from repository B\n"}, "B initial"
    )
    repo_a = make_repo(
        new_lore_repo, {"a-root.txt": "content from repository A\n"}, "A initial"
    )

    repo_a.link_add(b_mount, repo_b.get_id(), "/")
    repo_a.commit("A links B")
    repo_a.push()

    nested_mount = f"{b_mount}/{c_inner_mount}"
    repo_a.link_add(nested_mount, repo_c.get_id(), "/")
    repo_a.commit("A links B links C")
    repo_a.push()

    return repo_a, repo_b, repo_c, b_mount, nested_mount


@pytest.mark.smoke
def test_link_scan_keeps_a_nested_mount_missing_from_the_working_tree(new_lore_repo):
    """A scan crossing into a linked repository keeps the link that one holds."""
    repo_a, _repo_b, _repo_c, b_mount, _nested = _build_nested_link_repos_at(
        new_lore_repo, "b", "sub/c"
    )
    sibling = f"{b_mount}/sub/notes.txt"
    repo_a.write_files({sibling: "sibling\n"})
    repo_a.stage(sibling)
    repo_a.commit("Add a file beside the nested mount")
    repo_a.push()

    clone = repo_a.clone()
    clone.rmtree(f"{b_mount}/sub")

    clone.status(scan=True)
    scanned = [entry["path"] for entry in unstaged_entries(clone)]
    assert scanned == [sibling], (
        f"A scan reports the deletion beside the nested mount, got {scanned}"
    )

    clone.stage(scan=True)
    clone.commit("Delete the file beside the nested mount")
    clone.push()

    verify = repo_a.clone()
    assert working_tree_files(verify) == [
        "a-root.txt",
        f"{b_mount}/b-root.txt",
        f"{b_mount}/sub/c/c-data/inner.txt",
    ], f"The branch keeps the nested link mounted, got {working_tree_files(verify)}"


@pytest.mark.smoke
def test_link_info_reports_nested_link_with_full_path(new_lore_repo):
    """`lore link info` on a link inside another link reports the mount path
    relative to this repository, not to the intermediate linked repository.
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "libs/vendor/b", "c"
    )

    output = repo_a.link_info(nested_mount)

    assert f"Link path: {nested_mount}" in output, (
        f"link info must report a nested link's full mount path.\nOutput:\n{output}"
    )
    assert repo_c.get_id() in output, (
        f"link info must report the innermost linked repository.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_nested_link_probe(new_lore_repo):
    """Add a nested link (a link whose path is inside another link).

    NOT layers. This is A -> B -> C: repo A links repo B at some path, and a
    link to repo C is added *inside* B's mounted subtree. Verifies C's content
    clones in, C appears in the link list, and the nesting survives a commit +
    fresh clone round-trip.
    """
    # --- Repo C: the innermost target -------------------------------------
    repo_c = new_lore_repo()
    c_file = "c-data/inner.txt"
    repo_c.make_dirs(os.path.dirname(c_file))
    with repo_c.open_file(c_file, "w+") as f:
        f.writelines(["content from repository C\n"])
    repo_c.stage(scan=True)
    repo_c.commit("C initial")
    repo_c.push()

    # --- Repo B: the middle repo that A will link ------------------------
    repo_b = new_lore_repo()
    b_file = "b-root.txt"
    b_subdir_file = "b-dir/nested.txt"
    with repo_b.open_file(b_file, "w+") as f:
        f.writelines(["content from repository B\n"])
    repo_b.make_dirs(os.path.dirname(b_subdir_file))
    with repo_b.open_file(b_subdir_file, "w+") as f:
        f.writelines(["nested content from repository B\n"])
    repo_b.stage(scan=True)
    repo_b.commit("B initial")
    repo_b.push()

    # --- Repo A: the outermost parent ------------------------------------
    repo_a = new_lore_repo()
    a_file = "a-root.txt"
    with repo_a.open_file(a_file, "w+") as f:
        f.writelines(["content from repository A\n"])
    repo_a.stage(scan=True)
    repo_a.commit("A initial")
    repo_a.push()

    # Step 1: link B into A. This is an ordinary (non-nested) link and must
    # succeed — establishes the mount we then try to nest inside.
    b_mount = "vendor/b"
    repo_a.link_add(b_mount, repo_b.get_id(), "/")

    b_root_via_a = f"{b_mount}/b-root.txt"
    b_nested_via_a = f"{b_mount}/b-dir/nested.txt"
    assert repo_a.compare_file(repo_a, b_root_via_a), (
        "B's root file should be accessible through A's link"
    )
    assert repo_a.compare_file(repo_a, b_nested_via_a), (
        "B's nested file should be accessible through A's link"
    )

    repo_a.commit("A links B")
    repo_a.push()

    # Step 2: resolve/read machinery through the link works — confirm A can
    # still walk B's subtree via link list and status without error.
    link_list = repo_a.link_list()
    assert repo_b.get_id() in link_list, "B should appear in A's link list"
    assert b_mount in link_list, "B's mount path should appear in A's link list"

    # Step 3: the nested link — add a link to C at a path inside B's mounted
    # subtree (vendor/b/vendor/c). `link add` resolves through B's link, stages
    # the sub-link in B's state and registry, and folds B's new revision up
    # into A's pin.
    nested_mount = f"{b_mount}/vendor/c"
    expect_c_via_nested = f"{nested_mount}/c-data/inner.txt"

    repo_a.link_add(nested_mount, repo_c.get_id(), "/")

    # C's content must materialize under the nested mount.
    assert repo_a.file_exists(expect_c_via_nested), (
        f"Nested linked file did not clone in at {expect_c_via_nested}"
    )
    assert repo_a.compare_file(repo_a, expect_c_via_nested), (
        "Nested linked file content mismatch"
    )

    # C should appear in the link registry alongside B.
    nested_link_list = repo_a.link_list()
    assert repo_c.get_id() in nested_link_list, (
        "C should appear in the link list once nested-linked"
    )
    assert nested_mount in nested_link_list, (
        "Nested mount path should appear in the link list"
    )

    # And it must survive a round-trip through commit + fresh clone, which
    # exercises the commit-recursion and server tree-walk nesting paths (both
    # already nesting-capable).
    repo_a.commit("A links B links C")
    repo_a.push()

    fresh = repo_a.clone()
    assert fresh.file_exists(expect_c_via_nested), (
        "Nested linked file did not survive commit + clone round-trip"
    )
    assert fresh.compare_file(repo_a, expect_c_via_nested), (
        "Nested linked file content mismatch after clone"
    )


@pytest.mark.smoke
def test_nested_link_stage_unstage_reset(new_lore_repo):
    """Stage / unstage / reset of content INSIDE a nested link (A -> B -> C).

    Exercises that the stage machinery propagates a change made two link
    levels deep (a file owned by C, mounted under B, mounted under A) back up
    through B's pin to A's staged anchor, and that unstage/reset undo it.
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    new_inner_file = f"{nested_mount}/c-data/added.txt"

    # --- stage a modification two levels deep -----------------------------
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["modified content two levels deep\n"])
    with repo_a.open_file(new_inner_file, "w+") as f:
        f.writelines(["a brand new file inside C\n"])

    repo_a.stage(scan=True)

    status = repo_a.status()
    assert "Changes staged for commit" in status, "Deep change should stage"
    assert "M " + inner_file in status, "Modified inner file should be staged"
    assert "A " + new_inner_file in status, "Added inner file should be staged"

    # --- unstage the addition, keep the modification ----------------------
    repo_a.unstage(new_inner_file)
    status_after_unstage = repo_a.status()

    # The unstaged addition must drop out of the staged section (it becomes an
    # untracked file), while the modification stays staged.
    staged_section = status_after_unstage.split("Untracked files:")[0]
    assert "A " + new_inner_file not in staged_section, (
        "Unstaged addition should no longer be in the staged section"
    )
    assert "M " + inner_file in staged_section, (
        "Modification should remain staged after unstaging the addition"
    )
    assert new_inner_file in status_after_unstage, (
        "Unstaged addition should still show as an untracked file"
    )

    # --- commit the modification, then reset a fresh deep change ----------
    repo_a.stage(scan=True)
    repo_a.commit("Modify inner file two levels deep")
    repo_a.push()

    assert repo_a.compare_file(repo_a, inner_file), (
        "Committed deep modification should match on disk"
    )

    # A fresh clone must reproduce the committed deep modification.
    fresh = repo_a.clone()
    assert fresh.compare_file(repo_a, inner_file), (
        "Deep modification should survive commit + clone"
    )

    # reset a new deep modification back to committed content
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["TEMP deep modification to be reset\n"])
    repo_a.reset(inner_file)
    with repo_a.open_file(inner_file, "r") as f:
        content = f.read()
    assert "TEMP deep modification" not in content, "reset should discard deep change"
    assert "modified content two levels deep" in content, (
        "reset should restore committed deep content"
    )


@pytest.mark.smoke
def test_nested_link_update(new_lore_repo):
    """`link update` re-pins a NESTED link (the B -> C link) to a new revision.

    The re-pin and its registry write must target B's registry, not A's, and
    the new B revision must propagate up to A's pin.
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    # Advance C with a new revision the nested link can be re-pinned to.
    clone_c = repo_c.clone()
    with clone_c.open_file("c-data/inner.txt", "w+") as f:
        f.writelines(["C revision two\n"])
    with clone_c.open_file("c-v2-only.txt", "w+") as f:
        f.writelines(["only present in C v2\n"])
    clone_c.stage(scan=True)
    clone_c.commit("C v2")
    clone_c.push()
    c_v2 = clone_c.branch_info().local_latest

    # Re-pin the nested link to C's v2.
    output = repo_a.link_update(nested_mount, pin=c_v2)
    assert "updated" in output.lower(), "Nested link update should succeed"

    v2_only = f"{nested_mount}/c-v2-only.txt"
    assert repo_a.file_exists(v2_only), "C v2-only file should appear after re-pin"

    link_list = repo_a.link_list()
    assert c_v2 in link_list, "Nested link should show the new C v2 pin"

    repo_a.commit("Re-pin nested link to C v2")
    repo_a.push()

    fresh = repo_a.clone()
    assert fresh.file_exists(v2_only), "Re-pinned nested content should survive clone"


@pytest.mark.smoke
def test_nested_link_remove(new_lore_repo):
    """`link remove` removes a NESTED link (B -> C) and its registry entry.

    Removal must delete C's registry entry from B's registry, drop C's content
    from the mount, and propagate the new B revision up to A.
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    assert repo_a.file_exists(inner_file), "Precondition: nested content present"

    output = repo_a.link_remove(nested_mount)
    assert "Removed link" in output, "Nested link removal should succeed"

    assert not repo_a.file_exists(inner_file), (
        "Nested linked content should be gone after removal"
    )

    link_list = repo_a.link_list()
    assert repo_c.get_id() not in link_list, (
        "C should no longer appear in link list after nested removal"
    )

    repo_a.commit("Remove nested link to C")
    repo_a.push()

    # Removal must sync to a fresh clone.
    fresh = repo_a.clone()
    assert not fresh.file_exists(inner_file), (
        "Nested link removal should propagate to a fresh clone"
    )
    fresh_link_list = fresh.link_list()
    assert repo_c.get_id() not in fresh_link_list, (
        "C should not appear in a fresh clone's link list after removal"
    )


@pytest.mark.smoke
def test_nested_link_list_shows_nesting(new_lore_repo):
    """`link list` enumerates nested links with their full A-relative paths.

    Both the B link (top-level) and the C link (nested in B) must appear, each
    with the correct full mount path.
    """
    repo_a, repo_b, repo_c, b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    output = repo_a.link_list()

    assert repo_b.get_id() in output, "B (top-level link) should be listed"
    assert repo_c.get_id() in output, "C (nested link) should be listed"
    assert b_mount in output, "B's mount path should be listed"
    assert nested_mount in output, (
        "C's full nested mount path (parent mount + inner path) should be listed"
    )

    # A fresh clone should list the same nesting.
    fresh = repo_a.clone()
    fresh_output = fresh.link_list()
    assert repo_c.get_id() in fresh_output, (
        "Nested link should be listed in a fresh clone too"
    )
    assert nested_mount in fresh_output, (
        "Nested mount path should be listed in a fresh clone too"
    )


@pytest.mark.smoke
def test_nested_link_unstage_deep_outer_shallow_inner(new_lore_repo):
    """Nested unstage when the OUTER mount is deeper than the INNER mount.

    Regression test for the tracker's inner-first ordering: B is mounted deep
    (`libs/vendor/b`, 2 slashes) and C is mounted at B's root (`c`, 0 slashes).
    A naive deepest-first sort keyed on the link's own mount-path slash count
    would order B (outer) before C (inner) and fold a stale B into A's pin.

    Modify a file two levels deep, stage it, then unstage it, and verify the
    change fully reverts (both the deep file and, after commit of a real deep
    change and a fresh clone, that the intermediate pins are consistent).
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "libs/vendor/b", "c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    added_file = f"{nested_mount}/c-data/added.txt"

    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["deep modification\n"])
    with repo_a.open_file(added_file, "w+") as f:
        f.writelines(["a new deep file\n"])

    repo_a.stage(scan=True)
    staged = repo_a.status()
    assert "M " + inner_file in staged, "Deep modification should stage"
    assert "A " + added_file in staged, "Deep addition should stage"

    # Unstage everything; both changes must leave the staged section.
    repo_a.unstage(".")
    after = repo_a.status()
    staged_section = after.split("Untracked files:")[0]
    assert "M " + inner_file not in staged_section, (
        "Deep modification should be unstaged (not folded via a stale pin)"
    )
    assert "A " + added_file not in staged_section, "Deep addition should be unstaged"

    # Now re-stage and commit a real deep change; a fresh clone must reproduce
    # it, proving the intermediate B pin was folded correctly (not stale).
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["committed deep modification\n"])
    repo_a.stage(scan=True)
    repo_a.commit("Modify inner file two levels deep")
    repo_a.push()

    fresh = repo_a.clone()
    assert fresh.compare_file(repo_a, inner_file), (
        "Deep modification should survive commit + clone (pins folded correctly)"
    )


@pytest.mark.smoke
def test_nested_link_list_staged_shows_nested(new_lore_repo):
    """`link list --staged` reports a nested link with staged content.

    Stage a change two levels deep, then `link list --staged` must include the
    nested C link (not only the top-level B link).
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["staged deep change\n"])
    repo_a.stage(scan=True)

    # `link list --staged` reports by mount path + staged file count (no repo
    # id). The nested link must appear with its full A-relative path.
    staged_list = repo_a.link_list(staged=True)
    assert nested_mount in staged_list, (
        "Nested link's full mount path should appear in link list --staged"
    )


@pytest.mark.smoke
def test_nested_link_commit_scoped(new_lore_repo):
    """`commit --link <nested-path>` commits only the nested link's changes.

    Scoped commit must resolve the nested path to the innermost repo (C),
    commit C's staged change, and fold the pin up to A.
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["scoped commit change\n"])
    repo_a.stage(scan=True)

    output = repo_a.commit("Scoped nested commit", link=nested_mount)
    assert "Commit succeeded" in output, (
        f"Scoped commit of a nested link should succeed.\nOutput:\n{output}"
    )

    # The scoped commit advances C (and the intermediate B) to real revisions
    # and stages the resulting pin change on the top-level parent. Committing
    # the parent records it on A's branch; the change then survives a clone.
    repo_a.commit("Record scoped nested commit on parent")
    repo_a.push()

    fresh = repo_a.clone()
    assert fresh.compare_file(repo_a, inner_file), (
        "Scoped-committed deep change should survive a fresh clone"
    )


@pytest.mark.smoke
def test_nested_link_stage_delete_deep(new_lore_repo):
    """Staging a DELETE of a file two levels deep works.

    Regression for the stage delete-fallback that used a top-level-only parent
    lookup. Delete a file owned by C (mounted under B under A), stage it, and
    verify it commits and disappears from a fresh clone.
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    deep_file = f"{nested_mount}/c-data/inner.txt"
    assert repo_a.file_exists(deep_file), "Precondition: deep file present"

    repo_a.remove_file(deep_file)
    output = repo_a.stage(scan=True)
    status = repo_a.status()
    assert "D " + deep_file in status, (
        f"Deep delete should be staged.\nStage output:\n{output}\nStatus:\n{status}"
    )

    repo_a.commit("Delete a file two levels deep")
    repo_a.push()

    fresh = repo_a.clone()
    assert not fresh.file_exists(deep_file), (
        "Deep delete should propagate to a fresh clone"
    )


@pytest.mark.smoke
def test_nested_link_reset_deep(new_lore_repo):
    """`reset` of a modified file two levels deep restores committed content."""
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    deep_file = f"{nested_mount}/c-data/inner.txt"
    with repo_a.open_file(deep_file, "r") as f:
        original = f.read()

    with repo_a.open_file(deep_file, "w+") as f:
        f.writelines(["temporary deep change to be reset\n"])

    repo_a.reset(deep_file)
    with repo_a.open_file(deep_file, "r") as f:
        content = f.read()
    assert "temporary deep change" not in content, "reset should discard deep change"
    assert content == original, "reset should restore committed deep content exactly"


@pytest.mark.smoke
def test_nested_link_commit_message(new_lore_repo):
    """`--link-message <nested-path> <msg>` applies to the nested link's commit.

    Per-link commit messages are keyed by the full mount path, so a nested
    link (C, at `vendor/b/vendor/c`) receives its own message rather than the
    main message. Verified by reading C's committed revision message.
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["change for per-link message test\n"])
    repo_a.stage(scan=True)

    nested_message = "Dedicated message for nested link C"
    repo_a.commit(
        "Main parent message",
        link_messages={nested_mount: nested_message},
    )
    repo_a.push()

    # C's latest committed revision must carry the nested message, not the main
    # message. Read it from a fresh clone of C.
    clone_c = repo_c.clone()
    c_message = clone_c.revision_metadata_get("message")
    assert nested_message in c_message, (
        f"Nested link commit should use its --link-message.\n"
        f"Expected: {nested_message!r}\nGot: {c_message!r}"
    )
    assert "Main parent message" not in c_message, (
        "Nested link commit should not fall back to the main message when a "
        "per-link message was supplied"
    )


@pytest.mark.smoke
def test_link_branch_archive_include_links_reaches_nested_link(new_lore_repo):
    """`--include-links` descends into a link nested inside another link.

    `branch create` only seeds the top level, so the nested repository holds no
    branch to remove; the cascade is observed reaching it through the debug log
    rather than through a branch that disappears.
    """
    repo_a, repo_b, repo_c, _b_mount, _nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    nested_before = sorted(repo_c.branch_list().remote_branches)

    repo_a.branch_create("feature")
    repo_a.push()
    assert repo_b.branch_list().has_remote_branch("feature"), (
        f"Expected create to cascade into the top-level link, got: {repo_b.branch_list()}"
    )

    repo_a.branch_switch("main")
    output = repo_a.branch_archive("feature", include_links=True, level="debug")

    assert repo_c.get_id() in output, (
        f"Expected the cascade to reach nested link {repo_c.get_id()}, got: {output}"
    )
    assert sorted(repo_c.branch_list().remote_branches) == nested_before, (
        f"Expected nested link branches {nested_before}, got: {repo_c.branch_list()}"
    )
