# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import re

import pytest
from error_types import BranchDivergedError, LinkDivergedError
from link_helpers import (
    DEFAULT_LINK_MOUNT,
    DEFAULT_PARENT_FILE,
    build_nested_link_repos_at,
    make_parent_with_link,
    make_repo,
    remote_branch_entries,
    remount,
)
from lore_parsers import parse_jsonl, parse_status_json

from lore import Lore

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)


@pytest.mark.smoke
def test_link_branching_and_pinning(new_lore_repo):
    """Test that branching and pinning are orthogonal concerns for link add.

    A branch is always created in the linked repo (using the parent repo's
    current branch) unless --disable-branching is specified. --pin only
    controls the starting revision, not whether a branch is created.

    Scenarios:
      Case A: no --pin, no --disable-branching  -> branch created, uses latest
      Case B: --pin, no --disable-branching      -> branch created, uses pinned revision
      Case C: no --pin, --disable-branching      -> no branch created, uses default latest
      Case D: --pin, --disable-branching         -> no branch created, uses pinned revision
    """
    # Create the main (parent) repository
    parent_repo = make_repo(
        new_lore_repo,
        {
            "parent-file.txt": "parent content\n",
        },
    )

    # Create a feature branch in the parent repo so the current branch
    # is something other than main (to test that branch creation propagates)
    parent_repo.branch_create("feature-test")

    # Create 4 link target repositories, each with initial content on main

    # --- Link repo A (Case A: no pin, no disable-branching) ---
    link_repo_a = make_repo(
        new_lore_repo,
        {
            "file-a.txt": "link A content\n",
        },
    )

    # --- Link repo B (Case B: pin, no disable-branching) ---
    link_repo_b = make_repo(
        new_lore_repo,
        {
            "file-b.txt": "link B content\n",
        },
    )

    # Make a second commit so we can pin to the first one
    main_latest_b = link_repo_b.branch_info().local_latest
    with link_repo_b.open_file("file-b2.txt", "w+") as f:
        f.writelines(["link B second file\n"])
    link_repo_b.stage(scan=True)
    link_repo_b.commit("Second B commit")
    link_repo_b.push()

    # --- Link repo C (Case C: no pin, disable-branching) ---
    link_repo_c = make_repo(
        new_lore_repo,
        {
            "file-c.txt": "link C content\n",
        },
    )

    # --- Link repo D (Case D: pin, disable-branching) ---
    link_repo_d = make_repo(
        new_lore_repo,
        {
            "file-d.txt": "link D content\n",
        },
    )

    # Create a feature branch in repo D so we can pin to it
    link_repo_d.branch_create("pinned-branch")
    with link_repo_d.open_file("file-d2.txt", "w+") as f:
        f.writelines(["link D pinned branch content\n"])
    link_repo_d.stage(scan=True)
    link_repo_d.commit("D pinned branch commit")
    link_repo_d.push()
    pinned_branch_latest_d = link_repo_d.branch_info().local_latest

    # === Case A: link add without --pin, without --disable-branching ===
    parent_repo.link_add("link-a", link_repo_a.get_id(), "/")

    # Verify files are accessible
    assert parent_repo.file_exists("link-a/file-a.txt"), (
        "Case A: linked file should be accessible"
    )

    # Verify branch was created in linked repo
    branch_list_a = link_repo_a.branch_list()
    assert "feature-test" in branch_list_a.remote_branches, (
        "Case A: feature-test branch should be created in linked repo"
    )

    # Verify link list shows the feature-test branch
    link_output = parent_repo.link_list()
    pattern_a = rf"Link\s+{link_repo_a.get_id()}.*?Branch:\s+feature-test"
    assert re.search(pattern_a, link_output, re.DOTALL), (
        "Case A: link list should show feature-test as the branch"
    )

    # Verify no DisableAutoFollow flag
    pattern_a_flags = rf"Link\s+{link_repo_a.get_id()}.*?Flags:\s+None"
    assert re.search(pattern_a_flags, link_output, re.DOTALL), (
        "Case A: link should have no flags set"
    )

    parent_repo.commit("Add link A")
    parent_repo.push()

    # === Case B: link add with --pin, without --disable-branching ===
    parent_repo.link_add("link-b", link_repo_b.get_id(), "/", pin=f"{main_latest_b}")

    # Verify files from the pinned revision are present (only file-b.txt, not file-b2.txt)
    assert parent_repo.file_exists("link-b/file-b.txt"), (
        "Case B: pinned file should be accessible"
    )
    assert not parent_repo.file_exists("link-b/file-b2.txt"), (
        "Case B: file from later revision should NOT be present (pinned to earlier)"
    )

    # Verify branch was created in linked repo (branching enabled)
    branch_list_b = link_repo_b.branch_list()
    assert "feature-test" in branch_list_b.remote_branches, (
        "Case B: feature-test branch should be created in linked repo even with --pin"
    )

    # Verify link list shows the feature-test branch (not the pinned revision's branch)
    link_output = parent_repo.link_list()
    pattern_b = rf"Link\s+{link_repo_b.get_id()}.*?Branch:\s+feature-test"
    assert re.search(pattern_b, link_output, re.DOTALL), (
        "Case B: link list should show feature-test as the branch"
    )

    # Verify the revision is the pinned one
    pattern_b_rev = rf"Link\s+{link_repo_b.get_id()}.*?Revision:\s+{main_latest_b}"
    assert re.search(pattern_b_rev, link_output, re.DOTALL), (
        "Case B: link should be pinned to the specified revision"
    )

    # Verify no DisableAutoFollow flag
    pattern_b_flags = rf"Link\s+{link_repo_b.get_id()}.*?Flags:\s+None"
    assert re.search(pattern_b_flags, link_output, re.DOTALL), (
        "Case B: link should have no flags set"
    )

    parent_repo.commit("Add link B")
    parent_repo.push()

    # === Case C: link add without --pin, with --disable-branching ===
    parent_repo.link_add("link-c", link_repo_c.get_id(), "/", disable_branching=True)

    # Verify files are accessible
    assert parent_repo.file_exists("link-c/file-c.txt"), (
        "Case C: linked file should be accessible"
    )

    # Verify NO branch was created in linked repo
    branch_list_c = link_repo_c.branch_list()
    assert "feature-test" not in branch_list_c.remote_branches, (
        "Case C: feature-test branch should NOT be created when --disable-branching"
    )

    # Verify link list shows the default branch (main), not feature-test
    link_output = parent_repo.link_list()
    pattern_c = rf"Link\s+{link_repo_c.get_id()}.*?Branch:\s+main"
    assert re.search(pattern_c, link_output, re.DOTALL), (
        "Case C: link list should show main as the branch (default branch fallback)"
    )

    # Verify DisableAutoFollow flag is set
    pattern_c_flags = (
        rf"Link\s+{link_repo_c.get_id()}.*?Flags:\s+DisableAutoFollow \(0x1\)"
    )
    assert re.search(pattern_c_flags, link_output, re.DOTALL), (
        "Case C: link should have DisableAutoFollow flag"
    )

    parent_repo.commit("Add link C")
    parent_repo.push()

    # === Case D: link add with --pin, with --disable-branching ===
    parent_repo.link_add(
        "link-d",
        link_repo_d.get_id(),
        "/",
        pin="pinned-branch@LATEST",
        disable_branching=True,
    )

    # Verify files from the pinned branch are present
    assert parent_repo.file_exists("link-d/file-d.txt"), (
        "Case D: base file should be accessible"
    )
    assert parent_repo.file_exists("link-d/file-d2.txt"), (
        "Case D: pinned branch file should be accessible"
    )

    # Verify NO new branch was created in linked repo
    branch_list_d = link_repo_d.branch_list()
    assert "feature-test" not in branch_list_d.remote_branches, (
        "Case D: feature-test branch should NOT be created when --disable-branching"
    )

    # Verify link list shows the pinned branch (pinned-branch), not the parent's branch
    link_output = parent_repo.link_list()
    pattern_d = rf"Link\s+{link_repo_d.get_id()}.*?Branch:\s+pinned-branch"
    assert re.search(pattern_d, link_output, re.DOTALL), (
        "Case D: link list should show pinned-branch as the branch"
    )

    # Verify the revision is from the pinned branch
    pattern_d_rev = (
        rf"Link\s+{link_repo_d.get_id()}.*?Revision:\s+{pinned_branch_latest_d}"
    )
    assert re.search(pattern_d_rev, link_output, re.DOTALL), (
        "Case D: link should be pinned to the pinned-branch revision"
    )

    # Verify DisableAutoFollow flag is set
    pattern_d_flags = (
        rf"Link\s+{link_repo_d.get_id()}.*?Flags:\s+DisableAutoFollow \(0x1\)"
    )
    assert re.search(pattern_d_flags, link_output, re.DOTALL), (
        "Case D: link should have DisableAutoFollow flag"
    )

    parent_repo.commit("Add link D")
    parent_repo.push()

    # === Verify auto-follow only propagates to non-disabled links ===
    # Create a new branch — should propagate to A and B but NOT C and D
    parent_repo.branch_create("auto-follow-test")
    parent_repo.push()

    branch_list_a = link_repo_a.branch_list()
    assert "auto-follow-test" in branch_list_a.remote_branches, (
        "Auto-follow: branch should propagate to link A (no disable-branching)"
    )

    branch_list_b = link_repo_b.branch_list()
    assert "auto-follow-test" in branch_list_b.remote_branches, (
        "Auto-follow: branch should propagate to link B (no disable-branching)"
    )

    branch_list_c = link_repo_c.branch_list()
    assert "auto-follow-test" not in branch_list_c.remote_branches, (
        "Auto-follow: branch should NOT propagate to link C (disable-branching)"
    )

    branch_list_d = link_repo_d.branch_list()
    assert "auto-follow-test" not in branch_list_d.remote_branches, (
        "Auto-follow: branch should NOT propagate to link D (disable-branching)"
    )


