# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os
import subprocess
from pathlib import Path

import pytest
from error_types import NotALinkError, NothingStagedError
from link_helpers import make_parent_with_link, make_repo
from lore_parsers import parse_commit_stats_json
from service_util import LORE_SERVICE_ENVIRONMENT, SERVICE_UNAVAILABLE

from lore import Lore


@pytest.mark.smoke
def test_link_unchanged_commit(new_lore_repo):
    """Test that links are not committed when their content hasn't changed."""
    repo: Lore = new_lore_repo()

    # Create source repository with initial files
    source_text_file = "source-file.txt"
    source_subdir_file = "source/subdir/file.txt"

    with repo.open_file(source_text_file, "w+") as output_file:
        output_file.writelines(["initial source content\n"])

    repo.make_dirs(os.path.dirname(source_subdir_file))
    with repo.open_file(source_subdir_file, "w+") as output_file:
        output_file.writelines(["initial source subdir content\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Create link repository with initial files
    link_repo = new_lore_repo()
    link_file1 = "link-file1.txt"
    link_subdir_file = "linkdir/link-file2.txt"

    with link_repo.open_file(link_file1, "w+") as output_file:
        output_file.writelines(["initial link content 1\n"])

    link_repo.make_dirs(os.path.dirname(link_subdir_file))
    with link_repo.open_file(link_subdir_file, "w+") as output_file:
        output_file.writelines(["initial link content 2\n"])

    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    # Add the link to the source repository
    link_relative_path = "linked"
    repo.link_add(link_relative_path, link_repo.get_id(), "/")

    expected_link_file1 = "linked/link-file1.txt"
    expected_link_subdir_file = "linked/linkdir/link-file2.txt"

    # Verify initial link files are present
    assert repo.compare_file(repo, expected_link_file1)
    assert repo.compare_file(repo, expected_link_subdir_file)

    # Commit the initial link setup
    repo.commit()
    repo.push()

    # Modify only source repository files (not linked content)
    with repo.open_file(source_text_file, "w+") as output_file:
        output_file.writelines(["modified source content\n"])

    new_source_file = "new-source-file.txt"
    with repo.open_file(new_source_file, "w+") as output_file:
        output_file.writelines(["new source file content\n"])

    # Stage and commit changes to source repository only
    output = repo.stage(scan=True)
    assert "2 files" in output, "Expected 2 files to be staged"

    # Commit the source repository changes
    output = repo.commit("Modify source files only", debug=True)
    assert "Commit succeeded" in output, "Commit should succeed"
    assert "Before committing link node" not in output, "Expected no link changes"

    # Now modify link content and verify link content is committed
    with repo.open_file(expected_link_file1, "w+") as output_file:
        output_file.writelines(["modified link content\n"])

    repo.stage(scan=True)
    commit_output = repo.commit("Modify link content", debug=True)

    assert "Before committing link node" in commit_output, (
        "Link should have changed when content was modified"
    )

    # More source-only changes
    final_source_file = "final-source.txt"
    with repo.open_file(final_source_file, "w+") as output_file:
        output_file.writelines(["final source content\n"])

    repo.stage(scan=True)
    commit_output = repo.commit("Final source changes", debug=True)

    assert "Before committing link node" not in commit_output, (
        "Expected link unchanged message after link content was modified"
    )


@pytest.mark.smoke
def test_link_commit_per_link_message(new_lore_repo):
    """Test that per-link commit messages are applied to linked repositories."""
    # Create link repository with initial content
    link_repo: Lore = new_lore_repo()
    link_file = "link-file.txt"
    with link_repo.open_file(link_file, "w+") as f:
        f.write("initial link content\n")
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    # Create main repository and add link
    urc: Lore = new_lore_repo()
    main_file = "main-file.txt"
    with urc.open_file(main_file, "w+") as f:
        f.write("initial main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_path = "linked"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Modify files in both repos
    with urc.open_file(main_file, "w+") as f:
        f.write("updated main content\n")
    with urc.open_file(os.path.join(link_path, link_file), "w+") as f:
        f.write("updated link content\n")
    urc.stage(scan=True)

    # Commit with per-link message
    urc.commit(
        "Main repo update",
        link_messages={link_path: "Link-specific update message"},
        non_interactive=True,
    )
    urc.push()

    # Verify main repo message
    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Main repo update", (
        f"Expected main message 'Main repo update', got '{main_info.message}'"
    )

    # Verify link repo message by checking revision info on the link repo directly
    link_repo.sync()
    link_info = link_repo.revision_info(check=True, no_pager=True)
    assert link_info.message == "Link-specific update message", (
        f"Expected link message 'Link-specific update message', got '{link_info.message}'"
    )


@pytest.mark.smoke
def test_link_commit_no_link_message_fallback(new_lore_repo):
    """Test that without per-link messages, all repos get the main message."""
    link_path = "linked"
    urc, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "initial link content\n",
        },
        {
            "main-file.txt": "initial main content\n",
        },
    )

    # Modify files in both repos
    with urc.open_file("main-file.txt", "w+") as f:
        f.write("updated main content\n")
    with urc.open_file(os.path.join(link_path, "link-file.txt"), "w+") as f:
        f.write("updated link content\n")
    urc.stage(scan=True)

    # Commit without link messages — non-interactive to avoid prompts
    urc.commit("Shared message for all", non_interactive=True)
    urc.push()

    # Verify both repos get the same message
    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Shared message for all"

    link_repo.sync()
    link_info = link_repo.revision_info(check=True, no_pager=True)
    assert link_info.message == "Shared message for all"


