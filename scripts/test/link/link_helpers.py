# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Repository setup and output parsing shared by the link tests."""

import os
import re
from collections.abc import Callable
from dataclasses import dataclass

import pytest
from error_types import OverlappingLinkError, UnresolvedConflictError
from lore_parsers import parse_jsonl, parse_status_json, parse_status_summary_json
from test_utils import (
    current_revision,
    link_entries,
    read_text,
    registered_links,
    source_paths_by_link_path,
    staged_revision,
    status_paths,
    unstaged_entries,
    working_tree_files,
)

from thin_client import NODE_TYPE_LINK

from lore import Lore

DEFAULT_LINK_MOUNT = "vendor/lib"
DEFAULT_PARENT_FILE = "README.txt"
DEFAULT_LINKED_FILE = f"{DEFAULT_LINK_MOUNT}/linked.txt"
DEFAULT_LINKED_CONTENT = "linked content\n"
MAX_LINK_DEPTH = 8
SHARED_FOLDER = {"shared/a.txt": "a original\n", "shared/b.txt": "b original\n"}


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
    delete_commit_push(repo, folder)
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
    commit: bool = True,
    **link_add_kwargs,
) -> tuple[Lore, Lore]:
    """Returns (parent_repo, link_repo) with the link committed and pushed, or only staged by
    `link add` where `commit` is false."""
    parent_repo, (link_repo,) = make_parent_with_links(
        new_lore_repo,
        {link_path: link_files or {"linked.txt": DEFAULT_LINKED_CONTENT}},
        parent_files,
        source_path,
        commit,
        **link_add_kwargs,
    )
    return parent_repo, link_repo


def make_parent_with_links(
    new_lore_repo,
    links: dict[str, dict[str, str]],
    parent_files: dict | None = None,
    source_path: str = "/",
    commit: bool = True,
    **link_add_kwargs,
) -> tuple[Lore, list[Lore]]:
    """Returns (parent_repo, link_repos): the parent links `source_path` of a new repository
    holding `links[mount]` at each mount, in order, in one commit it pushes, or only staged by
    `link add` where `commit` is false."""
    parent_repo = make_repo(
        new_lore_repo, parent_files or {DEFAULT_PARENT_FILE: "baseline\n"}, "Baseline"
    )
    link_repos = []
    for mount, files in links.items():
        link_repo = make_repo(new_lore_repo, files, "Initial linked content")
        parent_repo.link_add(mount, link_repo.get_id(), source_path, **link_add_kwargs)
        link_repos.append(link_repo)
    if commit:
        parent_repo.commit("Add links")
        parent_repo.push()
    return parent_repo, link_repos


def link_added_on_feature_branch(
    new_lore_repo,
    link_path: str,
    outer: str = "",
    link_file: str | bytes = "link source content\n",
    link_files: dict | None = None,
    **link_add_options,
) -> tuple[Lore, Lore]:
    """Parent repo on `feature-branch`, link added and committed there only, to a repository
    holding `link_files`, or `link_file` as `link-file.txt`. `outer` links a repository there
    first, which `link_path` is in."""
    if outer:
        repo, _ = make_parent_with_link(new_lore_repo, outer)
    else:
        repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, link_files or {"link-file.txt": link_file})

    repo.branch_create("feature-branch")
    add_a_link(repo, source_repo, link_path=link_path, **link_add_options)
    return repo, source_repo


def delete_commit_push(repo: Lore, path: str, message: str | None = None):
    repo.rmtree(path)
    repo.stage(path, scan=True)
    repo.commit(message or f"Delete {path}")
    repo.push()


def diverge(
    repo: Lore,
    feature: dict,
    main: dict,
    feature_message: str = "Feature changes",
    main_message: str = "Main changes",
):
    """Commits `feature` on a new `feature-branch` and `main` on main, pushing both, and leaves
    main checked out."""
    repo.branch_create("feature-branch")
    repo.write_commit_push(feature_message, feature)
    repo.branch_switch("main")
    repo.write_commit_push(main_message, main)


def merge_main_into_feature(repo: Lore):
    """Checks out `feature-branch`, merges main into it, and pushes the merge."""
    repo.branch_switch("feature-branch")
    repo.branch_merge_start("main", message="Merge main")
    repo.push()


def update_link(
    repo: Lore, link_path: str, message: str = "Update the link", **link_update_kwargs
):
    """Commits the link at `link_path` updated by `link update`, and pushes."""
    repo.link_update(link_path, **link_update_kwargs)
    repo.commit(message)
    repo.push()


def remove_link(repo: Lore, link_path: str, message: str = "Remove the link"):
    """Commits the link at `link_path` removed, and pushes."""
    repo.link_remove(link_path)
    repo.commit(message)
    repo.push()


def stage_a_parent_edit(repo: Lore):
    """Stages a file of the parent's own, outside any link."""
    repo.write_files({"staged.txt": "staged\n"})
    repo.stage("staged.txt")


def remount(
    repo: Lore,
    link_path: str,
    source_id: str,
    source_path: str,
    message: str | None = "Remount the link",
    pin: str | None = None,
    **link_add_kwargs,
):
    """Re-points the link at `link_path` to `source_path`, at `pin` when given, committed and
    pushed unless `message` is None."""
    repo.link_remove(link_path)
    repo.link_add(link_path, source_id, source_path, pin=pin, **link_add_kwargs)
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
    """Remote branch list entries for a repository. An archived branch is absent
    from the list.

    Read from JSON rather than the text parser because the text form drops the
    branch id, and the id is what branch creation shares between a parent branch
    and the branch it creates in a linked repository.
    """
    return parse_jsonl(repo.branch_list(remote=True, json=True), "branchListEntry")


def error_line(failure: pytest.ExceptionInfo) -> str:
    """The first error line a failed command printed."""
    output = str(failure.value)
    line = next(
        (line for line in output.splitlines() if line.startswith("[Error]")), None
    )
    assert line is not None, f"No [Error] line in: {output}"
    return line


def remote_branch_names(repo: Lore) -> list[str]:
    """The sorted names of a repository's remote branches."""
    return sorted(remote_branches_by_name(repo))


def remote_branches_by_name(repo: Lore) -> dict[str, dict]:
    """A repository's remote branch list entries, by branch name."""
    return {entry["name"]: entry for entry in remote_branch_entries(repo)}


def layer_targets(repo: Lore) -> set[str]:
    """The target paths of the layers `layer list` shows."""
    return {
        line.split("-> ", 1)[1].strip()
        for line in repo.layer_list().splitlines()
        if "-> " in line
    }


def link_entry(repo: Lore, link_path: str) -> dict:
    """The `link list` entry of the link at `link_path`."""
    return next(e for e in link_entries(repo) if e["linkPath"] == link_path)


def with_another_scheme(url: str) -> str:
    """`url` under a scheme other than its own."""
    scheme, rest = url.split("://", 1)
    return f"{'lores' if scheme == 'lore' else 'lore'}://{rest}"


def link_field(repo: Lore, source_id: str, field: str) -> str:
    """The first word of `field` in the `link list` entry for `source_id`."""
    output = repo.link_list()
    match = re.search(rf"Link\s+{source_id}.*?{field}:\s*(\w+)", output, re.DOTALL)
    assert match, f"No {field} for {source_id} in link list:\n{output}"
    return match.group(1)


def link_pins(repo: Lore, source_id: str) -> list[str]:
    """The revisions the links to `source_id` pin, read from `link list`."""
    return [e["revision"] for e in link_entries(repo) if e["link"] == source_id]


def link_pin(repo: Lore, source_id: str) -> str:
    """The revision `source_id` is pinned at, read from `link list`."""
    pins = link_pins(repo, source_id)
    assert pins, f"No pin for {source_id} in link list"
    return pins[0]


def link_pin_at(repo: Lore, link_path: str) -> str:
    """The revision the link at `link_path` pins, read from `link list`."""
    return link_entry(repo, link_path)["revision"]


def link_tracks(repo: Lore, link_path: str) -> bool:
    return bool(link_entry(repo, link_path)["tracking"])


def assert_pinned_at_linked_main(repo: Lore, source_repo: Lore):
    """The link to `source_repo` pins the latest revision of its remote main."""
    assert link_pin(repo, source_repo.get_id()) == (
        source_repo.branch_info("main").remote_latest
    ), "The link should pin the latest of the linked repository's main"


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


def assert_files_hold(repo: Lore, files: dict[str, str]):
    """Each of `files` holds its content, whatever else the repository holds."""
    assert files.items() <= repo.read_files().items()