@pytest.mark.smoke
def test_implicit_link_branch(new_lore_repo):
    """Test the implicit link branch convention.

    When a link is added with branching enabled (the default), the
    LinkReference stores branch = zero. All operations that read
    the branch resolve zero to the parent's current branch ID.

    Branch creation in a repo with zero-branch links must not produce
    additional revisions in the parent (no bookkeeping revisions).
    """
    # Create the parent repository
    parent: Lore = new_lore_repo()
    parent.write_commit_push("Initial parent", {"parent.txt": "parent content\n"})

    # Create the linked repository
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"linked.txt": "linked content\n"})

    # Add the link (branching enabled by default — stores zero branch)
    parent.link_add("my-link", link_repo.get_id(), "/")
    parent.commit("Add link")
    parent.push()

    # Verify link list displays correct branch name (not empty/zero)
    link_output = parent.link_list()
    assert link_repo.get_id() in link_output, "Link should appear in link list"
    # The branch name should be the parent's branch name (e.g. 'main'),
    # not empty or a zero UUID
    zero_uuid = "00000000000000000000000000000000"
    assert zero_uuid not in link_output, (
        "link list should resolve zero branch to actual branch, not show zero UUID"
    )

    # Record revision count before branch creation
    history_before = parent.history()
    rev_count_before = len(history_before)

    # Create a new branch — should NOT produce a bookkeeping revision
    parent.branch_create("feature-branch")

    # Check revision count after branch creation
    history_after = parent.history()
    rev_count_after = len(history_after)

    assert rev_count_after == rev_count_before, (
        f"Branch creation should not produce bookkeeping revisions. "
        f"Before: {rev_count_before}, After: {rev_count_after}"
    )

    # Commit changes in the linked repo on the new branch
    with parent.open_file("my-link/new-file.txt", "w+") as f:
        f.write("new file in link\n")
    parent.stage(scan=True)
    parent.commit("Commit in link on feature branch")

    # Push should succeed with zero-branch link
    parent.push()

    # Verify link list still shows correct branch after branch switch
    link_output_feature = parent.link_list()
    assert zero_uuid not in link_output_feature, (
        "link list should still resolve zero branch after branch creation"
    )


@pytest.mark.smoke
def test_implicit_link_branch_disable_branching(new_lore_repo):
    """Test that --disable-branching still stores an explicit branch."""
    parent: Lore = new_lore_repo()
    parent.write_commit_push("Initial parent", {"parent.txt": "parent content\n"})

    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"linked.txt": "linked content\n"})

    # Add with disable-branching — should store explicit branch
    parent.link_add("my-link", link_repo.get_id(), "/", disable_branching=True)
    parent.commit("Add link with disable-branching")
    parent.push()

    # link list should still show a valid branch
    link_output = parent.link_list()
    assert link_repo.get_id() in link_output, "Link should appear in link list"