@pytest.mark.smoke
def test_link_commit_invalid_link_message_errors(new_lore_repo):
    """Test that --link-message with an invalid path produces an error."""
    urc = make_repo(
        new_lore_repo,
        {
            "main-file.txt": "content\n",
        },
    )

    # Modify and stage
    with urc.open_file("main-file.txt", "w+") as f:
        f.write("updated content\n")
    urc.stage(scan=True)

    # Record revision before the failed commit attempt
    info_before = urc.revision_info(check=True, no_pager=True)

    # Try to commit with an invalid link-message path — should fail
    output = urc.commit(
        "Main message",
        link_messages={"nonexistent/path": "Some message"},
        non_interactive=True,
        check=False,
    )
    assert "does not match any linked repository" in output, (
        f"Expected a specific error for invalid --link-message path, got: {output}"
    )

    # Verify no new revision was created
    info_after = urc.revision_info(check=True, no_pager=True)
    assert info_before.revision == info_after.revision, (
        "Commit should not have proceeded with an invalid --link-message path"
    )


@pytest.mark.smoke
def test_link_commit_multiple_link_messages(new_lore_repo):
    """Test that multiple --link-message flags work for different links."""
    # Create two link repositories
    link_repo_a = make_repo(
        new_lore_repo,
        {
            "file-a.txt": "link A content\n",
        },
    )

    link_repo_b = make_repo(
        new_lore_repo,
        {
            "file-b.txt": "link B content\n",
        },
    )

    # Create main repository and add both links
    urc = make_repo(
        new_lore_repo,
        {
            "main.txt": "main content\n",
        },
    )

    urc.link_add("link-a", link_repo_a.get_id(), "/")
    urc.link_add("link-b", link_repo_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    # Modify files in all three repos
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main\n")
    with urc.open_file("link-a/file-a.txt", "w+") as f:
        f.write("updated A\n")
    with urc.open_file("link-b/file-b.txt", "w+") as f:
        f.write("updated B\n")
    urc.stage(scan=True)

    # Commit with different messages per link
    urc.commit(
        "Main update",
        link_messages={
            "link-a": "Update A specifically",
            "link-b": "Update B specifically",
        },
        non_interactive=True,
    )
    urc.push()

    # Verify main repo message
    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Main update"

    # Verify each link has its specific message
    link_repo_a.sync()
    a_info = link_repo_a.revision_info(check=True, no_pager=True)
    assert a_info.message == "Update A specifically", (
        f"Expected 'Update A specifically', got '{a_info.message}'"
    )

    link_repo_b.sync()
    b_info = link_repo_b.revision_info(check=True, no_pager=True)
    assert b_info.message == "Update B specifically", (
        f"Expected 'Update B specifically', got '{b_info.message}'"
    )


@pytest.mark.smoke
def test_link_commit_partial_link_messages(new_lore_repo):
    """Test that links without a --link-message get the main message as fallback."""
    # Create two link repositories
    link_repo_a = make_repo(
        new_lore_repo,
        {
            "file-a.txt": "link A content\n",
        },
    )

    link_repo_b = make_repo(
        new_lore_repo,
        {
            "file-b.txt": "link B content\n",
        },
    )

    # Create main repository and add both links
    urc = make_repo(
        new_lore_repo,
        {
            "main.txt": "main content\n",
        },
    )

    urc.link_add("link-a", link_repo_a.get_id(), "/")
    urc.link_add("link-b", link_repo_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    # Modify files in all three repos
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main\n")
    with urc.open_file("link-a/file-a.txt", "w+") as f:
        f.write("updated A\n")
    with urc.open_file("link-b/file-b.txt", "w+") as f:
        f.write("updated B\n")
    urc.stage(scan=True)

    # Only specify message for link-a, not link-b
    urc.commit(
        "Main fallback message",
        link_messages={"link-a": "Specific A message"},
        non_interactive=True,
    )
    urc.push()

    # Verify link-a got its specific message
    link_repo_a.sync()
    a_info = link_repo_a.revision_info(check=True, no_pager=True)
    assert a_info.message == "Specific A message", (
        f"Expected 'Specific A message', got '{a_info.message}'"
    )

    # Verify link-b fell back to the main message
    link_repo_b.sync()
    b_info = link_repo_b.revision_info(check=True, no_pager=True)
    assert b_info.message == "Main fallback message", (
        f"Expected 'Main fallback message', got '{b_info.message}'"
    )


@pytest.mark.smoke
def test_link_commit_only_main_changes(new_lore_repo):
    """Test that commit with no link changes works normally even with --non-interactive."""
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        "linked",
        {
            "link-file.txt": "link content\n",
        },
        {
            "main.txt": "main content\n",
        },
    )

    # Only modify main file, not link
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main only\n")
    urc.stage(scan=True)

    # Commit with --non-interactive — no link changes so no prompting
    urc.commit("Main only change", non_interactive=True)
    urc.push()

    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Main only change"


@pytest.mark.smoke
def test_link_list_staged(new_lore_repo):
    """Test that urc link list --staged shows linked repos with staged changes."""
    link_path = "linked"
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link content\n",
        },
        {
            "main.txt": "main content\n",
        },
    )

    # Modify files in both repos and stage
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main\n")
    with urc.open_file(os.path.join(link_path, "link-file.txt"), "w+") as f:
        f.write("updated link\n")
    urc.stage(scan=True)

    # link list --staged should show the linked repo with file count
    output = urc.link_list(staged=True)
    assert link_path in output, (
        f"Expected link path '{link_path}' in output, got: {output}"
    )
    assert "file" in output and "changed" in output, (
        f"Expected file count in output, got: {output}"
    )