def assert_conflict_marked(repo: Lore, path: str):
    """`path` holds the markers of a conflict a merge left in it."""
    assert "<<<<<<<" in read_text(repo, path), f"{path} should hold conflict markers"


def fail_unless_holds(repo: Lore, path: str, content: str):
    """Fails the test unless `path` holds `content`, with `pytest.fail` rather than an
    `AssertionError`, so a test marked `xfail(raises=AssertionError)` still fails on it."""
    held = read_text(repo, path)
    if held != content:
        pytest.fail(f"{path} must hold {content!r}, got {held!r}")


def folder_replaced_by_a_link(new_lore_repo, **link_add_options) -> tuple[Lore, Lore]:
    """Parent on `main` holding `SHARED_FOLDER`, plus a `feature` branch where a link
    replaced that folder with a repository of its own, with `feature` checked out.

    The linked copy is byte-identical to the folder it replaces, which is what
    moving a folder out into its own repository leaves behind.
    """
    parent = make_repo(new_lore_repo, {"root.txt": "root\n", **SHARED_FOLDER})
    source = make_repo(new_lore_repo, in_the_shared_mount(SHARED_FOLDER))

    parent.branch_create("feature")
    replace_folder_with_a_link(parent, source, **link_add_options)

    return parent, source


def replace_folder_with_a_link(
    repo: Lore, source: Lore, source_path: str = "/", **link_add_options
):
    """Commits `shared/` replaced with a link to `source_path` of `source`, and pushes."""
    repo.rmtree("shared")
    repo.stage("shared", scan=True)
    add_a_link(repo, source, source_path, **link_add_options)


def add_a_link(
    repo: Lore,
    source: Lore,
    source_path: str = "/",
    link_path: str = "shared",
    message: str | None = None,
    **link_add_options,
):
    """Commits a link at `link_path` to `source_path` of `source`, and pushes."""
    repo.link_add(link_path, source.get_id(), source_path, **link_add_options)
    repo.commit(message or f"Link {link_path}/")
    repo.push()