@pytest.mark.smoke
def test_link_branch_create_reuses_existing_link_branch_id(new_lore_repo):
    """Branch creation succeeds when the linked repo already holds the
    requested branch ID, and the existing linked branch is reused.

    The auto-follow cascade creates the parent's branch ID in every linked
    repository. When that ID is already present there, the cascade adopts it:
    the parent branch is created, and the linked branch keeps its identity
    and its latest revision.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)

    branch_id = "5b9c1d2e3f4a4b5c8d9e0f1a2b3c4d5e"
    branch_name = "shared-feature"

    link_repo.branch_create(branch_name, id=branch_id)
    link_repo.write_commit_push("Link work on shared branch", {"on-branch.txt": "x\n"})
    link_latest_before = link_repo.branch_info(branch_id).local_latest

    parent.branch_create(branch_name, id=branch_id)

    parent_branch = parent.branch_info()
    assert parent_branch.name == branch_name, (
        f"Parent must be on the newly created branch.\nGot: {parent_branch.name!r}"
    )
    assert parent_branch.id == branch_id, (
        "Parent branch must carry the requested branch ID.\n"
        f"Expected: {branch_id}\nGot: {parent_branch.id}"
    )

    link_branch = link_repo.branch_info(branch_id)
    assert link_branch.name == branch_name, (
        "The pre-existing linked branch must be reused under its own name.\n"
        f"Expected: {branch_name!r}\nGot: {link_branch.name!r}"
    )
    assert link_branch.local_latest == link_latest_before, (
        "Reusing the linked branch must leave its latest revision untouched.\n"
        f"Expected: {link_latest_before}\nGot: {link_branch.local_latest}"
    )

    link_output = parent.link_list()
    assert branch_name in link_output, (
        f"link list must resolve the link to {branch_name!r}.\nOutput:\n{link_output}"
    )


@pytest.mark.smoke
def test_link_branch_create_reports_reuse_in_event(new_lore_repo):
    """Reusing a linked branch is reported as `linkBranchCreate` with
    `reused` set.

    The event carries the mount path, the linked repository, the branch ID,
    and that branch's latest revision, so a caller can tell an adopted branch
    from a freshly created one without parsing text output.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)

    branch_id = "3d4e5f60718293a4b5c6d7e8f9a0b1c2"
    branch_name = "evented-feature"

    link_repo.branch_create(branch_name, id=branch_id)
    link_repo.write_commit_push("Link work", {"evented.txt": "w\n"})
    link_head = link_repo.branch_info(branch_id).local_latest

    output = parent.branch_create(branch_name, id=branch_id, json=True)

    events = parse_jsonl(output, "linkBranchCreate")
    assert len(events) == 1, (
        f"Expected exactly one linkBranchCreate event.\nOutput:\n{output}"
    )
    event = events[0]
    assert event["reused"] is True, (
        f"Event must mark the branch as reused.\nGot: {event}"
    )
    assert event["linkPath"] == DEFAULT_LINK_MOUNT, (
        f"Event must name the mount path.\nExpected: {DEFAULT_LINK_MOUNT}\n"
        f"Got: {event['linkPath']}"
    )
    assert event["linkRepository"] == link_repo.get_id(), (
        "Event must name the linked repository holding the reused branch.\n"
        f"Expected: {link_repo.get_id()}\nGot: {event['linkRepository']}"
    )
    assert event["branch"] == branch_id, (
        f"Event must name the reused branch ID.\nExpected: {branch_id}\n"
        f"Got: {event['branch']}"
    )
    assert event["revision"] == link_head, (
        "Event must carry the reused branch's latest revision.\n"
        f"Expected: {link_head}\nGot: {event['revision']}"
    )

    created = parse_jsonl(output, "branchCreate")
    assert [entry["name"] for entry in created] == [branch_name], (
        f"The parent branch must still be reported as created.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_branch_create_reports_creation_in_event(new_lore_repo):
    """Creating a link's branch is reported as `linkBranchCreate` with
    `reused` clear.

    The cascade reports an outcome for every link it touches, not only the
    reuse case, so a caller sees what happened in each linked repository.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)

    branch_name = "fresh-feature"
    output = parent.branch_create(branch_name, json=True)

    events = parse_jsonl(output, "linkBranchCreate")
    assert len(events) == 1, (
        f"Expected exactly one linkBranchCreate event.\nOutput:\n{output}"
    )
    event = events[0]
    assert event["reused"] is False, (
        f"Event must mark the branch as newly created.\nGot: {event}"
    )
    assert event["linkPath"] == DEFAULT_LINK_MOUNT, (
        f"Event must name the mount path.\nExpected: {DEFAULT_LINK_MOUNT}\n"
        f"Got: {event['linkPath']}"
    )
    assert event["linkRepository"] == link_repo.get_id(), (
        f"Event must name the linked repository.\nGot: {event['linkRepository']}"
    )

    branch_list = link_repo.branch_list()
    assert branch_name in branch_list.remote_branches, (
        f"The branch must exist in the linked repository.\nGot: {branch_list}"
    )


@pytest.mark.smoke
def test_link_branch_create_reports_each_mount_of_same_repo(new_lore_repo):
    """One outcome event per mount when a repository is linked twice.

    A repository can be mounted at more than one path, so the mount path is
    what distinguishes the two cascade entries - the repository ID is the same
    for both.
    """
    parent: Lore = new_lore_repo()
    parent.write_commit_push("Initial parent", {"parent.txt": "parent content\n"})

    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"linked.txt": "linked content\n"})

    mounts = ["vendor/first", "vendor/second"]
    for mount in mounts:
        parent.link_add(mount, link_repo.get_id(), "/")
        parent.commit(f"Add link at {mount}")
        parent.push()

    output = parent.branch_create("dual-mount", json=True)

    events = parse_jsonl(output, "linkBranchCreate")
    assert sorted(event["linkPath"] for event in events) == mounts, (
        "Each mount of the linked repository must report its own outcome "
        f"event.\nOutput:\n{output}"
    )
    assert {event["linkRepository"] for event in events} == {link_repo.get_id()}, (
        "Both events must name the same linked repository, which is why the "
        f"mount path is needed to tell them apart.\nOutput:\n{output}"
    )
    assert len({event["revision"] for event in events}) == 1, (
        "Both mounts address one branch in one repository, so the cascade must "
        "resolve it once and report the same revision for every mount rather "
        f"than creating it per mount.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_branch_create_reuses_link_branch_id_under_other_name(new_lore_repo):
    """A linked branch that already owns the requested ID keeps its own name.

    Adoption is keyed on the branch ID, so a linked branch created earlier
    under a different name is reused as-is rather than renamed or recreated.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)

    branch_id = "7a1b2c3d4e5f6071829304a5b6c7d8e9"
    link_branch_name = "link-local-name"
    parent_branch_name = "parent-name"

    link_repo.branch_create(link_branch_name, id=branch_id)
    link_repo.write_commit_push("Link work", {"link-only.txt": "y\n"})
    link_latest_before = link_repo.branch_info(branch_id).local_latest

    parent.branch_create(parent_branch_name, id=branch_id)

    assert parent.branch_info().name == parent_branch_name, (
        "Parent branch must be created under the requested name"
    )

    link_branch = link_repo.branch_info(branch_id)
    assert link_branch.name == link_branch_name, (
        "The linked branch must keep the name it was created with.\n"
        f"Expected: {link_branch_name!r}\nGot: {link_branch.name!r}"
    )
    assert link_branch.local_latest == link_latest_before, (
        "Adopting the linked branch must leave its latest revision untouched.\n"
        f"Expected: {link_latest_before}\nGot: {link_branch.local_latest}"
    )