@pytest.mark.smoke
def test_link_list_staged_no_changes(new_lore_repo):
    """Test that urc link list --staged shows nothing when no links have staged changes."""
    urc, _link_repo = make_parent_with_link(
        new_lore_repo,
        "linked",
        {
            "link-file.txt": "link content\n",
        },
        {
            "main.txt": "main content\n",
        },
    )

    # Only modify main file, not link
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main only\n")
    urc.stage(scan=True)

    # link list --staged should show no links
    output = urc.link_list(staged=True)
    assert "No linked repositories with staged changes" in output, (
        f"Expected no-links message, got: {output}"
    )


@pytest.mark.smoke
def test_link_list_staged_relays_when_the_service_is_in_use(
    new_lore_repo, stops_background_services, global_dir_name
):
    """Listing staged links goes to the service when one is in use, as other
    commands do, rather than opening the repository in the client, where it
    waits on the lock of a service holding the repository. Shown with no
    service reachable: the listing reports that, and so does a commit that
    lists the links to ask for their messages or to check a `--link-message`
    path, stopping at that listing rather than asking about no link or
    rejecting the path."""
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"link-file.txt": "link content\n"})

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})
    urc.link_add("linked", link_repo.get_id(), "/")
    urc.commit("Add link")
    with urc.open_file(os.path.join("linked", "link-file.txt"), "w+") as f:
        f.write("updated link\n")
    urc.stage(scan=True)

    env = urc.sandboxed_env(
        **LORE_SERVICE_ENVIRONMENT,
        LORE_SERVICE_EXECUTABLE=str(Path(global_dir_name) / "no-such-lore-binary"),
    )

    def relayed(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [urc.lore_executable_path, "--repository", urc.path, *args],
            capture_output=True,
            text=True,
            env=env,
            cwd=urc.path,
            stdin=subprocess.DEVNULL,
            check=False,
        )

    listed = relayed("link", "list", "--staged")
    assert listed.returncode == SERVICE_UNAVAILABLE, listed.stdout + listed.stderr

    committed = relayed("commit", "Main message")
    output = committed.stdout + committed.stderr
    assert committed.returncode == SERVICE_UNAVAILABLE, output
    assert "Linked repositories with staged changes" not in committed.stdout, output
    assert output.count("Failed to send command to Lore service") == 1, output

    messaged = relayed(
        "commit", "Main message", "--link-message", "linked", "Linked message"
    )
    output = messaged.stdout + messaged.stderr
    assert messaged.returncode == SERVICE_UNAVAILABLE, output
    assert "does not match" not in output, output


