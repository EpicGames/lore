# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os
import re

from lore_parsers import parse_status_json
from test_utils import unstaged_entries

from lore import Lore

# ---------------------------------------------------------------------------
# Helpers for the link-reset tests below.
# ---------------------------------------------------------------------------


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


def commit_initial_main(new_lore_repo, file_name: str) -> Lore:
    """Create a main repo with one committed file and push."""
    repo: Lore = new_lore_repo()
    with repo.open_file(file_name, "w+") as f:
        f.writelines(["main repo initial content\n"])
    repo.stage(scan=True)
    repo.commit("initial main")
    repo.push()
    return repo


def make_link_source(new_lore_repo, file_paths: list[str]) -> tuple[Lore, list[str]]:
    """Create a link source repo with the given files committed and pushed."""
    repo: Lore = new_lore_repo()
    for path in file_paths:
        directory = os.path.dirname(path)
        if directory:
            repo.make_dirs(directory)
        with repo.open_file(path, "w+") as f:
            f.writelines([f"link source content for {path}\n"])
    repo.stage(scan=True)
    repo.commit("initial source")
    repo.push()
    return repo, file_paths


DEFAULT_LINK_MOUNT = "vendor/lib"


DEFAULT_PARENT_FILE = "README.txt"


def make_parent_with_link(
    new_lore_repo,
    link_path: str = DEFAULT_LINK_MOUNT,
    link_files: dict | None = None,
    parent_files: dict | None = None,
    **link_add_kwargs,
):
    """Returns (parent_repo, link_repo) with the link committed and pushed."""
    parent_repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo()

    parent_repo.write_commit_push(
        "Baseline",
        parent_files or {DEFAULT_PARENT_FILE: "baseline\n"},
    )
    link_repo.write_commit_push(
        "Initial linked content", link_files or {"linked.txt": "linked content\n"}
    )

    parent_repo.link_add(link_path, link_repo.get_id(), "/", **link_add_kwargs)
    parent_repo.commit("Add link")
    parent_repo.push()
    return parent_repo, link_repo


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

    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo content\n"])
    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    link_repo = new_lore_repo()

    # Create base files in linked repo, below the path the link exposes
    source_prefix = source_path.strip("/")
    for file_info in files:
        source_file = "/".join(filter(None, [source_prefix, file_info["path"]]))
        dirs = "/".join(source_file.split("/")[:-1])
        if dirs:
            link_repo.make_dirs(dirs)
        with link_repo.open_file(source_file, "w+") as f:
            f.writelines([file_info["base"]])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    urc.link_add(link_path, link_repo.get_id(), source_path, debug=True)
    urc.commit("Add link")
    urc.push()

    # Create feature branch
    urc.branch_create("feature-branch")

    # Feature branch: modify files through mount path
    for file_info in files:
        with urc.open_file(f"{link_path}/{file_info['path']}", "w+") as f:
            f.writelines([file_info["theirs"]])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    # Switch to main: modify same files differently
    urc.branch_switch("main")
    for file_info in files:
        with urc.open_file(f"{link_path}/{file_info['path']}", "w+") as f:
            f.writelines([file_info["mine"]])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

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