@pytest.mark.smoke
def test_link_branch_create_commits_onto_reused_link_branch(new_lore_repo):
    """Work committed through the mount lands on the reused linked branch.

    Proves adoption leaves a usable link: after the parent branch is created
    on top of a pre-existing linked branch, a commit through the mount path
    advances that same linked branch.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)

    branch_id = "1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f"
    branch_name = "adopted-feature"

    link_repo.branch_create(branch_name, id=branch_id)
    link_repo.push()
    link_latest_before = link_repo.branch_info().local_latest

    parent.branch_create(branch_name, id=branch_id)

    mounted_file = f"{DEFAULT_LINK_MOUNT}/through-mount.txt"
    with parent.open_file(mounted_file, "w+") as f:
        f.write("written through the mount\n")
    parent.stage(scan=True)
    parent.commit("Commit into adopted link branch")
    parent.push()

    link_repo.sync()
    link_branch_after = link_repo.branch_info()
    assert link_branch_after.id == branch_id, (
        "The linked repo must still be on the reused branch"
    )
    assert link_branch_after.local_latest != link_latest_before, (
        "Committing through the mount must advance the reused linked branch"
    )
    assert link_repo.file_exists("through-mount.txt"), (
        "The file committed through the mount must be present on the reused "
        "linked branch"
    )


@pytest.mark.smoke
def test_link_update_after_reusing_link_branch_pulls_branch_head(new_lore_repo):
    """`link update` after adoption re-pins to the reused branch's head.

    The adopted branch may already carry revisions the parent's pin predates.
    Those become available the moment the link is updated, which is the normal
    way a pin advances.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)

    branch_id = "2f3e4d5c6b7a8908172635445362718f"
    branch_name = "ahead-feature"

    link_repo.branch_create(branch_name, id=branch_id)
    link_repo.write_commit_push(
        "Work already on the linked branch", {"ahead.txt": "z\n"}
    )
    link_head = link_repo.branch_info(branch_id).local_latest

    parent.branch_create(branch_name, id=branch_id)

    parent.link_update(DEFAULT_LINK_MOUNT)
    parent.commit("Update link to adopted branch head")
    parent.push()

    assert parent.file_exists(f"{DEFAULT_LINK_MOUNT}/ahead.txt"), (
        "link update must materialise content from the reused branch's head"
    )

    link_output = parent.link_list()
    assert link_head in link_output, (
        "link list must report the reused branch's head as the pin.\n"
        f"Expected: {link_head}\nOutput:\n{link_output}"
    )


@pytest.mark.smoke
def test_push_names_parent_branch_for_parent_revision(new_lore_repo):
    """`lore push` must attribute the parent's revision to the parent's branch.

    When a parent repository links a child and both create a branch of the same
    name, the cascade disambiguates the child's
    branch by appending the parent branch id (child ends up with
    ``<name>-<parent branch id>``). During push the parent walks its own history
    and, for each revision, pushes the link first before pushing the parent's own
    revision. The CLI stored the branch name of the most recent BranchPush event
    in shared state, so the child's BranchPush (fired while pushing the link)
    overwrote it. The parent's subsequent "Pushing <rev> to branch <name>" /
    "Pushed revision N -> <rev> to branch <name>" lines then printed the child's
    disambiguated branch name even though the revision was the parent's.

    This asserts the correct, positive behaviour: the push line that reports the
    parent's own revision names the parent's branch (``test``).
    """
    child_repo = make_repo(
        new_lore_repo,
        {
            "child-file.txt": "initial child content\n",
        },
    )

    # Occupy `test` in the child so the parent's cascade must disambiguate it.
    child_repo.branch_create("test")
    child_repo.push("test")
    child_repo.branch_switch("main")

    parent_repo = make_repo(
        new_lore_repo,
        {
            "parent-file.txt": "parent content\n",
        },
    )

    link_path = "linked"
    parent_repo.link_add(link_path, child_repo.get_id(), "/")
    parent_repo.commit("Add link")
    parent_repo.push()

    parent_repo.branch_create("test")
    parent_branch_id = parent_repo.branch_info().id
    disambiguated_child_branch = f"test-{parent_branch_id}"

    with parent_repo.open_file(f"{link_path}/child-file.txt", "w+") as f:
        f.writelines(["updated via parent link mount\n"])
    parent_repo.stage(f"{link_path}/child-file.txt")
    parent_repo.commit("probe")

    parent_revision = parent_repo.branch_info().local_latest

    push_output = parent_repo.push()

    # Key on the parent's revision signature to isolate the parent's push lines.
    begin_match = re.search(rf"Pushing {parent_revision} to branch (\S+)", push_output)
    assert begin_match, (
        f"Expected a push line for the parent's revision {parent_revision}.\n"
        f"Push output:\n{push_output}"
    )
    assert begin_match.group(1) == "test", (
        f"The parent's revision {parent_revision} must be reported as pushed to "
        f"the parent's branch 'test', but it named '{begin_match.group(1)}'. "
        f"'{disambiguated_child_branch}' is the child's branch, which exists only "
        f"in the linked repository.\nPush output:\n{push_output}"
    )

    end_match = re.search(
        rf"Pushed revision \d+ -> {parent_revision} to branch (\S+)", push_output
    )
    assert end_match, (
        f"Expected a 'Pushed revision' line for the parent's revision "
        f"{parent_revision}.\nPush output:\n{push_output}"
    )
    assert end_match.group(1) == "test", (
        f"The parent's revision {parent_revision} must be reported as pushed to "
        f"the parent's branch 'test', but it named '{end_match.group(1)}'. "
        f"'{disambiguated_child_branch}' is the child's branch, which exists only "
        f"in the linked repository.\nPush output:\n{push_output}"
    )