@pytest.mark.smoke
def test_link_list_staged_through_the_service(new_lore_repo, background_lore_service):
    """With the service in use, listing staged links is carried out by the
    service, which holds the repository, rather than by a client that would
    wait on the service's lock. An interactive commit lists them twice: to ask
    for a message per link, and to check each `--link-message` path."""
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"link-file.txt": "link content\n"})

    urc: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})
    link_path = "linked"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")

    with urc.open_file(os.path.join(link_path, "link-file.txt"), "w+") as f:
        f.write("updated link\n")
    urc.stage(scan=True)

    output = urc.link_list(staged=True)
    assert f"{link_path} (1 file changed)" in output, output

    urc.commit("Main message")

    with urc.open_file(os.path.join(link_path, "link-file.txt"), "w+") as f:
        f.write("updated link again\n")
    urc.stage(scan=True)
    urc.commit("Main message", link_messages={link_path: "Link message"})
    assert "No linked repositories with staged changes" in urc.link_list(staged=True)


@pytest.mark.smoke
def test_link_scoped_commit(new_lore_repo):
    """Test committing a single link independently and verifying parent pin is staged."""
    link_path = "linked"
    repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"link-file.txt": "initial link content\n"},
        {"parent-file.txt": "parent content\n"},
    )

    # Modify a file inside the link
    linked_file = f"{link_path}/link-file.txt"
    with repo.open_file(linked_file, "w+") as f:
        f.writelines(["modified link content\n"])

    repo.stage(linked_file)

    # Commit only the link
    output = repo.commit("Link-scoped commit", link=link_path)
    assert "Commit succeeded" in output

    # Parent should show staged changes (the updated link pin)
    status = repo.status()
    assert "Changes staged for commit" in status

    # Commit the parent to finalize
    output = repo.commit("Update link pin")
    assert "Commit succeeded" in output


@pytest.mark.smoke
def test_link_scoped_commit_reports_statistics(new_lore_repo):
    """A commit scoped to a link is a commit, and has to report what it cost. The
    scoped paths return before the one every other commit takes, so a report bound
    to that path alone would leave every link and layer commit silent."""
    link_path = "linked"
    repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"link-file.txt": "initial link content\n"},
        {"parent-file.txt": "parent content\n"},
    )

    linked_file = f"{link_path}/link-file.txt"
    with repo.open_file(linked_file, "w+") as f:
        f.writelines(["modified link content\n"])
    repo.stage(linked_file)

    stats = parse_commit_stats_json(
        repo.commit("Link-scoped commit", link=link_path, json=True, stats=1)
    )
    assert stats is not None, "a link-scoped commit must emit its statistics event"

    files = stats["files"]
    assert files["modified"] == 1, (
        f"the one file changed inside the link was committed as a modification, "
        f"got {files}"
    )
    assert files["files"] == 1, f"and it is the only file committed, got {files}"
    assert stats["fragments"]["fragmentsProduced"] > 0, (
        f"the commit wrote fragments, got {stats['fragments']}"
    )