def link_replaced_by_its_folder(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent on `main` holding a link at `shared/`, plus a `feature` branch where
    a folder of the link's content replaced it, with `main` checked out."""
    parent, source = make_parent_with_link(
        new_lore_repo, "shared", in_the_shared_mount(SHARED_FOLDER)
    )
    parent.branch_create("feature")
    replace_link_with_folder(parent)
    parent.branch_switch("main")
    return parent, source


def in_the_shared_mount(files: dict[str, str]) -> dict[str, str]:
    """`files` below `shared/`, by their paths in a mount there."""
    return {path.removeprefix("shared/"): content for path, content in files.items()}


def replace_link_with_folder(repo: Lore, files: dict[str, str] = SHARED_FOLDER):
    """Commits the link at `shared/` removed, then `files` in its place, and pushes."""
    repo.link_remove("shared")
    repo.commit("Remove the link")
    repo.write_commit_push("Replace the link with its folder", files)


def change_inside_the_folder(repo: Lore) -> dict[str, str]:
    """Commits a file of `shared/` modified and another added, and returns what
    `shared/` holds."""
    changes = {
        "shared/a.txt": "a changed on main\n",
        "shared/c.txt": "c added on main\n",
    }
    repo.write_commit_push("Change files under shared/", changes)
    return {**SHARED_FOLDER, **changes}


def add_inside_the_folder(repo: Lore) -> dict[str, str]:
    """Commits a file added under `shared/`, and returns what `shared/` holds."""
    added = {"shared/c.txt": "c\n"}
    repo.write_commit_push("Add a file under shared/", added)
    return {**SHARED_FOLDER, **added}


def delete_inside_the_folder(repo: Lore) -> dict[str, str]:
    """Commits a file deleted under `shared/`, and returns what `shared/` holds."""
    repo.remove_file("shared/b.txt")
    repo.stage("shared/b.txt")
    repo.commit("Delete a file under shared/")
    repo.push()
    return {"shared/a.txt": SHARED_FOLDER["shared/a.txt"]}


def delete_the_folder(repo: Lore) -> dict[str, str]:
    """Commits `shared/` deleted, and returns what `shared/` holds."""
    delete_commit_push(repo, "shared")
    return {}


def assert_holds_folder(repo: Lore, files: dict[str, str] = SHARED_FOLDER):
    """`shared/` holds exactly `files`, and nothing is reported as changed."""
    assert working_tree_files(repo, "shared") == sorted(files), (
        "shared/ must hold the folder's files"
    )
    for path, content in files.items():
        assert read_text(repo, path) == content, (
            f"{path} must hold the folder's content"
        )
    assert parse_status_json(repo.status(json=True)) == [], "nothing may be staged"
    assert unstaged_entries(repo) == [], "the file system must match the revision"


def assert_holds_link(repo: Lore, source: Lore, pin: str | None = None):
    """`shared/` is the link to `source`, at `pin` when given, serving its content."""
    info = repo.link_info("shared")
    assert source.get_id() in info, f"shared must link the repository:\n{info}"
    assert "Link path: shared" in info, f"shared must be a link:\n{info}"
    if pin:
        assert f"Revision: {pin}" in info, f"shared must hold its pin:\n{info}"
    assert repo.compare_file(source, "shared/b.txt", "b.txt"), (
        "the mount must serve the linked repository's content"
    )
    assert unstaged_entries(repo) == [], "the file system must match the revision"


def assert_merge_settled(repo: Lore, merged: str, assert_holds: Callable[[Lore], None]):
    """Pushes the merge of `merged` `repo` committed, which `assert_holds` checks `repo` and a
    fresh clone of its branch hold; the revision verifies, and merging `merged` again leaves
    nothing to merge."""
    repo.push()
    assert_holds(repo)
    assert "Verified repository state integrity" in repo.repository_verify()
    assert_holds(repo.clone(branch=repo.branch_info().name))
    again = repo.branch_merge_start(merged, message="Merge again")
    assert "0 updated, 0 deleted, 0 merged, 0 conflicted" in again, (
        f"a merge after the resolve must have nothing left to merge:\n{again}"
    )


def mount_listing(repo: Lore, link_path: str) -> list[str]:
    """The names the mount at `link_path` holds at its top, sorted."""
    return sorted(os.listdir(os.path.join(repo.path, link_path)))


def mounted_files(repo: Lore, link_path: str) -> dict[str, str]:
    """The files the mount at `link_path` holds at its top, with their contents."""
    files = {}
    for entry in os.scandir(os.path.join(repo.path, link_path)):
        if entry.is_file():
            with open(entry.path) as f:
                files[entry.name] = f.read()
    return files


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
    mounted = mount_listing(repo, link_path)
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

    diverge(
        urc,
        {f"{link_path}/{f['path']}": f["theirs"] for f in files},
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


def changed_paths(diff_output: str) -> list[str]:
    """The working tree paths `lore file diff` output reports changed, sorted."""
    return sorted(
        line[4:]
        for line in diff_output.splitlines()
        if line.startswith("+++ ") and line != "+++ /dev/null"
    )


def revision_delta(
    repo: Lore, revision: str | None = None
) -> list[tuple[str, str, bool]]:
    """The `revision info --delta` entries of `revision` as sorted (path, action, flagModify)."""
    deltas = parse_jsonl(
        repo.revision_info(revision, delta=True, json=True), "revisionInfoDelta"
    )
    return sorted(
        (delta["path"], delta["action"], delta["flagModify"]) for delta in deltas
    )


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

    add_a_link(repo_a, repo_b, link_path=b_mount)
    nested_mount = f"{b_mount}/{c_inner_mount}"
    add_a_link(repo_a, repo_c, link_path=nested_mount)

    return repo_a, repo_b, repo_c, b_mount, nested_mount


def build_link_chain(new_lore_repo, links: int) -> tuple[Lore, str]:
    """A parent holding `links` links, each mounted inside the one before, every linked
    repository holding `a.txt` and `b.txt`. Returns (parent_repo, deepest_mount)."""
    parent_repo = make_repo(new_lore_repo, {DEFAULT_PARENT_FILE: "baseline\n"})
    mount = ""
    for level in range(1, links + 1):
        linked = make_repo(new_lore_repo, {"a.txt": "a\n", "b.txt": "b\n"})
        mount = f"{mount}/l{level}".lstrip("/")
        add_a_link(parent_repo, linked, link_path=mount)
    return parent_repo, mount


def parent_with_a_link_merging_into_a_failure(new_lore_repo) -> tuple[Lore, Lore]:
    """Returns (urc, nested) once `feature-branch` and main each add a file to the linked
    repository at `libs/first` and change its `data.txt`, and main adds a file to the one at
    `libs/other`, changes its `other.txt`, and mounts all of `nested` inside the link
    `libs/second`, with main checked out."""
    urc, _ = make_parent_with_links(
        new_lore_repo,
        {
            "libs/first": {"data.txt": "base\n"},
            "libs/other": {"other.txt": "other\n"},
            "libs/second": {"second.txt": "second\n"},
        },
        {"main-file.txt": "main base\n"},
    )
    nested = make_repo(new_lore_repo, {"sub/nested.txt": "nested\n"})

    def changes(branch: str) -> dict[str, str]:
        return {
            f"libs/first/{branch}.txt": f"{branch}\n",
            "libs/first/data.txt": f"{branch}\n",
        }

    diverge(
        urc,
        changes("feature"),
        {
            **changes("main"),
            "libs/other/main.txt": "main\n",
            "libs/other/other.txt": "main\n",
        },
    )
    add_a_link(urc, nested, link_path="libs/second/whole")
    return urc, nested


def mount_part_inside_the_second_link_on_feature(urc: Lore, nested: Lore) -> Lore:
    """Mounts part of `nested` inside `libs/second` on `feature-branch`, which nests with the
    mount main holds there, so merging the link fails once the merge writes it. Returns the
    clone of `feature-branch` that pushed it."""
    feature = urc.clone(branch="feature-branch")
    add_a_link(feature, nested, "sub", link_path="libs/second/part")
    return feature


def unmount_the_part_inside_the_second_link(feature: Lore):
    remove_link(feature, "libs/second/part")


def start_the_merge_failing_at_the_second_link(urc: Lore):
    staged = staged_revision(urc)
    with pytest.raises(OverlappingLinkError):
        urc.branch_merge_start("feature-branch", message="Merge feature-branch")
    assert staged_revision(urc) != staged, (
        "the failed merge should leave the merge of a link before the second staged"
    )


def resolve_the_only_conflict(urc: Lore, path: str):
    assert urc.unresolved_paths() == {path}
    urc.write_files({path: "resolved\n"})
    urc.branch_merge_resolve(path)


def assert_link_holds(repo: Lore, link_path: str, files: dict[str, str]):
    """The link at `link_path` exposes exactly `files`, with their contents."""
    assert mount_listing(repo, link_path) == sorted(files)
    assert mounted_files(repo, link_path) == files


def commit_through_mount(
    repo: Lore, files: dict[str, str], link_path: str = DEFAULT_LINK_MOUNT
):
    """Writes `files` under the link at `link_path` and commits them, which commits the linked
    repository locally and pins the link to that unpushed revision."""
    paths = {f"{link_path}/{path}": content for path, content in files.items()}
    repo.write_files(paths)
    repo.stage(list(paths))
    repo.commit("Edit through the mount")


SUBTREE_LINK = "linked/repo"


def subtree_link_with_outside_change(new_lore_repo, inside_change, conflicting):
    """Returns (parent, linked), parent on main: the link mounts `content/assets`, and the
    linked repository's feature branch changes `config/settings.txt`, outside it.

    The parent's feature branch changes `a.txt` through the mount where `inside_change`, and
    `root.txt` otherwise. Where `conflicting`, the linked repository's main branch changes
    `config/settings.txt` too.
    """
    parent, linked = make_parent_with_link(
        new_lore_repo,
        SUBTREE_LINK,
        {"content/assets/a.txt": "a base\n", "config/settings.txt": "settings base\n"},
        {"root.txt": "root\n"},
        source_path="content/assets",
    )
    parent.branch_create("feature")
    parent.write_commit_push(
        "Feature change",
        {f"{SUBTREE_LINK}/a.txt": "a on feature\n"}
        if inside_change
        else {"root.txt": "root on feature\n"},
    )
    linked.branch_switch("feature")
    linked.write_commit_push(
        "Outside change on feature", {"config/settings.txt": "settings on feature\n"}
    )
    update_link(parent, SUBTREE_LINK)
    parent.branch_switch("main")
    if conflicting:
        linked.branch_switch("main")
        linked.write_commit_push(
            "Outside change on main", {"config/settings.txt": "settings on main\n"}
        )
        update_link(parent, SUBTREE_LINK)
    return parent, linked


def assert_linked_main_merged_settings(parent: Lore, linked: Lore):
    """Linked main's latest is a merge holding feature's `config/settings.txt`, and it is the
    revision the parent's latest pins."""
    linked.branch_switch("main")
    merged = linked.revision_info()
    assert len(merged.merge.split()) == 2, (
        f"linked main must take a merge revision, got {merged}"
    )
    assert read_text(linked, "config/settings.txt") == "settings on feature\n"
    assert committed_link_pin(parent, linked.get_id()) == merged.signature


MERGE_LINKS = ["linked/repo", "linked/other"]


def diverged_link_branches(new_lore_repo):
    """Returns (urc, link_repos): `urc` links a repository at each of `MERGE_LINKS`, and
    `feature-branch` and main each add a file to the parent and through every link, with main
    checked out."""
    urc, link_repos = make_parent_with_links(
        new_lore_repo,
        {
            link_path: {"linked.txt": DEFAULT_LINKED_CONTENT}
            for link_path in MERGE_LINKS
        },
    )
    diverge(
        urc,
        {"feature.txt": "feature\n"}
        | {f"{link_path}/feature.txt": "feature\n" for link_path in MERGE_LINKS},
        {"main.txt": "main\n"}
        | {f"{link_path}/main.txt": "main\n" for link_path in MERGE_LINKS},
    )
    return urc, link_repos


def repo_holding_links(new_lore_repo, outer: str) -> tuple[Lore, str]:
    """Returns (urc, prefix): a repository holding `main-file.txt`, where the links a test adds
    below `prefix` go. Where `outer` is set, it holds `main-file.txt` in a repository it links
    there, so the links the test adds are nested in that link."""
    if not outer:
        return make_repo(new_lore_repo, {"main-file.txt": "main base\n"}), ""
    urc, _ = make_parent_with_link(
        new_lore_repo, outer, {"main-file.txt": "main base\n"}
    )
    return urc, f"{outer}/"


def merge_a_branch_removing_a_link(
    new_lore_repo,
    added: bool,
    pinned: bool,
    conflict: str | None = None,
    link_messages: dict[str, str] | None = None,
    outer: str = "",
) -> tuple[Lore, Lore]:
    """Returns (urc, link_repo) once main merges `feature-branch`, which removes the link at
    `libs/old` while each branch changes the linked repository through it, and links the
    repository again at `libs/new` where `added`.

    `pinned` links the first revision of the linked repository at both paths rather than its
    latest. `conflict` names the file both branches change apart from that: `main-file.txt`, or
    `shared.txt` inside the link. `link_messages` passes per-link messages to the merge. `outer`
    nests every path in a link mounted there, as `repo_holding_links` describes.
    """
    urc, p = repo_holding_links(new_lore_repo, outer)
    link_repo = make_repo(
        new_lore_repo, {"linked.txt": "linked base\n", "shared.txt": "shared base\n"}
    )
    first = link_repo.branch_info().local_latest
    add_a_link(urc, link_repo, link_path=f"{p}libs/old", pin=first if pinned else None)

    def changes(branch: str) -> dict[str, str]:
        files = {f"{p}libs/old/{branch}.txt": f"{branch}\n"}
        if conflict == "main-file.txt":
            files[f"{p}main-file.txt"] = f"{branch}\n"
        elif conflict == "shared.txt":
            files[f"{p}libs/old/shared.txt"] = f"{branch}\n"
        return files

    urc.branch_create("feature-branch")
    urc.write_commit_push("Feature changes the link", changes("feature"))
    urc.link_remove(f"{p}libs/old")
    if added:
        urc.link_add(
            f"{p}libs/new", link_repo.get_id(), "/", pin=first if pinned else None
        )
    urc.commit("Feature removes the link")
    urc.push()

    urc.branch_switch("main")
    urc.write_commit_push("Main changes the link", changes("main"))

    urc.branch_merge(
        "feature-branch", message="Merge feature-branch", link_messages=link_messages
    )
    return urc, link_repo


def merge_a_branch_moving_a_link(
    new_lore_repo,
    pinned: bool,
    moved: tuple[str, str],
    conflict: bool = False,
    outer: str = "",
) -> tuple[Lore, Lore]:
    """Returns (urc, link_repo) once main merges `feature-branch`, which moves `moved[0]`, the
    link `libs/old` or the directory `libs` holding it, to `moved[1]`, while each branch adds a
    file to the linked repository through the link. `pinned` links the first revision of the
    linked repository rather than its latest, and `conflict` changes `main-file.txt` on both
    branches. `outer` nests every path in a link mounted there, as `repo_holding_links`
    describes."""
    urc, p = repo_holding_links(new_lore_repo, outer)
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked base\n"})
    first = link_repo.branch_info().local_latest
    add_a_link(urc, link_repo, link_path=f"{p}libs/old", pin=first if pinned else None)

    def changes(branch: str) -> dict[str, str]:
        files = {f"{p}libs/old/{branch}.txt": f"{branch}\n"}
        if conflict:
            files[f"{p}main-file.txt"] = f"{branch}\n"
        return files

    urc.branch_create("feature-branch")
    urc.write_commit_push("Feature changes the link", changes("feature"))
    urc.move(f"{p}{moved[0]}", f"{p}{moved[1]}")
    if moved[0] == "libs/old" or p:
        # `lore stage move` refuses a link and a path inside one, and a scan stages a rename in
        # case.
        urc.stage(scan=True, case="rename")
    else:
        urc.stage_move(*moved)
    urc.commit("Feature moves the link")
    urc.push()

    urc.branch_switch("main")
    urc.write_commit_push("Main changes the link", changes("main"))

    urc.branch_merge("feature-branch", message="Merge feature-branch")
    return urc, link_repo


#: How the merged branch moves the link `libs/old`: what it moves, where to, where the link ends
#: up, and the files the link exposes after the merge.
LINK_MOVES = {
    "link-in-case": (
        ("libs/old", "libs/Old"),
        "libs/Old",
        ["feature.txt", "linked.txt", "main.txt"],
    ),
    "directory-in-case": (
        ("libs", "Libs"),
        "Libs/old",
        ["feature.txt", "linked.txt", "main.txt"],
    ),
    "directory-elsewhere": (
        ("libs", "vendor"),
        "vendor/old",
        ["feature.txt", "linked.txt", "main.txt"],
    ),
}


#: Two links reaching one repository: nested in two linked repositories, or held by the main
#: repository at two folders of the repository.
LINKS_TO_ONE_REPOSITORY = {
    "nested": ("vendor/b/deps/d", "vendor/c/deps/d"),
    "subtrees": ("libs/a", "libs/b"),
}


def links_to_one_repository(new_lore_repo, shape: str) -> tuple[Lore, Lore, list[Lore]]:
    """Returns (urc, d, holders): `urc` links `d` at both mounts of `LINKS_TO_ONE_REPOSITORY`
    under `shape`, at one pin, through `holders`, the repositories linked at the mounts' parents
    where the links are nested. Each mount holds `d.txt`."""
    mounts = LINKS_TO_ONE_REPOSITORY[shape]
    subtrees = shape == "subtrees"
    d = make_repo(
        new_lore_repo,
        {"a/d.txt": "base\n", "b/d.txt": "base\n", "c/d.txt": "base\n"}
        if subtrees
        else {"d.txt": "base\n"},
    )
    urc = make_repo(new_lore_repo, {"a.txt": "base\n"})
    holders = []
    if shape == "nested":
        for mount in mounts:
            holder = make_repo(new_lore_repo, {"holder.txt": "base\n"})
            urc.link_add(mount.removesuffix("/deps/d"), holder.get_id(), "/")
            holders.append(holder)
        urc.commit("Link the holders")
        urc.push()
    for mount in mounts:
        urc.link_add(mount, d.get_id(), mount.rsplit("/", 1)[1] if subtrees else "/")
    urc.commit("Link d twice")
    urc.push()
    return urc, d, holders


def mapped_links_in_mapped_links(
    new_lore_repo, fixed: bool = False
) -> tuple[Lore, Lore, Lore]:
    """Returns (urc, n, pn): `urc` links `n` at `n1`, exposing `f1`, and at `n2`, exposing `f2`,
    and `n` links `pn` at `f1/pn`, exposing `x`, and at `f2/pn`, exposing `y`, branching disabled
    where `fixed`, so the working tree holds `pn.txt` of each of `pn`'s folders at `n1/pn` and
    `n2/pn`."""
    pn = make_repo(new_lore_repo, {"x/pn.txt": "base\n", "y/pn.txt": "base\n"})
    n = make_repo(new_lore_repo, {"f1/n.txt": "base\n", "f2/n.txt": "base\n"})
    for folder, source in (("f1", "x"), ("f2", "y")):
        n.link_add(f"{folder}/pn", pn.get_id(), source, disable_branching=fixed)
    n.commit("Link pn twice")
    n.push()
    urc = make_repo(new_lore_repo, {"a.txt": "base\n"})
    for mount, source in (("n1", "f1"), ("n2", "f2")):
        urc.link_add(mount, n.get_id(), source)
    urc.commit("Link n twice")
    urc.push()
    return urc, n, pn


def change_through_both_links(urc: Lore, shape: str, files: dict[str, str]):
    """Commits `files` inside the repository both links of `shape` reach, through the first, and
    pins the second at the result."""
    first, second = LINKS_TO_ONE_REPOSITORY[shape]
    urc.write_commit_push(
        "Change d", {f"{first}/{path}": c for path, c in files.items()}
    )
    # Mounts in one repository move together; nested ones live in two holders.
    if shape == "nested":
        update_link(urc, second, pin=link_pin_at(urc, first))


def diverge_one_repository_two_links_reach(urc, shape: str, a_conflict: bool = False):
    """Changes the repository both links of `shape` reach on `feature` and on main, and `a.txt` on
    both where `a_conflict`, leaving main checked out."""
    urc.branch_create("feature")
    change_through_both_links(
        urc, shape, {"d.txt": "feature\n", "feature.txt": "feature\n"}
    )
    if a_conflict:
        urc.write_commit_push("Feature changes a", {"a.txt": "feature\n"})
    urc.branch_switch("main")
    change_through_both_links(urc, shape, {"main.txt": "main\n"})
    if a_conflict:
        urc.write_commit_push("Main changes a", {"a.txt": "main\n"})


def diverge_both_mounts(urc: Lore, conflicted: list[str]) -> list[str]:
    """Changes `d.txt` under each of `conflicted`, mounts of the "subtrees" shape, differently on
    `feature` and on main, and `feature.txt` under the first mount on `feature`, leaving main
    checked out. Returns the conflicted paths."""
    first, _ = LINKS_TO_ONE_REPOSITORY["subtrees"]
    urc.branch_create("feature")
    urc.write_commit_push(
        "Change d on feature",
        {f"{first}/feature.txt": "feature\n"}
        | {f"{mount}/d.txt": "feature\n" for mount in conflicted},
    )
    urc.branch_switch("main")
    urc.write_commit_push(
        "Change d on main", {f"{mount}/d.txt": "main\n" for mount in conflicted}
    )
    return [f"{mount}/d.txt" for mount in conflicted]


def assert_pinned_alike(urc: Lore) -> str:
    """Both mounts of the "subtrees" shape pin one revision, which is returned."""
    pins = {link_pin_at(urc, mount) for mount in LINKS_TO_ONE_REPOSITORY["subtrees"]}
    assert len(pins) == 1, f"the mounts must pin one revision, got {pins}"
    return pins.pop()


def diverge_pins_of_both_mounts(new_lore_repo, second_on_main: bool = False) -> tuple[Lore, dict]:
    """Returns (urc, pins): `urc` links a fixed repository at both mounts of the "subtrees" shape,
    pinned at `pins["three"]` on `feature` and at `pins["two"]` on main, which adds the second
    mount where `second_on_main`, leaving main checked out."""
    d = make_repo(new_lore_repo, {"a/d.txt": "one\n", "b/d.txt": "one\n"})
    urc = make_repo(new_lore_repo, {"a.txt": "base\n"})
    first, second = LINKS_TO_ONE_REPOSITORY["subtrees"]
    for mount in (first,) if second_on_main else (first, second):
        add_a_link(urc, d, mount.rsplit("/", 1)[1], mount, disable_branching=True)
    pins = {}
    for content in ("two", "three"):
        d.write_commit_push(content, {"a/d.txt": f"{content}\n"})
        pins[content] = d.branch_info().local_latest
    urc.branch_create("feature")
    update_link(urc, first, pin=pins["three"])
    urc.branch_switch("main")
    update_link(urc, first, pin=pins["two"])
    if second_on_main:
        add_a_link(urc, d, "b", second, pin=pins["two"], disable_branching=True)
    return urc, pins


SAME_SIZE_LINK_FILES = {
    f"data/file{index}.bin": str(index) * 4096 for index in range(4)
}


def assert_scan_matches_linked_files_by_time(repo: Lore):
    summary = parse_status_summary_json(repo.status(scan=True, json=True, offline=True))
    assert summary is not None, "scan must emit a repositoryStatusSummary event"
    assert summary["hashChecks"] == 0, (
        f"the scan must measure no file the command wrote, got {summary}"
    )
    assert summary["mtimeMatches"] >= len(SAME_SIZE_LINK_FILES), (
        f"the scan must match every linked file by its recorded time, got {summary}"
    )


def staged_file_with_owner(new_lore_repo, path: str, owner: str) -> Lore:
    """A new repository with `path` staged, holding `owner` as its content and owner metadata."""
    repo: Lore = new_lore_repo()
    # Content of its own, so a dump of one file cannot pass for the other.
    with repo.open_file(path, "w") as output_file:
        output_file.write(f"{owner}\n")
    repo.stage(path)
    repo.file_metadata_set(path, ["owner", owner])
    return repo


def file_metadata_values(repo: Lore, path: str) -> dict[str, str]:
    events = parse_jsonl(repo.file_metadata_get(path, json=True), "metadata")
    return {event["key"]: event["value"]["data"] for event in events}


def dumped_address(repo: Lore, path: str) -> str:
    output = repo.file_dump(path=path)
    return next(line for line in output.splitlines() if line.startswith("Address"))


def dumped_id(repo: Lore, path: str) -> str:
    """The node id `repository dump --path` reports for `path`."""
    name = re.escape(os.path.basename(path))
    match = re.search(
        rf"^{name} id (\d+)\b", repo.repository_dump(path=path), re.MULTILINE
    )
    assert match, path
    return match.group(1)


def replace_mount_with_a_file(
    new_lore_repo,
    link_files: dict | None = None,
    source_path: str = "/",
    staged: bool = True,
):
    """Returns (parent, link_repo) with the link at `shared`, mounting `source_path` of a
    repository holding `link_files`, replaced by a file, staged when `staged`."""
    parent, link_repo = make_parent_with_link(
        new_lore_repo, "shared", link_files, source_path=source_path
    )
    parent.rmtree("shared")
    parent.write_files({"shared": "file\n"})
    if staged:
        parent.stage("shared", scan=True)
    return parent, link_repo


def written_content(repo: Lore, path: str, output: str, **file_write_kwargs) -> str:
    """The text `lore file write --path path --output output` writes, read as the test wrote it."""
    repo.file_write(path=path, output=output, **file_write_kwargs)
    with open(output) as written_file:
        return written_file.read()


def file_history_entries(repo: Lore, path: str) -> list[tuple[str, str]]:
    """(revision, action) of each `lore file history` entry of `path`, newest first."""
    history = parse_jsonl(repo.file_history(path, json=True), "fileHistory")
    return [(entry["revision"], entry["action"]) for entry in history]


def assert_link_follows_feature(repo: Lore, source: Lore):
    assert re.search(
        rf"Link\s+{source.get_id()}.*?Branch:\s+feature\b[^\n]*\[tracking\]",
        repo.link_list(),
        re.DOTALL,
    ), "the link must follow the parent onto the new branch"


def stage_a_link_removal(parent: Lore, _source: Lore):
    parent.link_remove(DEFAULT_LINK_MOUNT)


def stage_a_remount(parent: Lore, source: Lore):
    remount(parent, DEFAULT_LINK_MOUNT, source.get_id(), "/sub", message=None)


def stage_a_divergent_merge(parent: Lore, _source: Lore):
    """Stages the merge of main's latest, which another clone pushed, into a
    local commit on main."""
    other = parent.clone()
    other.write_commit_push("Remote change", {"remote.txt": "remote\n"})
    parent.write_files({"local.txt": "local\n"})
    parent.stage("local.txt")
    parent.commit("Local change")
    parent.branch_merge_start("main", no_commit=True)


def branch_over_a_link_on_another_branch(new_lore_repo, stage) -> tuple[Lore, Lore]:
    """Parent on a new `feature` branch, created over a link that `link update`
    pinned to another branch of the linked repository, with what `stage` stages
    before the branch was created, and the linked repository."""
    parent, source = make_parent_with_link(
        new_lore_repo,
        link_files={"linked.txt": "linked content\n", "sub/inner.txt": "inner\n"},
    )
    source.branch_create("release")
    source.write_commit_push("Release", {"release.txt": "release\n"})
    update_link(parent, DEFAULT_LINK_MOUNT, pin="release@LATEST")
    assert not link_tracks(parent, DEFAULT_LINK_MOUNT), "the link must name release"
    stage(parent, source)

    parent.branch_create("feature")

    assert current_revision(parent) == parent.branch_info().local_latest, (
        "the working copy must stand on the revision the new branch starts at"
    )
    return parent, source


def stage_a_link_add(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent with a link staged by `link add` and not committed, and the
    linked repository."""
    return make_parent_with_link(new_lore_repo, commit=False)


def stage_a_link_update(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent with a committed link that a staged `link update` repins, and the
    linked repository."""
    parent, source = make_parent_with_link(new_lore_repo)
    source.write_commit_push("Advance", {"advanced.txt": "advanced\n"})
    parent.link_update(DEFAULT_LINK_MOUNT)
    return parent, source


def make_parent_reaching_shared_twice(new_lore_repo) -> tuple[Lore, Lore]:
    """Returns (parent, shared): the parent links `first` and `second`, each of
    which links `shared` at `<name>/shared`, all pushed."""
    parent = make_repo(new_lore_repo, {DEFAULT_PARENT_FILE: "baseline\n"})
    shared = make_repo(new_lore_repo, {"shared.txt": "shared content\n"})
    for name in ("first", "second"):
        middle = make_repo(new_lore_repo, {f"{name}.txt": f"{name} content\n"})
        add_a_link(parent, middle, link_path=name)
        add_a_link(parent, shared, link_path=f"{name}/shared")
    return parent, shared


def make_parent_with_two_links(new_lore_repo):
    """Set up a parent repo with two links (sec, thr) and content in each.

    Returns (parent_repo, second_repo, third_repo).
    """
    repo, (second_repo, third_repo) = make_parent_with_links(
        new_lore_repo,
        {
            "sec": {"second.txt": "second content"},
            "thr": {"third.txt": "third content"},
        },
        {"main.txt": "main content"},
    )
    return repo, second_repo, third_repo


LOCAL_WORK = {
    "vendor/linked.txt": "edited locally\n",
    "vendor/untracked.txt": "never staged\n",
}


LINKED_FILES_WITH_SUB = {"linked.txt": "linked\n", "sub/inner.txt": "inner\n"}


def link_on_feature_with_local_work(
    new_lore_repo, local_work: dict[str, str] = LOCAL_WORK
) -> tuple[Lore, str]:
    """Parent on `feature`, which links a repository holding `LINKED_FILES_WITH_SUB` at
    `vendor/`, with `local_work` written in the mount. `main` holds no link, and
    `folder` holds a plain `vendor/` folder of other files. Returns the parent and
    the revision `feature` branched from."""
    parent = make_repo(new_lore_repo, {"root.txt": "root\n"})
    source = make_repo(new_lore_repo, LINKED_FILES_WITH_SUB)

    parent.branch_create("folder")
    parent.write_commit_push("Add a folder", {"vendor/other.txt": "other\n"})
    parent.branch_switch("main")
    branched_from = parent.branch_info().local_latest

    parent.branch_create("feature")
    add_a_link(parent, source, link_path="vendor")

    parent.write_files(local_work)
    return parent, branched_from


def assert_local_work_kept(parent: Lore) -> None:
    for path, content in LOCAL_WORK.items():
        assert read_text(parent, path) == content, f"{path} must keep its local content"


DEPENDENCY_PARENT_FILES = ["a.txt", "b.txt", "c.txt", "d.txt"]


def dependency_parent_files() -> dict[str, str]:
    return {path: f"{path}\n" for path in DEPENDENCY_PARENT_FILES}


def dependency_listing(repo: Lore) -> dict[str, set[str]]:
    """Each parent file's dependencies, and its dependents marked with `<`."""
    listing = {path: set() for path in DEPENDENCY_PARENT_FILES}
    for reverse, mark in ((False, ""), (True, "<")):
        output = repo.file_dependency_list(
            DEPENDENCY_PARENT_FILES, reverse=reverse, json=True, offline=True
        )
        entries = iter(parse_jsonl(output, "fileDependencyListEntry"))
        files = parse_jsonl(output, "fileDependencyListFile")
        for path, file in zip(DEPENDENCY_PARENT_FILES, files, strict=True):
            assert file["path"] == repo._fix_path(path)
            for _ in range(file["entryCount"]):
                listing[path].add(mark + next(entries)["path"])
    return listing


def change_and_delete(repo: Lore, files: dict[str, str], deleted: list[str]):
    """Changes every file of `files` and deletes those of `deleted`, committed and pushed."""
    repo.write_files({path: "changed\n" for path in files})
    for path in deleted:
        repo.remove_file(path)
    repo.stage(scan=True)
    repo.commit("Change and delete files")
    repo.push()


def changes_at_mount(changes: list, link_path: str) -> list:
    """Every diff entry describing the link node at `link_path`."""
    return [
        change
        for change in changes
        if change.path == link_path and change.node_type == NODE_TYPE_LINK
    ]


def make_parent_with_pin_move(new_lore_repo) -> tuple[Lore, Lore, str, str]:
    """Returns (parent_repo, link_repo, pin_one, pin_two): the parent links the linked repository
    at `pin_one` and ignores `*.log`. `pin_two` modifies `mod.txt` and `tracked.log`, deletes
    `gone.txt` and the directory `olddir`, moves `moved/from.txt` to `moved/to.txt` with other
    content and `moved/plain.txt` to `moved/plain-to.txt` as it is, adds `added.txt` and
    `added.log`, and leaves `keep.txt` as it is."""
    parent_repo, link_repo = make_parent_with_link(
        new_lore_repo,
        link_files={
            "keep.txt": "keep\n",
            "mod.txt": "one\n",
            "tracked.log": "one\n",
            "gone.txt": "one\n",
            "olddir/x.txt": "one\n",
            "moved/from.txt": "one\n",
            "moved/plain.txt": "plain\n",
        },
        parent_files={DEFAULT_PARENT_FILE: "baseline\n", ".loreignore": "*.log\n"},
    )
    pin_one = link_repo.branch_info().local_latest
    for source, destination in [
        ("moved/from.txt", "moved/to.txt"),
        ("moved/plain.txt", "moved/plain-to.txt"),
    ]:
        link_repo.move(source, destination)
        link_repo.stage_move(source, destination)
    link_repo.remove_file("gone.txt")
    link_repo.rmtree("olddir")
    link_repo.write_files(
        {
            "mod.txt": "two\n",
            "tracked.log": "two\n",
            "moved/to.txt": "two\n",
            "added.txt": "two\n",
            "added.log": "two\n",
        }
    )
    link_repo.stage(scan=True)
    link_repo.commit("Pin two")
    link_repo.push()
    return parent_repo, link_repo, pin_one, link_repo.branch_info().local_latest


MAPPED_LINKED_FILES = {
    "foo/f1.txt": "foo 1\n",
    "bar/b1.txt": "bar 1\n",
    "baz/z1.txt": "baz 1\n",
}


MAPPED_FOO = "a/lib"


MAPPED_BAR = "b/lib"


MAPPED_BAZ = "c/lib"


def make_mapped_parent(new_lore_repo, linked: Lore | None = None) -> tuple[Lore, Lore]:
    """Returns (parent, linked), the parent mounting the linked repository's `foo` at
    `MAPPED_FOO` and its `bar` at `MAPPED_BAR`, committed and pushed. The linked repository is `linked`
    where given, else a new repository holding `MAPPED_LINKED_FILES`."""
    if linked is None:
        linked = make_repo(new_lore_repo, MAPPED_LINKED_FILES, "Linked initial")
    parent = make_repo(new_lore_repo, {"README.txt": "baseline\n"}, "Baseline")
    parent.link_add(MAPPED_FOO, linked.get_id(), "/foo")
    parent.link_add(MAPPED_BAR, linked.get_id(), "/bar")
    parent.commit("Mount the linked repository twice")
    parent.push()
    return parent, linked


def assert_warns_once(output: str):
    warnings = [line for line in output.splitlines() if line.startswith("[Warn]")]
    assert len(warnings) == 1, f"Expected one warning for the link.\nGot: {output}"


def link_added_on_branch_and_main_moved(
    new_lore_repo,
    link_path: str,
    feature_edit: str | bytes | None,
    linked_main_files: dict,
    outer: str = "",
    link_file: str | bytes = "link source content\n",
    link_files: dict | None = None,
) -> tuple[Lore, Lore]:
    """`feature-branch` adds a following link to a repository holding `link_file` and, given
    `feature_edit`, commits it to `link-file.txt` through the mount. `linked_main_files` are
    committed to the linked repository's `main` and a change to the parent's, which is left
    checked out. `outer` adds the link in a link mounted there, which main changes."""
    repo, source_repo = link_added_on_feature_branch(
        new_lore_repo, link_path, outer, link_file, link_files
    )
    if feature_edit:
        repo.write_commit_push(
            "Edit inside the link", {f"{link_path}/link-file.txt": feature_edit}
        )
    if linked_main_files:
        source_repo.write_commit_push("Linked main moves on", linked_main_files)
    repo.branch_switch("main")
    repo.write_commit_push(
        "Change on main", {f"{outer}/root.txt".lstrip("/"): "root\n"}
    )
    return repo, source_repo


def sorted_by_name(entries: list[dict]) -> list[dict]:
    return sorted(entries, key=lambda entry: entry["name"])


def link_holding_a_link_added_on_branch(
    new_lore_repo,
    feature_edit: str,
    linked_main_edit: dict,
    mounts: dict[str, str] | None = None,
    inner_mounts: dict[str, str] | None = None,
) -> tuple[Lore, Lore, Lore]:
    """Returns (repo, outer, inner) once `feature-branch` adds following links to `outer`, at each
    of `mounts` mounting its source path (`vendor/b` mounting `/` by default). `outer` holds
    `sub/s.txt` and links `inner` at each of `inner_mounts` mounting its source path (`deps/c`
    mounting `/` by default), each of which holds `c.txt` at `LINK_FILE_EDITS`'s base. `outer`'s
    `feature-branch` committed `feature_edit` to the `c.txt` at `deps/c`.
    `outer`'s `main` then commits `linked_main_edit` inside `inner`, and the parent's `main`, left
    checked out, a change of its own."""
    inner_mounts = inner_mounts or {"deps/c": "/"}
    inner_files = {
        f"{source.strip('/')}/c.txt".lstrip("/"): LINK_FILE_EDITS[0]
        for source in inner_mounts.values()
    }
    inner = make_repo(new_lore_repo, inner_files)
    outer = make_repo(new_lore_repo, {"b.txt": "b base\n", "sub/s.txt": "s\n"})
    for inner_mount, source in inner_mounts.items():
        outer.link_add(inner_mount, inner.get_id(), source)
    outer.commit("Link the inner repository")
    outer.push()
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    repo.branch_create("feature-branch")
    outer.branch_create("feature-branch", id=repo.branch_info("feature-branch").id)
    outer.write_commit_push(
        "Edit inside the nested link", {"deps/c/c.txt": feature_edit}
    )
    for link_path, source_path in (mounts or {"vendor/b": "/"}).items():
        repo.link_add(link_path, outer.get_id(), source_path)
    repo.commit("Add a link holding a link on feature branch")
    repo.push()
    outer.branch_switch("main")
    if linked_main_edit:
        move_linked_main_inside(outer, linked_main_edit)
    repo.branch_switch("main")
    repo.write_commit_push("Change on main", {"root.txt": "root\n"})
    return repo, outer, inner


def move_linked_main_inside(outer: Lore, files: dict):
    outer.write_commit_push(
        "Linked main moves on",
        {f"deps/c/{path}": content for path, content in files.items()},
    )


def assert_mains_hold_the_merge(repo: Lore, outer: Lore, inner: Lore):
    """`repo` pins `outer`'s `main`, which pins `inner`'s `main`."""
    assert_pinned_at_linked_main(repo, outer)
    assert_pinned_at_linked_main(outer.clone(), inner)


def link_edited_on_both_sides(
    new_lore_repo,
    sides: dict[str, tuple[str | bytes, str | bytes, str | bytes]],
    executable_on: tuple[str, ...] = (),
) -> tuple[Lore, Lore]:
    """`feature-branch` adds a following link to a repository holding each file of `sides` at its
    base, with main merged in, as `merge into` requires, and commits each file's feature edit
    through the mount. The linked repository's `main` then commits each file's main edit. Each
    side `executable_on` names, `"feature"` or `"linked main"`, gives `link-file.txt` the
    executable bit with its edit."""
    repo, source_repo = link_added_on_branch_and_main_moved(
        new_lore_repo,
        "linked/repo",
        None,
        {},
        link_files={name: base for name, (base, _, _) in sides.items()},
    )
    merge_main_into_feature(repo)
    repo.write_files(
        {f"linked/repo/{name}": edit for name, (_, edit, _) in sides.items()}
    )
    if "feature" in executable_on:
        os.chmod(os.path.join(repo.path, "linked/repo/link-file.txt"), 0o755)
    repo.stage([f"linked/repo/{name}" for name in sides])
    repo.commit("Edit inside the link")
    repo.push()
    source_repo.write_files({name: edit for name, (_, _, edit) in sides.items()})
    if "linked main" in executable_on:
        os.chmod(os.path.join(source_repo.path, "link-file.txt"), 0o755)
    source_repo.stage(list(sides))
    source_repo.commit("Linked main moves on")
    source_repo.push()
    return repo, source_repo


# Edits on different lines, as base, feature and linked main, and how they merge.
LINK_FILE_EDITS = ("one\ntwo\nthree\n", "feature\ntwo\nthree\n", "one\ntwo\nmain\n")


OTHER_FILE_EDITS = ("a\nb\nc\n", "feature\nb\nc\n", "a\nb\nmain\n")


MERGED_EDITS = {
    "link-file.txt": "feature\ntwo\nmain\n",
    "other.txt": "feature\nb\nmain\n",
}


def fixed_link_pinned_on_main(new_lore_repo, link_path: str) -> tuple[Lore, Lore, str]:
    """Parent on main with a fixed link, and a newer revision to move it to.

    The link is added with `--disable-branching`, so its registry entry names a
    branch of the linked repository and its pin is plain revision data that a
    merge has to carry. The linked repository then gets a second revision, so
    the pin can move with nothing staged through the mount path.
    """
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})

    add_a_link(repo, source_repo, link_path=link_path, disable_branching=True)

    source_repo.write_commit_push(
        "Second source revision", {"link-file.txt": "link source revision two\n"}
    )

    return repo, source_repo, link_pin(repo, source_repo.get_id())


def diverge_link_pins(new_lore_repo, link_path: str) -> tuple[Lore, Lore, str, str]:
    """Feature branch pins a third source revision, main pins the second."""
    repo, source_repo, _pin_base = fixed_link_pinned_on_main(new_lore_repo, link_path)
    revision_two = source_repo.branch_info().local_latest

    source_repo.write_commit_push(
        "Third source revision", {"link-file.txt": "link source revision three\n"}
    )
    revision_three = source_repo.branch_info().local_latest

    repo.branch_create("feature-branch")
    update_link(repo, link_path, pin=revision_three)

    repo.branch_switch("main")
    update_link(repo, link_path, pin=revision_two)

    return repo, source_repo, revision_two, revision_three


def link_readded_fixed_on_feature(
    new_lore_repo, link_path: str, fixed: bool = True
) -> tuple[Lore, Lore, str]:
    """A link on main, re-added at a newer revision on a branch, fixed there where `fixed`
    and following there otherwise."""
    repo = make_repo(new_lore_repo, {"main-file.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"link-file.txt": "link source content\n"})
    repo.link_add(link_path, source_repo.get_id(), "/", disable_branching=not fixed)
    repo.commit("Add the link", non_interactive=True)
    repo.push()

    source_repo.write_commit_push("Second source revision", {"link-file.txt": "two\n"})
    revision_two = source_repo.branch_info().local_latest
    repo.branch_create("feature-branch")
    repo.link_remove(link_path)
    repo.commit("Remove the link", non_interactive=True)
    repo.push()
    repo.link_add(link_path, source_repo.get_id(), "/", disable_branching=fixed)
    repo.commit("Re-add the link", non_interactive=True)
    repo.push()

    repo.branch_switch("main")
    return repo, source_repo, revision_two


def parent_linking_at(new_lore_repo, mount: str) -> tuple[Lore, Lore]:
    """A parent linking at `mount` a repository holding `conflicted.txt` and
    `sibling.txt`, and the linked repository. A mount below `linked/repo` is a link
    inside the link there, which holds `outer.txt`."""
    link_files = {"conflicted.txt": "base\n", "sibling.txt": "sibling\n"}
    if mount == "linked/repo":
        return make_parent_with_link(new_lore_repo, mount, link_files)
    repo, _ = make_parent_with_link(new_lore_repo, "linked/repo", {"outer.txt": "o\n"})
    source = make_repo(new_lore_repo, link_files)
    add_a_link(repo, source, link_path=mount)
    return repo, source


def change_actions(output: str, path: str) -> set[str]:
    """The change actions a diff reported for `path`.

    A directory and a link node both print with a trailing slash, which the
    path a caller asks about does not carry.
    """
    actions = set()
    for line in output.splitlines():
        match = re.match(r"^([ADMCG])\s+(\S.*)$", line.strip())
        if match and match.group(2).rstrip("/") == path:
            actions.add(match.group(1))
    return actions


def conflicted_count(output: str) -> int:
    """The conflict count a merge reported."""
    match = re.search(r"(\d+) conflicted", output)
    assert match, f"merge did not report a conflict count:\n{output}"
    return int(match.group(1))


def assert_mount_conflict(
    parent: Lore,
    merged_branch: str,
    message: str,
) -> None:
    """Merging `merged_branch` conflicts at the mount, commits nothing, and aborts cleanly."""
    head_before = parent.branch_info().local_latest

    output = parent.branch_merge_start(merged_branch, message=message, check=False)

    assert conflicted_count(output) == 1, (
        f"the merge must report the replaced mount as its one conflict, got:\n{output}"
    )
    assert status_paths(parent, "flagConflict") == ["shared"], (
        f"the conflict must be reported at the mount, got {status_paths(parent, 'flagConflict')}"
    )
    listed = parent.branch_merge_conflicts_list()
    assert "shared" in listed.splitlines(), (
        f"`merge conflicts list` must list the mount, got:\n{listed}"
    )
    assert parent.branch_info().local_latest == head_before, (
        "the merge must leave the branch on its pre-merge revision"
    )

    with pytest.raises(UnresolvedConflictError):
        parent.commit(message)

    parent.branch_merge_abort()

    assert parent.branch_info().local_latest == head_before, (
        "aborting the merge must leave the branch on its pre-merge revision"
    )
    assert parse_status_json(parent.status(json=True)) == [], (
        "aborting the merge must leave nothing staged"
    )


def mount_conflict(new_lore_repo, layout: str, into: str):
    """Returns (parent, source, merged, sides) once a merge of the branch `merged` into the
    branch holding `into`, the link or the folder at `shared/`, stops on the conflict at the
    mount.

    `layout` names what the branches did: `folder_changed` and `folder_deleted` replace the
    folder with a link on one branch and change or delete the folder on the other;
    `link_changed` replaces the link with its folder on one branch and edits inside the link on
    the other. `sides` maps `mine` and `theirs` to what that side holds at the mount, as
    `assert_holds_side` reads it.
    """
    if layout == "link_changed":
        parent, source = link_replaced_by_its_folder(new_lore_repo)
        link_branch, folder_branch = "main", "feature"
        folder = ("folder", SHARED_FOLDER)
        edited = {"shared/a.txt": "a changed in the link\n"}
        parent.write_commit_push("Change a file inside the link", edited)
        link = ("link", {**SHARED_FOLDER, **edited})
    else:
        parent, source = folder_replaced_by_a_link(new_lore_repo)
        link_branch, folder_branch = "feature", "main"
        link = ("link", SHARED_FOLDER)
        parent.branch_switch("main")
        if layout == "folder_changed":
            folder = ("folder", change_inside_the_folder(parent))
        else:
            delete_the_folder(parent)
            folder = ("nothing", None)

    current, merged = (
        (link_branch, folder_branch) if into == "link" else (folder_branch, link_branch)
    )
    parent.branch_switch(current)
    output = parent.branch_merge_start(merged, message=f"Merge {merged}", check=False)
    assert conflicted_count(output) == 1, f"the merge must conflict:\n{output}"
    assert parent.unresolved_paths() == {"shared"}, (
        "the conflict must stand at the mount"
    )
    mine, theirs = (link, folder) if into == "link" else (folder, link)
    return parent, source, merged, {"mine": mine, "theirs": theirs}


def assert_holds_side(repo: Lore, source: Lore, side: tuple):
    """`shared/` holds `side`: `("link", files)` for the link to `source` serving `files`,
    `("folder", files)` for a folder of `files`, or `("nothing", None)`."""
    kind, held = side
    if kind == "link":
        assert_holds_link(repo, source)
        assert mounted_files(repo, "shared") == in_the_shared_mount(held)
        assert registered_links(repo) == {"shared": source.get_id()}
    elif kind == "folder":
        assert_holds_folder(repo, held)
        assert registered_links(repo) == {}
    else:
        assert not repo.path_exists("shared"), "shared/ must be gone"
        assert registered_links(repo) == {}


def assert_merge_settled_on_side(parent: Lore, source: Lore, merged: str, side: tuple):
    assert_merge_settled(
        parent, merged, lambda repo: assert_holds_side(repo, source, side)
    )


def assert_holds_side_in_merge(repo: Lore, source: Lore, side: tuple):
    """`shared/` holds `side` in the working tree of a merge in progress, as
    `assert_holds_side` reads it."""
    kind, held = side
    if kind == "link":
        assert registered_links(repo) == {"shared": source.get_id()}
        assert mounted_files(repo, "shared") == in_the_shared_mount(held)
    else:
        assert registered_links(repo) == {}
        assert working_tree_files(repo, "shared") == sorted(held)


def shared_link_row(parent: Lore) -> tuple[str, int, str]:
    """The pin, the flags and the mounted subtree of the link at `shared/`."""
    entry = link_entry(parent, "shared")
    return entry["revision"], entry["flags"], entry["sourcePath"]


def link_row_conflicts(parent: Lore) -> list[str]:
    """The paths of the links whose rows the merge in progress lists as conflicting."""
    return [
        event["linkPath"]
        for event in parse_jsonl(
            parent.branch_merge_conflicts_list(json=True), "branchMergeLinkPinConflict"
        )
    ]


def resolve_the_conflict_inside_the_removed_link(urc):
    """Resolves the conflict the merge of the link at `libs/old` stopped on, and starts the
    merge again to merge the parent, which conflicts at the mount the merged branch removed."""
    resolve_the_only_conflict(urc, "libs/old/shared.txt")
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")


def merged_link_row(repo, link_path: str) -> tuple:
    """What a merge carries of the row of the link at `link_path`: its repository and flags,
    the branch it names unless it follows its parent's, and its pin unless it follows it, as
    the linked branch it lands on takes its pin from a merge."""
    entry = link_entry(repo, link_path)
    tracking = entry["tracking"]
    return (
        entry["link"],
        entry["flags"],
        None if tracking else entry["branch"],
        None if tracking else entry["revision"],
    )


def link_removed_on_feature_and_changed_on_main(
    new_lore_repo, change: str, removed: str
):
    """Returns (urc, link_repo, row, files) with main checked out, where `feature-branch`
    removed the link at `libs/old`, then the `directory` `libs` holding it where `removed`
    says so, and main made `change` to it: `edit_inside` the link, the same after
    `feature-branch` edited inside it too (`edit_inside_both`), a `pin_update` or a
    `branch_change` of a fixed link, or a `flag_change` re-adding it fixed. `row` is main's row
    of the link, as `merged_link_row` reads it, and `files` what a link holding both branches'
    changes serves."""
    urc = make_repo(
        new_lore_repo, {"main-file.txt": "main base\n", "libs/readme.txt": "libs\n"}
    )
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked base\n"})
    fixed = change in ("pin_update", "branch_change")
    add_a_link(urc, link_repo, link_path="libs/old", disable_branching=fixed)

    urc.branch_create("feature-branch")
    feature_files = {}
    if change == "edit_inside_both":
        feature_files = {"feature.txt": "f\n"}
        urc.write_commit_push(
            "Feature changes the link", {"libs/old/feature.txt": "f\n"}
        )
    urc.link_remove("libs/old")
    urc.commit("Feature removes the link")
    if removed == "directory":
        urc.rmtree("libs")
        urc.stage("libs", scan=True)
        urc.commit("Feature removes the directory that held the link")
    urc.push()

    urc.branch_switch("main")
    if change.startswith("edit_inside"):
        urc.write_commit_push("Main changes the link", {"libs/old/main.txt": "m\n"})
    elif change == "pin_update":
        link_repo.write_commit_push("Second revision", {"linked.txt": "two\n"})
        update_link(urc, "libs/old", pin=link_repo.branch_info().local_latest)
    elif change == "branch_change":
        link_repo.branch_create("other")
        link_repo.write_commit_push("On another branch", {"linked.txt": "other\n"})
        update_link(urc, "libs/old", pin="other@LATEST")
    else:
        urc.link_remove("libs/old")
        urc.commit("Main removes the link")
        urc.link_add("libs/old", link_repo.get_id(), "/", disable_branching=True)
        urc.commit("Main links it again fixed")
        urc.push()
    files = {**mounted_files(urc, "libs/old"), **feature_files}
    return urc, link_repo, merged_link_row(urc, "libs/old"), files


NESTED_LINK = "vendor/b/deps/c"


@dataclass
class NestedConflictMerge:
    urc: Lore
    conflicted: str
    """The path in conflict."""
    merged_outer: dict[str, str]
    """The files the merge of the outer repository adds."""
    pins: dict[str, str]
    """The pins of both linked repositories before the merge."""
    nested_id: str
    """The repository the nested link links."""
    latest: str
    """The latest revision before the merge."""


def merge_with_a_conflict_in_a_nested_link(new_lore_repo, link) -> NestedConflictMerge:
    """A merge of every link, of the outer link alone or of the nested link alone, which marks a
    conflict inside the link nested in the outer one at its mount."""
    urc, repo_b, repo_c, _, nested = build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "deps/c"
    )
    beside = make_repo(new_lore_repo, {"beside.txt": "base\n"})
    add_a_link(urc, beside, link_path="vendor/a")
    conflicted = f"{nested}/c-data/inner.txt"
    urc.branch_create("feature")
    urc.write_commit_push(
        "Feature changes the links",
        {
            conflicted: "feature\n",
            "vendor/a/feature.txt": "feature\n",
            "vendor/b/feature.txt": "feature\n",
        },
    )
    urc.branch_switch("main")
    urc.write_commit_push(
        "Main changes the links", {conflicted: "main\n", "vendor/a/main.txt": "main\n"}
    )
    latest = urc.branch_info().local_latest
    merged_outer = {} if link == NESTED_LINK else {"vendor/b/feature.txt": "feature\n"}
    pins = {repo.get_id(): link_pin(urc, repo.get_id()) for repo in [repo_b, repo_c]}

    urc.branch_merge_start("feature", message="Merge feature", link=link)
    assert urc.unresolved_paths() == {conflicted}
    listed = parse_jsonl(
        urc.branch_merge_conflicts_list(json=True), "branchMergeConflictFile"
    )
    assert [entry["path"] for entry in listed] == [conflicted]
    assert_conflict_marked(urc, conflicted)
    return NestedConflictMerge(
        urc, conflicted, merged_outer, pins, repo_c.get_id(), latest
    )


OVERLAP_LINK_FILES = {
    "sub/outer.txt": "outer content\n",
    "sub/test/inner.txt": "inner content\n",
    "sub/other/sibling.txt": "sibling content\n",
}


def parent_and_link_for_overlap(new_lore_repo):
    parent = make_repo(
        new_lore_repo, {"parent.txt": "parent content\n"}, "Initial parent"
    )
    link_repo = make_repo(new_lore_repo, OVERLAP_LINK_FILES, "Initial link")
    return parent, link_repo


def assert_switch_realized(repo: Lore, expected_files: list[str], source: dict):
    unstaged = unstaged_entries(repo)
    assert unstaged == [], f"A clean switch must leave nothing unstaged, got {unstaged}"
    assert source_paths_by_link_path(repo) == source, (
        "The switch must restore the registry the branch committed"
    )
    assert working_tree_files(repo, "vendor") == expected_files, (
        "The switch must realize the mount against the source path its own branch names"
    )


REPLACED_LINK_FILES = {"top.txt": "top\n", "sub/a.txt": "a\n"}


REPLACED_LINK_EXPOSES = {
    "/": (["sub", "top.txt"], {"top.txt": "top\n"}),
    "sub": (["a.txt"], {"a.txt": "a\n"}),
}


def assert_link_put_back(parent: Lore, link_repo: Lore, source_path: str):
    names, files = REPLACED_LINK_EXPOSES[source_path]
    assert_crr_clean(
        parent,
        expected_files_present=[],
        expected_files_absent=[],
        expected_link_registry={link_repo.get_id(): True},
    )
    assert_mount_exposes(parent, "shared", source_path, names)
    assert mounted_files(parent, "shared") == files