@pytest.mark.smoke
def test_link_push_tells_a_layer_from_a_link(new_lore_repo):
    """A repository holding both a link and a layer reports each mount as its own
    kind, and the parent as neither.

    A mount carries one kind, so each context reports exactly one of the three
    and none reports both flags at once.
    """
    parent_repo, _link_repo = make_parent_with_link(new_lore_repo, "vendor/lib")

    layer_repo: Lore = new_lore_repo(parent_repo.name + "_layer")
    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {"lay/layered.txt": b"layer content"})
    parent_repo.layer_add("lay", layer_repo, "lay/")

    parent_repo.write_files({"vendor/lib/linked.txt": "edited through the mount\n"})
    parent_repo.stage("vendor/lib/linked.txt")
    parent_repo.commit("Edit through the mount")

    kinds = sorted(
        (event["flagLink"], event["flagLayer"])
        for event in parse_jsonl(parent_repo.push(json=True), "branchPush")
    )

    assert kinds == [(False, False), (False, True), (True, False)], (
        "Push must report the parent as neither a link nor a layer, the layer as "
        f"a layer, and the link as a link, each once, got {kinds}"
    )


def _current_revision(repo: Lore) -> str:
    """The revision `status` reports the working copy at."""
    match = re.search(r"On branch \S+ revision \d+ -> (\w+)", repo.status())
    assert match, "status must report the current revision"
    return match.group(1)


def _tracks(repo: Lore, link_path: str) -> bool:
    """Whether the link at `link_path` follows its parent's branch."""
    entries = parse_jsonl(repo.link_list(json=True), "linkEntry")
    return bool(next(e for e in entries if e["linkPath"] == link_path)["tracking"])


def _assert_link_follows_feature(repo: Lore, source: Lore):
    assert re.search(
        rf"Link\s+{source.get_id()}.*?Branch:\s+feature\b[^\n]*\[tracking\]",
        repo.link_list(),
        re.DOTALL,
    ), "the link must follow the parent onto the new branch"


@pytest.mark.smoke
@pytest.mark.parametrize("pin", [None, "main@LATEST"], ids=["latest", "pin"])
def test_link_update_along_the_parent_branch_keeps_the_link_tracking(
    new_lore_repo, pin
):
    """A `link update` to a revision on the parent's branch keeps the link
    following the parent's branch: a branch created over it starts at the
    parent's revision, and a later update on main reaches it through a merge."""
    parent, source = make_parent_with_link(new_lore_repo)
    assert (
        f"{DEFAULT_LINK_MOUNT} is already up to date; not pinned"
        in parent.link_update(DEFAULT_LINK_MOUNT)
    ), "updating a link whose repository has not moved must say it pins nothing"
    source.write_commit_push("Advance", {"advanced.txt": "advanced\n"})
    parent.link_update(DEFAULT_LINK_MOUNT, pin=pin)
    parent.commit("Move the link")
    parent.push()
    assert _tracks(parent, DEFAULT_LINK_MOUNT), "the link must follow main"
    start = _current_revision(parent)

    parent.branch_create("feature")

    assert parent.branch_info().local_latest == start, (
        "a branch over a tracking link must start at the parent's revision"
    )
    assert _current_revision(parent) == start
    parent.write_commit_push("First commit", {"feature.txt": "on feature\n"})

    parent.branch_switch("main")
    source.write_commit_push("Advance again", {"again.txt": "again\n"})
    parent.link_update(DEFAULT_LINK_MOUNT)
    parent.commit("Move the link on main")
    parent.push()
    parent.branch_switch("feature")
    parent.branch_merge("main", message="Merge main")
    parent.push()

    _assert_link_follows_feature(parent, source)
    with parent.open_file(f"{DEFAULT_LINK_MOUNT}/again.txt", "r") as input_file:
        assert input_file.read() == "again\n", "the merge must carry the update"


def _stage_an_edit(parent: Lore, _source: Lore):
    parent.write_files({"staged.txt": "staged\n"})
    parent.stage("staged.txt")


def _stage_a_link_removal(parent: Lore, _source: Lore):
    parent.link_remove(DEFAULT_LINK_MOUNT)


def _stage_a_remount(parent: Lore, source: Lore):
    remount(parent, DEFAULT_LINK_MOUNT, source.get_id(), "/sub", message=None)


def _stage_a_divergent_merge(parent: Lore, _source: Lore):
    """Stages the merge of main's latest, which another clone pushed, into a
    local commit on main."""
    other = parent.clone()
    other.write_commit_push("Remote change", {"remote.txt": "remote\n"})
    parent.write_files({"local.txt": "local\n"})
    parent.stage("local.txt")
    parent.commit("Local change")
    parent.branch_merge_start("main", no_commit=True)


def _branch_over_a_link_on_another_branch(new_lore_repo, stage) -> tuple[Lore, Lore]:
    """Parent on a new `feature` branch, created over a link that `link update`
    pinned to another branch of the linked repository, with what `stage` stages
    before the branch was created, and the linked repository."""
    parent, source = make_parent_with_link(
        new_lore_repo,
        link_files={"linked.txt": "linked content\n", "sub/inner.txt": "inner\n"},
    )
    source.branch_create("release")
    source.write_commit_push("Release", {"release.txt": "release\n"})
    parent.link_update(DEFAULT_LINK_MOUNT, pin="release@LATEST")
    parent.commit("Pin the link to release")
    parent.push()
    assert not _tracks(parent, DEFAULT_LINK_MOUNT), "the link must name release"
    stage(parent, source)

    parent.branch_create("feature")

    assert _current_revision(parent) == parent.branch_info().local_latest, (
        "the working copy must stand on the revision the new branch starts at"
    )
    return parent, source


@pytest.mark.smoke
def test_branch_created_over_a_link_on_another_branch_keeps_a_staged_edit_staged(
    new_lore_repo,
):
    """An edit staged before the branch was created stays staged, outside the
    revision the branch starts at, and is committed by the first commit."""
    parent, _source = _branch_over_a_link_on_another_branch(
        new_lore_repo, _stage_an_edit
    )

    staged = parse_status_json(parent.status(json=True))
    assert [entry["path"] for entry in staged] == ["staged.txt"], (
        f"the edit must still be staged, got {staged}"
    )

    parent.commit("First commit")
    parent.push()
    clone = parent.clone(branch="feature")
    with clone.open_file("staged.txt", "r") as input_file:
        assert input_file.read() == "staged\n", "the first commit must carry the edit"