@pytest.mark.smoke
def test_link_scoped_commit_no_parent_change(new_lore_repo):
    """Test that link-scoped commit preserves parent's own staged changes."""
    link_path = "linked"
    repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"link-file.txt": "initial link content\n"},
        {"parent-file.txt": "parent content\n"},
    )

    # Stage a parent file change
    with repo.open_file("parent-file.txt", "w+") as f:
        f.writelines(["modified parent content\n"])
    repo.stage("parent-file.txt")

    # Also modify a file in the link
    linked_file = f"{link_path}/link-file.txt"
    with repo.open_file(linked_file, "w+") as f:
        f.writelines(["modified link content\n"])
    repo.stage(linked_file)

    # Commit only the link
    output = repo.commit("Link-only commit", link=link_path)
    assert "Commit succeeded" in output

    # Parent should still have staged changes (parent-file.txt + link pin)
    status = repo.status()
    assert "Changes staged for commit" in status
    assert "parent-file.txt" in status

    # Commit the parent — should include both the file change and link pin
    output = repo.commit("Parent commit with file and link pin")
    assert "Commit succeeded" in output


@pytest.mark.smoke
def test_link_scoped_commit_not_a_link(new_lore_repo):
    """Test that --link on a non-link path fails."""
    repo = make_repo(
        new_lore_repo,
        {
            "regular-dir/file.txt": "content\n",
        },
    )

    # Modify a file and stage it
    with repo.open_file("regular-dir/file.txt", "w+") as f:
        f.writelines(["modified\n"])
    repo.stage(scan=True)

    # Try to commit with --link pointing to a regular directory
    with pytest.raises(NotALinkError):
        repo.commit("Should fail", link="regular-dir")


@pytest.mark.smoke
def test_link_scoped_commit_nothing_staged(new_lore_repo):
    """Test that --link with no staged changes in the link fails."""
    link_path = "linked"
    repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "link content\n",
        },
        {
            "parent-file.txt": "parent content\n",
        },
    )

    # No changes in the link — commit should fail
    with pytest.raises(NothingStagedError):
        repo.commit("Should fail", link=link_path)


@pytest.mark.smoke
def test_link_scoped_commit_consecutive(new_lore_repo):
    """Test two consecutive --link commits without committing the parent in between."""
    link_path = "linked"
    repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"link-file.txt": "initial link content\n"},
        {"parent-file.txt": "parent content\n"},
    )

    # First file change inside the link
    with repo.open_file(f"{link_path}/first.txt", "w+") as f:
        f.writelines(["first file\n"])
    repo.stage(f"{link_path}/first.txt")

    output = repo.commit("First link commit", link=link_path)
    assert "Commit succeeded" in output

    # Second file change inside the link — no parent revision in between
    with repo.open_file(f"{link_path}/second.txt", "w+") as f:
        f.writelines(["second file\n"])
    repo.stage(f"{link_path}/second.txt")

    output = repo.commit("Second link commit", link=link_path)
    assert "Commit succeeded" in output

    # Finalize parent
    output = repo.commit("Update link pin")
    assert "Commit succeeded" in output


