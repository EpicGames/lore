# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Repository setup and output parsing shared by the link tests."""

import os
import re

from lore_parsers import parse_jsonl, parse_status_json
from test_utils import unstaged_entries

from lore import Lore

DEFAULT_LINK_MOUNT = "vendor/lib"
DEFAULT_PARENT_FILE = "README.txt"
MAX_LINK_DEPTH = 8


def make_repo(new_lore_repo, files: dict, message: str = "Initial commit") -> Lore:
    """A new repository holding `files`, committed and pushed."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push(message, files)
    return repo


def make_repo_renumbering_folder(
    new_lore_repo, folder: str = "sub"
) -> tuple[Lore, str, str]:
    """Returns (repo, first_pin, second_pin), where `{folder}/a.txt` reads "one" then "two".

    The second pin recreates `folder` after another, so it is a different node there: a mount
    that follows it by node id rather than by path lands on the other folder.
    """
    repo = make_repo(new_lore_repo, {f"{folder}/a.txt": "one\n"})
    first_pin = repo.branch_info().local_latest
    repo.rmtree(folder)
    repo.stage(scan=True)
    repo.commit(f"Drop {folder}")
    repo.push()
    repo.write_commit_push(
        f"Recreate {folder} after another",
        {"other/z.txt": "z\n", f"{folder}/a.txt": "two\n"},
    )
    return repo, first_pin, repo.branch_info().local_latest


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


def remount(
    repo: Lore,
    link_path: str,
    source_id: str,
    source_path: str,
    message: str | None = "Remount the link",
    pin: str | None = None,
):
    """Re-points the link at `link_path` to `source_path`, at `pin` when given, committed and
    pushed unless `message` is None."""
    repo.link_remove(link_path)
    repo.link_add(link_path, source_id, source_path, pin=pin)
    if message is not None:
        repo.commit(message)
        repo.push()


def make_parent_with_remount(
    new_lore_repo,
    link_files: dict,
    source_path: str,
    remount_path: str,
    link_path: str = DEFAULT_LINK_MOUNT,
    branch: str = "feature-branch",
    link_changes: dict | None = None,
) -> tuple[Lore, Lore]:
    """Returns (parent_repo, link_repo): the link mounts `source_path` on main and is remounted
    onto `remount_path` on `branch`, which is left checked out.

    `link_changes` are written through the mount and committed on `branch` before the remount,
    so the remount can target a subtree only that change brings in.
    """
    parent_repo, link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        link_files,
        {DEFAULT_PARENT_FILE: "baseline\n"},
        source_path=source_path,
    )
    parent_repo.branch_create(branch)
    if link_changes:
        parent_repo.write_commit_push(
            "Change inside the link",
            {f"{link_path}/{path}": content for path, content in link_changes.items()},
        )
    remount(parent_repo, link_path, link_repo.get_id(), remount_path)
    return parent_repo, link_repo


def remote_branch_entries(repo: Lore) -> list[dict]:
    """Remote branch list entries for a repository.

    Archived branches are deliberately not requested. The remote leg reports
    every entry as unarchived and the client asks for archived branches to be
    excluded, so an archived branch is absent from this list entirely rather than
    present with a flag set — its absence is the signal, and asking for archived
    entries would only cost an extra scan.

    Read from JSON rather than the text parser because the text form drops the
    branch id, and the id is what branch creation shares between a parent branch
    and the branch it creates in a linked repository.
    """
    output = repo.branch_list(json=True)
    return [
        e for e in parse_jsonl(output, "branchListEntry") if e["location"] == "remote"
    ]


def link_pin(repo: Lore, source_id: str) -> str:
    """The revision `source_id` is pinned at, read from `link list`."""
    output = repo.link_list()
    match = re.search(rf"Link\s+{source_id}.*?Revision:\s*(\w+)", output, re.DOTALL)
    assert match, f"No pin for {source_id} in link list:\n{output}"
    return match.group(1)


def committed_link_pin(repo: Lore, source_id: str) -> str:
    """The pin `repo`'s latest pushed revision holds, which `link list` on `repo` itself does not
    show while the staged state repins the link."""
    return link_pin(repo.clone(), source_id)


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


def assert_mount_source(repo: Lore, link_path: str, source_path: str):
    """The link at `link_path` names `source_path` as what it exposes."""
    info = repo.link_info(link_path)
    assert f"Source path: {source_path}" in info, (
        f"Link at {link_path} should mount {source_path!r}.\nGot:\n{info}"
    )


def assert_mount_exposes(
    repo: Lore, link_path: str, source_path: str, expected_names: list[str]
):
    """The link at `link_path` mounts `source_path` and nothing else."""
    assert_mount_source(repo, link_path, source_path)
    mounted = sorted(os.listdir(os.path.join(repo.path, link_path)))
    assert mounted == sorted(expected_names), (
        f"Mount {link_path} should expose exactly {sorted(expected_names)}, "
        f"got: {mounted}"
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


def build_nested_link_repos_at(new_lore_repo, b_mount, c_inner_mount):
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


def build_link_chain(new_lore_repo, links: int) -> tuple[Lore, str]:
    """A parent holding `links` links, each mounted inside the one before, every linked
    repository holding `a.txt` and `b.txt`. Returns (parent_repo, deepest_mount)."""
    parent_repo = make_repo(new_lore_repo, {DEFAULT_PARENT_FILE: "baseline\n"})
    mount = ""
    for level in range(1, links + 1):
        linked = make_repo(new_lore_repo, {"a.txt": "a\n", "b.txt": "b\n"})
        mount = f"{mount}/l{level}".lstrip("/")
        parent_repo.link_add(mount, linked.get_id(), "/")
        parent_repo.commit(f"Link level {level}")
        parent_repo.push()
    return parent_repo, mount