@pytest.mark.smoke
def test_branch_created_over_a_link_on_another_branch_merges_a_staged_merge_into_it(
    new_lore_repo,
):
    """A merge of main's latest into a local commit, staged before the branch
    was created, merges main's latest into the new branch, which pushes."""
    parent, _source = _branch_over_a_link_on_another_branch(
        new_lore_repo, _stage_a_divergent_merge
    )
    remote_main = parent.branch_info("main").remote_latest

    parent.commit("Merge main")
    parent.push()

    merge = parent.revision_info().merge.split()
    assert merge[1:] == [remote_main], (
        f"the merge must take main's latest {remote_main} as its second parent, "
        f"got {merge}"
    )


@pytest.mark.smoke
@pytest.mark.parametrize(
    "stage_link_change",
    [_stage_a_link_removal, _stage_a_remount],
    ids=["remove", "remount"],
)
def test_branch_created_over_a_staged_link_change_keeps_the_link_on_it_once_unstaged(
    new_lore_repo, stage_link_change
):
    """A link on another branch that a staged removal or remount replaces
    follows the new branch once the change is unstaged, and the branch takes a
    commit and pushes."""
    parent, source = _branch_over_a_link_on_another_branch(
        new_lore_repo, stage_link_change
    )

    parent.unstage(".")

    _assert_link_follows_feature(parent, source)
    parent.write_commit_push("First commit", {"feature.txt": "on feature\n"})
    assert _current_revision(parent) == parent.branch_info().local_latest


@pytest.mark.smoke
def test_branch_created_over_nested_links_takes_a_commit(new_lore_repo):
    """A link added inside another keeps the outer link tracking, so a branch
    created over them starts at the parent's revision and takes its first
    commit. Pushing it needs the nested repository's branch, which branch
    creation does not make."""
    parent, _b, _c, b_mount, _nested_mount = build_nested_link_repos_at(
        new_lore_repo, "vendor", "nested"
    )
    assert _tracks(parent, b_mount), "the outer link must follow main"
    start = _current_revision(parent)

    parent.branch_create("feature")
    assert parent.branch_info().local_latest == start
    assert _current_revision(parent) == start

    parent.write_files({"feature.txt": "on feature\n"})
    parent.stage("feature.txt")
    parent.commit("First commit")

    assert _current_revision(parent) == parent.branch_info().local_latest