@pytest.mark.smoke
def test_link_scoped_commit_push_propagates_to_link(new_lore_repo):
    """Pushing after a `commit --link` must push the linked repo's new revision.

    Regression test: `commit --link <path>` creates a new revision in the linked
    repo and advances its branch, but does NOT create a new revision in the parent.
    A subsequent `lore push` from the parent checked only whether the parent had
    new revisions to push; if not, it returned early without walking the link list,
    leaving the new link revision unpushed.
    """
    link_path = "linked"
    repo, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {
            "link-file.txt": "initial link content\n",
        },
        {
            "parent-file.txt": "parent content\n",
        },
    )

    # Snapshot parent's remote latest — should be unchanged after the link-scoped push
    parent_remote_before = repo.branch_info().remote_latest

    # Make a change inside the link, stage and commit with --link only
    with repo.open_file(f"{link_path}/link-file.txt", "w+") as f:
        f.writelines(["updated link content\n"])
    repo.stage(f"{link_path}/link-file.txt")

    link_commit_message = "Link-only update via --link"
    output = repo.commit(link_commit_message, link=link_path)
    assert "Commit succeeded" in output

    # Parent should have no new revision to push (the commit only advanced the link)
    assert repo.branch_info().local_latest == parent_remote_before, (
        "Parent should have no new revision after commit --link"
    )

    # Push from the parent — should propagate the link's new revision to the remote
    repo.push()

    # Pull the link's remote into the standalone clone we made earlier. If the
    # link revision was pushed, its message will now be the linked repo's latest.
    link_repo.sync()
    link_info = link_repo.revision_info(check=True, no_pager=True)
    assert link_info.message == link_commit_message, (
        f"Link revision created by `commit --link` was not pushed to the linked repository's remote. "
        f"Expected message '{link_commit_message}', got '{link_info.message}'"
    )


@pytest.mark.smoke
def test_link_scoped_commit_subdirectory_source_path_translation(new_lore_repo):
    """When a link's source_path is a subdirectory of the source repo (e.g.
    FolderProvidingLink) and the link is mounted under a different name in
    the parent (e.g. FolderReceivingLink), a link-scoped commit (--link
    FolderReceivingLink) for a newly-added file used to fail with:

      Failed writing file FolderProvidingLink/<file> to immutable store:
      ... <link_path>/FolderProvidingLink/<file>: cannot find the path

    The path translation from remote tree path -> local filesystem path
    erroneously concatenated the source folder name onto the local link
    folder, instead of substituting it.
    """
    # Source repo with a subdirectory we will link out of.
    source_repo = make_repo(
        new_lore_repo,
        {
            "FolderProvidingLink/SharedFile.txt": "AAAA\n",
        },
    )

    pinned_revision = source_repo.branch_info().local_latest

    # Receiving repo with a folder that will receive the link, mounted under
    # a different name than the source folder.
    receiving_repo: Lore = new_lore_repo()
    receiving_repo.make_dirs("FolderReceivingLink")
    receiving_repo.stage(scan=True)
    receiving_repo.commit("Create FolderReceivingLink")
    receiving_repo.push()

    receiving_repo.link_add(
        "FolderReceivingLink",
        source_repo.get_id(),
        "FolderProvidingLink",
        pin=pinned_revision,
    )
    receiving_repo.commit("Add link FolderReceivingLink -> FolderProvidingLink")
    receiving_repo.push()

    # Sanity: linked file is mounted directly under the link path, not
    # nested inside FolderProvidingLink.
    assert receiving_repo.file_exists("FolderReceivingLink/SharedFile.txt"), (
        "SharedFile.txt should be mounted directly under FolderReceivingLink"
    )
    assert not receiving_repo.file_exists(
        "FolderReceivingLink/FolderProvidingLink/SharedFile.txt"
    ), "Source folder name must not appear nested inside the link path"

    # Add a new file under the link path and stage it.
    new_file = "FolderReceivingLink/SharedFile2.txt"
    with receiving_repo.open_file(new_file, "w+") as f:
        f.writelines(["BBBB\n"])
    receiving_repo.stage(new_file)

    # Link-scoped commit must succeed. The bug caused this to fail because
    # the commit code looked for the file at
    # FolderReceivingLink/FolderProvidingLink/SharedFile2.txt on disk.
    output = receiving_repo.commit(
        "File added to linked folder", link="FolderReceivingLink"
    )
    assert "Commit succeeded" in output, (
        f"Link-scoped commit should succeed, got output: {output}"
    )

    # Finalize the parent so the new link pin is recorded.
    output = receiving_repo.commit("Update link pin")
    assert "Commit succeeded" in output
    receiving_repo.push()

    # The new file must be reachable through the link in a fresh clone.
    sync_repo = receiving_repo.clone()
    assert sync_repo.file_exists("FolderReceivingLink/SharedFile2.txt"), (
        "Newly committed file must be present under the link in a fresh clone"
    )
    assert sync_repo.file_exists("FolderReceivingLink/SharedFile.txt"), (
        "Original linked file must still be present under the link"
    )
