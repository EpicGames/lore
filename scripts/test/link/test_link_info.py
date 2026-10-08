# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from error_types import NotALinkError

from lore import Lore


@pytest.mark.smoke
def test_link_info_reports_link_details(new_lore_repo):
    """`lore link info <mount_path>` reports the linked repository, its mount
    and source paths, the resolved branch, the pinned revision and no staged
    change for an untouched link.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"sub/link-file.txt": "link content\n"})
    pinned_revision = link_repo.branch_info().local_latest

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/sub")
    urc.commit("Add link")
    urc.push()

    output = urc.link_info(link_path)

    assert link_repo.get_id() in output, (
        f"link info must report the linked repository id.\nOutput:\n{output}"
    )
    assert f"Link path: {link_path}" in output, (
        f"link info must report the mount path.\nOutput:\n{output}"
    )
    assert "Source path: sub" in output, (
        f"link info must report the source path inside the link.\nOutput:\n{output}"
    )
    assert "Branch: main" in output, (
        f"link info must report the resolved branch name.\nOutput:\n{output}"
    )
    assert f"Revision: {pinned_revision}" in output, (
        f"link info must report the pinned revision.\nOutput:\n{output}"
    )
    assert "Flags: None" in output, (
        f"link info must report the link flags.\nOutput:\n{output}"
    )
    assert "Staged: none" in output, (
        f"link info must report no staged change for an untouched link.\nOutput:\n{output}"
    )
    assert "Staged files: 0" in output, (
        f"link info must report zero staged files for an untouched link.\n"
        f"Output:\n{output}"
    )
    assert f"Remote revision: {pinned_revision}" in output, (
        f"link info must report the remote latest revision of the resolved "
        f"branch.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_reports_staged_pin_update(new_lore_repo):
    """`lore link info` reports an uncommitted pin bump as a staged update."""
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("v1", {"link-file.txt": "v1\n"})
    pin_v1 = link_repo.branch_info().local_latest

    link_repo.write_commit_push("v2", {"link-file.txt": "v2\n"})
    pin_v2 = link_repo.branch_info().local_latest

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "linked"
    urc.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    urc.commit("Add link")
    urc.push()

    urc.link_update(link_path, pin=pin_v2)

    output = urc.link_info(link_path)

    assert "Staged: modified" in output, (
        f"link info must report an uncommitted pin bump as a staged update.\n"
        f"Output:\n{output}"
    )
    assert f"Revision: {pin_v2}" in output, (
        f"link info must report the newly staged pin.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_errors_for_non_link_path(new_lore_repo):
    """`lore link info` on a path that is not a link fails with a clear error."""
    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"regular-dir/file.txt": "content\n"})

    with pytest.raises(NotALinkError):
        urc.link_info("regular-dir")


@pytest.mark.smoke
def test_link_info_counts_staged_files_inside_link(new_lore_repo):
    """`lore link info` counts staged files inside the link, not just the pin."""
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push(
        "Initial link", {"sub/one.txt": "one\n", "sub/two.txt": "two\n"}
    )

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/sub")
    urc.commit("Add link")
    urc.push()

    assert "Staged files: 0" in urc.link_info(link_path)

    urc.write_files({f"{link_path}/one.txt": "one modified\n"})
    urc.stage(f"{link_path}/one.txt")

    output = urc.link_info(link_path)

    assert "Staged files: 1" in output, (
        f"link info must count a staged file inside the link.\nOutput:\n{output}"
    )
    assert "Staged: modified" in output, (
        f"a link with staged content inside it must report a staged update.\n"
        f"Output:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_reports_staged_add(new_lore_repo):
    """`lore link info` reports an uncommitted `link add` as a staged addition
    with no staged files inside it yet.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"link-file.txt": "content\n"})

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/")

    output = urc.link_info(link_path)

    assert "Staged: added" in output, (
        f"link info must report an uncommitted link add as a staged addition.\n"
        f"Output:\n{output}"
    )
    assert "Staged files: 0" in output, (
        f"a link staged for addition has no staged files inside it.\nOutput:\n{output}"
    )
    assert "Source path: /" in output, (
        f"link info must report a root source path as `/`.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_reports_staged_removal(new_lore_repo):
    """`lore link info` reports an uncommitted `link remove` as a staged
    removal, describing it from the committed registry entry that staging the
    removal dropped, and does not count files inside a link that is going away.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"link-file.txt": "content\n"})
    pinned_revision = link_repo.branch_info().local_latest

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    urc.link_remove(link_path)

    output = urc.link_info(link_path)

    assert "Staged: removed" in output, (
        f"link info must report an uncommitted link remove as a staged "
        f"removal.\nOutput:\n{output}"
    )
    assert "Staged files: 0" in output, (
        f"a link staged for removal must not count files inside it.\nOutput:\n{output}"
    )
    assert f"Revision: {pinned_revision}" in output, (
        f"link info must still report the pin of a link staged for removal.\n"
        f"Output:\n{output}"
    )
    assert "Branch: main" in output, (
        f"link info must still report the branch of a link staged for removal.\n"
        f"Output:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_omits_remote_revision_when_local(new_lore_repo):
    """`lore link info --local` omits the remote revision rather than reaching
    for the remote, and still reports everything held locally.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"link-file.txt": "content\n"})

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    output = urc.link_info(link_path, local=True)

    assert "Remote revision:" not in output, (
        f"link info --local must not report a remote revision.\nOutput:\n{output}"
    )
    assert link_repo.get_id() in output, (
        f"link info --local must still report the linked repository.\nOutput:\n{output}"
    )
    assert f"Link path: {link_path}" in output, (
        f"link info --local must still report the mount path.\nOutput:\n{output}"
    )
    assert "Staged: none" in output, (
        f"link info --local must still report the staged state.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_distinguishes_tracking_from_pinned_branch(new_lore_repo):
    """`lore link info` distinguishes a link that follows its parent's branch
    from one pinned to an explicit branch.
    """
    tracking_target: Lore = new_lore_repo()
    tracking_target.write_commit_push("Initial", {"a.txt": "a\n"})

    pinned_target: Lore = new_lore_repo()
    pinned_target.write_commit_push("Initial", {"b.txt": "b\n"})

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    urc.link_add("libs/tracking", tracking_target.get_id(), "/")
    urc.link_add("libs/pinned", pinned_target.get_id(), "/", disable_branching=True)
    urc.commit("Add links")
    urc.push()

    tracking_output = urc.link_info("libs/tracking")
    pinned_output = urc.link_info("libs/pinned")

    assert "[tracking]" in tracking_output, (
        f"a link following its parent's branch must report as tracking.\n"
        f"Output:\n{tracking_output}"
    )
    assert "[pinned]" in pinned_output, (
        f"a link with an explicit branch must report as pinned.\n"
        f"Output:\n{pinned_output}"
    )