def _stage_a_link_add(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent with a link staged by `link add` and not committed, and the
    linked repository."""
    parent = make_repo(new_lore_repo, {DEFAULT_PARENT_FILE: "baseline\n"})
    source = make_repo(new_lore_repo, {"linked.txt": "linked content\n"})
    parent.link_add(DEFAULT_LINK_MOUNT, source.get_id(), "/")
    return parent, source


def _stage_a_link_update(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent with a committed link that a staged `link update` repins, and the
    linked repository."""
    parent, source = make_parent_with_link(new_lore_repo)
    source.write_commit_push("Advance", {"advanced.txt": "advanced\n"})
    parent.link_update(DEFAULT_LINK_MOUNT)
    return parent, source


@pytest.mark.smoke
@pytest.mark.parametrize(
    "stage_link", [_stage_a_link_add, _stage_a_link_update], ids=["add", "update"]
)
def test_branch_created_over_a_staged_link_branches_the_linked_repository(
    new_lore_repo, stage_link
):
    """A link only staged, added or repinned, gets the new branch in the linked
    repository, and the first commit on the branch pins the link to it and
    pushes."""
    parent, source = stage_link(new_lore_repo)

    parent.branch_create("feature")

    cascaded = [
        e["id"] for e in remote_branch_entries(source) if e["name"] == "feature"
    ]
    assert cascaded == [parent.branch_info("feature").id], (
        f"branch create must make the branch in the linked repository, got {cascaded}"
    )

    parent.commit("Commit the staged link")
    parent.push()

    _assert_link_follows_feature(parent, source)


@pytest.mark.smoke
def test_link_push_onto_a_moved_link_branch_fails_as_diverged(new_lore_repo):
    """A link push onto a linked branch that moved on fails as a diverged link.

    Linked `main` moves on from another clone, so the commit made through the
    mount is not a fast-forward of it.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)
    link_repo.clone().write_commit_push(
        "Move linked main", {"linked.txt": "moved on upstream\n"}
    )

    mounted_file = f"{DEFAULT_LINK_MOUNT}/linked.txt"
    parent.write_files({mounted_file: "edited through the mount\n"})
    parent.stage(mounted_file)
    parent.commit("Edit through the mount")

    with pytest.raises(LinkDivergedError):
        parent.push()


@pytest.mark.smoke
def test_link_push_reports_every_diverged_link(new_lore_repo):
    """A push that stops at several diverged links names each of them."""
    parent, first_link = make_parent_with_link(new_lore_repo, "vendor/first")
    second_link = make_repo(new_lore_repo, {"linked.txt": "second linked content\n"})
    parent.link_add("vendor/second", second_link.get_id(), "/")
    parent.commit("Add a second link")
    parent.push()

    for link_repo in (first_link, second_link):
        link_repo.clone().write_commit_push(
            "Move linked main", {"linked.txt": "moved on upstream\n"}
        )
    edits = {
        "vendor/first/linked.txt": "edited through the first mount\n",
        "vendor/second/linked.txt": "edited through the second mount\n",
    }
    parent.write_files(edits)
    parent.stage(list(edits))
    parent.commit("Edit through both mounts")

    with pytest.raises(LinkDivergedError) as failure:
        parent.push(json=True)

    output = str(failure.value)
    completes = parse_jsonl(output, "complete")
    error_logs = [log for log in parse_jsonl(output, "log") if log["level"] == "error"]
    assert [complete["status"] for complete in completes] == [52], completes
    # The failing completion logs its own error, beside one line for the other failure.
    assert len(error_logs) == 2, error_logs


@pytest.mark.smoke
def test_push_of_a_diverged_layer_pushes_nothing(new_lore_repo):
    """A push whose layer has diverged fails before it pushes the repository."""
    parent = make_repo(new_lore_repo, {DEFAULT_PARENT_FILE: "baseline\n"})
    layer_repo: Lore = new_lore_repo(parent.name + "_layer")
    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {"lay/layered.txt": b"layer content"})
    parent.layer_add("lay", layer_repo, "lay/")
    pushed = parent.branch_info().remote_latest

    layer_repo.write_commit_push(
        "Move layer main", {"lay/layered.txt": b"moved on upstream"}
    )
    edits = {
        "lay/layered.txt": "edited through the parent\n",
        DEFAULT_PARENT_FILE: "edited\n",
    }
    parent.write_files(edits)
    parent.stage(list(edits))
    parent.commit("Edit the layer and the parent")

    with pytest.raises(BranchDivergedError):
        parent.push()

    assert parent.branch_info().remote_latest == pushed


def make_parent_reaching_shared_twice(new_lore_repo) -> tuple[Lore, Lore]:
    """Returns (parent, shared): the parent links `first` and `second`, each of
    which links `shared` at `<name>/shared`, all pushed."""
    parent = make_repo(new_lore_repo, {DEFAULT_PARENT_FILE: "baseline\n"})
    shared = make_repo(new_lore_repo, {"shared.txt": "shared content\n"})
    for name in ("first", "second"):
        middle = make_repo(new_lore_repo, {f"{name}.txt": f"{name} content\n"})
        parent.link_add(name, middle.get_id(), "/")
        parent.commit(f"Link {name}")
        parent.push()
        parent.link_add(f"{name}/shared", shared.get_id(), "/")
        parent.commit(f"Link the shared repository in {name}")
        parent.push()
    return parent, shared


@pytest.mark.smoke
def test_link_push_of_diverged_revisions_of_one_branch_pushes_nothing(new_lore_repo):
    """Edits made through two mounts of one linked branch, each on the revision
    its mount pins, are diverged revisions of that branch: the push fails as a
    diverged link before it pushes either."""
    parent, shared = make_parent_reaching_shared_twice(new_lore_repo)
    pushed = shared.branch_info().remote_latest
    for name in ("first", "second"):
        mounted_file = f"{name}/shared/{name}-edit.txt"
        parent.write_files({mounted_file: f"edited through {name}\n"})
        parent.stage(mounted_file)
        parent.commit(f"Edit the shared repository through {name}")

    with pytest.raises(LinkDivergedError):
        parent.push()

    assert shared.branch_info().remote_latest == pushed


@pytest.mark.smoke
def test_link_push_pushes_a_repository_two_links_reach_once(new_lore_repo):
    """Two linked repositories that both link a third push the third once, at the
    newest revision either of them names, which holds the other."""
    parent, shared = make_parent_reaching_shared_twice(new_lore_repo)

    # Each edit lands on the shared branch's latest revision, which the second
    # mount is moved onto first, so the revisions the two name are on one line.
    for name in ("first", "second"):
        if name == "second":
            first_pin = re.search(
                r"Link path: first/shared .*?Revision:\s*(\w+)",
                parent.link_list(),
                re.DOTALL,
            ).group(1)
            parent.link_update(f"{name}/shared", pin=first_pin)
            parent.commit(f"Move {name}/shared onto the first edit")
        mounted_file = f"{name}/shared/{name}-edit.txt"
        parent.write_files({mounted_file: f"edited through {name}\n"})
        parent.stage(mounted_file)
        parent.commit(f"Edit the shared repository through {name}")

    shared_pushes = [
        event
        for event in parse_jsonl(parent.push(json=True), "branchPush")
        if event["repository"] == shared.get_id()
    ]

    assert len(shared_pushes) == 1, (
        "Both links reach the shared repository on one branch, which should be "
        f"pushed once, got {len(shared_pushes)}: {shared_pushes}"
    )
    verify = parent.clone()
    assert verify.file_exists("first/shared/first-edit.txt")
    assert verify.file_exists("second/shared/first-edit.txt")
    assert verify.file_exists("second/shared/second-edit.txt")


@pytest.mark.smoke
def test_link_branch_archive_leaves_child_branch(new_lore_repo):
    """Archiving a branch in a parent does not remove the linked repository's.

    Branch create cascades into the linked repository using the same branch ID.
    Archiving in the parent then touches only the parent.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)

    # Branch create switches the parent onto the new branch and cascades.
    parent.branch_create("feature")
    parent_branch_id = parent.branch_info("feature").id

    cascaded = [e for e in remote_branch_entries(link_repo) if e["name"] == "feature"]
    assert len(cascaded) == 1, (
        f"Branch create should cascade one 'feature' branch into the linked "
        f"repository, got {cascaded}"
    )
    assert cascaded[0]["id"] == parent_branch_id, (
        f"Cascaded branch should carry the parent branch ID {parent_branch_id}, "
        f"got {cascaded[0]['id']}"
    )

    # The current branch cannot be archived, so step off it first.
    parent.branch_switch("main")
    parent.branch_archive("feature")

    assert not parent.has_branch("feature"), (
        "Archived branch should be gone from the parent's branch list"
    )

    survivor = [e for e in remote_branch_entries(link_repo) if e["name"] == "feature"]
    assert len(survivor) == 1, (
        f"The linked repository should keep its branch when the parent "
        f"archives, got {survivor}"
    )
    assert survivor[0]["id"] == parent_branch_id, (
        "The linked branch should be untouched by the parent archive, ID included"
    )


@pytest.mark.smoke
def test_link_archived_parent_branch_does_not_disturb_link_resolution(new_lore_repo):
    """An archived parent branch is inert while the linked branch still exists.

    A link is pinned to a revision in the parent's state, not to a branch name,
    so switch, sync and link resolution never go through the archived branch.
    """
    parent, link_repo = make_parent_with_link(new_lore_repo)

    # A sibling branch to switch through after the archive, so the switch is a
    # real branch change rather than a switch onto the current branch.
    parent.branch_create("sibling")
    parent.branch_switch("main")

    parent.branch_create("feature")
    parent.write_commit_push(
        "Commit inside the link mount on feature",
        {f"{DEFAULT_LINK_MOUNT}/on-feature.txt": "written on the feature branch\n"},
    )

    parent.branch_switch("main")
    parent.branch_archive("feature")

    assert any(e["name"] == "feature" for e in remote_branch_entries(link_repo)), (
        "Precondition: the linked branch outlives the parent's archive"
    )

    # main is unaffected: the link still resolves and its content is present.
    parent.sync()
    assert parent.file_exists(f"{DEFAULT_LINK_MOUNT}/linked.txt"), (
        "Linked content should still be present on main after archiving another branch"
    )
    assert not parent.file_exists(f"{DEFAULT_LINK_MOUNT}/on-feature.txt"), (
        "main should see its own pinned revision of the link, not the archived branch's"
    )

    links = parent.link_list()
    assert link_repo.get_id() in links, (
        f"Link should still resolve after the archive: {links}"
    )
    assert "[Error]" not in links, f"link list should not report an error: {links}"

    # Switching away and back still works with the archived branch present.
    parent.branch_switch("sibling")
    parent.branch_switch("main")
    assert parent.branch_list().current_branch == "main", (
        "Branch switch should work with an archived branch present"
    )


