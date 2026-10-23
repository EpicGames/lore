# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from link_helpers import make_parent_with_link, make_repo

from lore import Lore


def _remote_branches(repo: Lore) -> list[str]:
    return sorted(repo.run(["branch", "list"], remote=True, no_pager=True).splitlines())


@pytest.mark.smoke
def test_dry_run_branch_create_creates_no_linked_branch(new_lore_repo):
    """`branch create --dry-run` creates no branch in a linked repository, where `branch
    create` without it does."""
    parent, link_repo = make_parent_with_link(new_lore_repo)
    branches = _remote_branches(link_repo)

    parent.branch_create("feature", dry_run=True)
    assert _remote_branches(link_repo) == branches, (
        "a dry run created the linked branch"
    )

    parent.branch_create("feature")
    assert _remote_branches(link_repo) != branches


@pytest.mark.smoke
def test_dry_run_link_add_creates_no_linked_branch(new_lore_repo):
    """`link add --dry-run` on a branch the linked repository lacks creates no branch there,
    where `link add` without it does."""
    parent = make_repo(new_lore_repo, {"a.txt": "a\n"})
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked\n"})
    parent.branch_create("feature")
    parent.write_commit_push("Feature", {"b.txt": "feature\n"})
    branches = _remote_branches(link_repo)

    parent.link_add("lnk", link_repo.get_id(), "/", dry_run=True)
    assert _remote_branches(link_repo) == branches, (
        "a dry run created the linked branch"
    )

    parent.link_add("lnk", link_repo.get_id(), "/")
    assert _remote_branches(link_repo) != branches


@pytest.mark.smoke
def test_dry_run_merge_into_link_pushes_nothing(new_lore_repo):
    """`branch merge into --link --dry-run` leaves the target branch on the remote where it
    was, where `branch merge into --link` moves it."""
    parent, _link_repo = make_parent_with_link(
        new_lore_repo, "lnk", {"linked.txt": "linked\n"}
    )
    parent.branch_create("feature")
    parent.write_files({"lnk/feature.txt": "feature\n"})
    parent.stage(scan=True)
    parent.commit("Feature in the link")
    parent.push()

    def remote_latest() -> list[str]:
        info = parent.run(["branch", "info", "main"], no_pager=True)
        return [line for line in info.splitlines() if "Remote Latest" in line]

    latest = remote_latest()

    parent.branch_merge_into("main", "Merge the link", link="lnk", dry_run=True)
    assert remote_latest() == latest, "a dry run moved the target branch on the remote"

    parent.branch_merge_into("main", "Merge the link", link="lnk")
    assert remote_latest() != latest
