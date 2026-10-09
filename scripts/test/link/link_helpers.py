# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Repository setup and output parsing shared by the link tests."""

import re

from lore_parsers import parse_status_json
from test_utils import unstaged_entries

from lore import Lore

DEFAULT_LINK_MOUNT = "vendor/lib"
DEFAULT_PARENT_FILE = "README.txt"


def make_repo(new_lore_repo, files: dict, message: str = "Initial commit") -> Lore:
    """A new repository holding `files`, committed and pushed."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push(message, files)
    return repo


def make_parent_with_link(
    new_lore_repo,
    link_path: str = DEFAULT_LINK_MOUNT,
    link_files: dict | None = None,
    parent_files: dict | None = None,
    source_path: str = "/",
    **link_add_kwargs,
) -> tuple[Lore, Lore]:
    """Returns (parent_repo, link_repo) with the link committed and pushed."""
    parent_repo = make_repo(
        new_lore_repo, parent_files or {DEFAULT_PARENT_FILE: "baseline\n"}, "Baseline"
    )
    link_repo = make_repo(
        new_lore_repo,
        link_files or {"linked.txt": "linked content\n"},
        "Initial linked content",
    )

    parent_repo.link_add(link_path, link_repo.get_id(), source_path, **link_add_kwargs)
    parent_repo.commit("Add link")
    parent_repo.push()
    return parent_repo, link_repo


def link_pin(repo: Lore, source_id: str) -> str:
    """The revision `source_id` is pinned at, read from `link list`."""
    output = repo.link_list()
    match = re.search(rf"Link\s+{source_id}.*?Revision:\s*(\w+)", output, re.DOTALL)
    assert match, f"No pin for {source_id} in link list:\n{output}"
    return match.group(1)


def assert_crr_clean(
    repo: Lore,
    expected_files_present: list[str],
    expected_files_absent: list[str],
    expected_link_registry: dict[str, bool],
):
    """Clean status, realized disk, registry-correct.

    `expected_link_registry` maps a needle (repo id or link path) to whether
    it should appear in `link_list()` output.
    """
    staged = parse_status_json(repo.status(json=True))
    assert staged == [], f"Expected clean staged status, got {staged}"
    unstaged = unstaged_entries(repo)
    assert unstaged == [], f"Expected clean unstaged status, got {unstaged}"
    for path in expected_files_present:
        assert repo.file_exists(path), f"Expected file present: {path}"
    for path in expected_files_absent:
        assert not repo.file_exists(path), f"Expected file absent: {path}"
    link_list_output = repo.link_list()
    for needle, present in expected_link_registry.items():
        if present:
            assert needle in link_list_output, (
                f"Expected {needle!r} in link_list, got: {link_list_output}"
            )
        else:
            assert needle not in link_list_output, (
                f"Expected {needle!r} not in link_list, got: {link_list_output}"
            )


def setup_link_merge_conflict(
    new_lore_repo, link_path="linked/repo", files=None, source_path="/"
):
    """Helper: create main repo + linked repo with conflicting changes on feature branch.

    `files` is a list of dicts with keys: path, base, mine, theirs. Each path is relative to
    what the link exposes, so it is the path below the mount as well; `source_path` is where
    the linked repository itself holds that subtree. Each file will be created with base
    content, then modified on both branches. Returns (urc, link_repo, link_path).
    """
    if files is None:
        files = [
            {
                "path": "data.txt",
                "base": "base\n",
                "mine": "mine\n",
                "theirs": "theirs\n",
            }
        ]

    # Base files sit below the path the link exposes in the linked repository.
    source_prefix = source_path.strip("/")
    urc, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"/".join(filter(None, [source_prefix, f["path"]])): f["base"] for f in files},
        {"main-file.txt": "main repo content\n"},
        source_path=source_path,
    )

    urc.branch_create("feature-branch")
    urc.write_commit_push(
        "Feature branch changes",
        {f"{link_path}/{f['path']}": f["theirs"] for f in files},
    )

    urc.branch_switch("main")
    urc.write_commit_push(
        "Main branch changes",
        {f"{link_path}/{f['path']}": f["mine"] for f in files},
    )

    return urc, link_repo, link_path


ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")
DIFF_ACTIONS = frozenset({"A", "D", "M", "V", "C"})


def parse_revision_diff(output: str) -> list[tuple[str, str]]:
    """Parse `lore revision diff` output into (action, path) pairs.

    The CLI colourizes the action letter, so ANSI escapes are stripped first.
    """
    entries: list[tuple[str, str]] = []
    for raw_line in ANSI_ESCAPE.sub("", output).splitlines():
        parts = raw_line.strip().split(" ", 1)
        if len(parts) == 2 and parts[0] in DIFF_ACTIONS:
            entries.append((parts[0], parts[1].strip()))
    return entries