# ---------------------------------------------------------------------------
# Branch archive cascading into links.
# ---------------------------------------------------------------------------


def _setup_repo_with_two_links(new_lore_repo):
    """Set up a parent repo with two links (sec, thr) and content in each.

    Returns (parent_repo, second_repo, third_repo).
    """
    repo: Lore = new_lore_repo()
    second_repo: Lore = new_lore_repo(repo.name + "_second")
    third_repo: Lore = new_lore_repo(repo.name + "_third")

    repo.write_commit_push(None, {"main.txt": b"main content"})
    second_repo.write_commit_push(None, {"second.txt": b"second content"})
    third_repo.write_commit_push(None, {"third.txt": b"third content"})

    repo.link_add("sec", second_repo.get_id(), "/")
    repo.link_add("thr", third_repo.get_id(), "/")
    repo.commit("add links")
    repo.push()
    return repo, second_repo, third_repo


@pytest.mark.smoke
def test_link_branch_archive_include_links(new_lore_repo):
    """`--include-links` archives the branch in the linked repository too, so
    the link is left with exactly the branches it had before the create.
    """
    repo, link_repo = make_parent_with_link(new_lore_repo)

    branches_before = sorted(link_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    assert link_repo.branch_list().has_remote_branch("feature"), (
        "Expected branch create to cascade into the linked repository"
    )

    repo.branch_switch("main")
    repo.branch_archive("feature", include_links=True)

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == branches_before, (
        f"Expected link branches {branches_before}, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_multiple_links(new_lore_repo):
    """`--include-links` archives the branch in every configured link."""
    repo, second_repo, third_repo = _setup_repo_with_two_links(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    for link in (second_repo, third_repo):
        assert link.branch_list().has_remote_branch("feature"), (
            f"Expected branch create to cascade into {link.name}"
        )

    repo.branch_switch("main")
    repo.branch_archive("feature", include_links=True)

    for link in (second_repo, third_repo):
        assert sorted(link.branch_list().remote_branches) == ["main"], (
            f"Expected only 'main' remaining in {link.name}, got: {link.branch_list()}"
        )


@pytest.mark.smoke
def test_link_branch_archive_single_link(new_lore_repo):
    """`--link <path>` archives the branch in that link and no other."""
    repo, second_repo, third_repo = _setup_repo_with_two_links(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    repo.branch_archive("feature", link="sec")

    assert sorted(second_repo.branch_list().remote_branches) == ["main"], (
        f"Expected the scoped link to be archived, got: {second_repo.branch_list()}"
    )
    assert third_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the other link to be left alone, got: {third_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_unknown_link_errors(new_lore_repo):
    """`--link` naming a path that is not a link is an error, not a silent no-op."""
    repo, link_repo = make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", link=DEFAULT_PARENT_FILE, check=False)

    assert "not a link" in output.lower(), (
        f"Expected a non-link path to be reported, got: {output}"
    )
    assert link_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the link to be left alone, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_link_flags_conflict(new_lore_repo):
    """`--include-links` and `--link` are mutually exclusive."""
    repo, _link_repo = make_parent_with_link(new_lore_repo)

    output = repo.branch_archive(
        "feature", include_links=True, link=DEFAULT_LINK_MOUNT, check=False
    )

    assert "cannot be used with" in output.lower(), (
        f"Expected clap to reject the flag combination, got: {output}"
    )


@pytest.mark.smoke
def test_link_branch_archive_local_keeps_link_remote(new_lore_repo):
    """`--local --include-links` archives the link's local cache only, leaving
    the link's remote branch in place.
    """
    repo, link_repo = make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()

    repo.branch_switch("main")
    repo.branch_archive("feature", local=True, include_links=True)

    assert sorted(repo.branch_list().local_branches) == ["main"], (
        f"Expected only 'main' remaining locally, got: {repo.branch_list()}"
    )
    assert link_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the link remote branch to remain, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_tolerates_already_archived_link(new_lore_repo):
    """Archiving a branch a link already archived is not an error."""
    repo, link_repo = make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    link_repo.branch_switch("main")
    link_repo.branch_archive("feature")

    repo.branch_archive("feature", include_links=True)

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the link, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_reports_once(new_lore_repo):
    """Archiving reports a single branch, not one line per link."""
    repo, _second_repo, _third_repo = _setup_repo_with_two_links(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", include_links=True)

    assert output.count("Archived branch") == 1, (
        f"Expected one archive line for the outer repository, got: {output}"
    )


@pytest.mark.smoke
def test_link_branch_archive_current_leaves_links(new_lore_repo):
    """Refusing to archive the current branch leaves the links alone."""
    repo, link_repo = make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()

    repo.branch_archive("feature", include_links=True, check=False)

    assert link_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the link branch to be untouched, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_skips_auto_follow_disabled_link(new_lore_repo):
    """A link with auto-follow disabled never received the branch from the
    create cascade, so `--include-links` leaves its branches as they were.
    """
    repo, link_repo = make_parent_with_link(new_lore_repo, disable_branching=True)

    branches_before = sorted(link_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    repo.branch_archive("feature", include_links=True)

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == branches_before, (
        f"Expected link branches {branches_before}, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_single_auto_follow_disabled_link_errors(new_lore_repo):
    """Naming an auto-follow-disabled link with `--link` is refused, rather than
    deleting a branch the create cascade never put there.
    """
    repo, link_repo = make_parent_with_link(new_lore_repo, disable_branching=True)

    branches_before = sorted(link_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", link=DEFAULT_LINK_MOUNT, check=False)

    assert "does not follow the parent's branches" in output.lower(), (
        f"Expected the opted-out link to be reported, got: {output}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == branches_before, (
        f"Expected link branches {branches_before}, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_repository_linked_twice(new_lore_repo):
    """A repository mounted at two paths is archived once and reaches the
    branches it had before the create.
    """
    repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo(repo.name + "_link")

    repo.write_commit_push(None, {"main.txt": b"main content"})
    link_repo.write_commit_push(None, {"link.txt": b"link content"})

    repo.link_add("first", link_repo.get_id(), "/")
    repo.link_add("second", link_repo.get_id(), "/")
    repo.commit("add links")
    repo.push()

    branches_before = sorted(link_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", include_links=True)

    assert output.count("Archived branch") == 1, (
        f"Expected one archive line for the outer repository, got: {output}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == branches_before, (
        f"Expected link branches {branches_before}, got: {link_repo.branch_list()}"
    )
