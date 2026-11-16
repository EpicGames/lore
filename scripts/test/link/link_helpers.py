# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Repository setup and output parsing shared by the link tests."""

import os
import re

import pytest
from error_types import OverlappingLinkError
from lore_parsers import parse_jsonl, parse_status_json
from test_utils import staged_revision, unstaged_entries

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


def link_entry(repo: Lore, link_path: str) -> dict:
    """The `link list` entry of the link at `link_path`."""
    entries = parse_jsonl(repo.link_list(json=True), "linkEntry")
    return next(e for e in entries if e["linkPath"] == link_path)


def link_pin(repo: Lore, source_id: str) -> str:
    """The revision `source_id` is pinned at, read from `link list`."""
    output = repo.link_list()
    match = re.search(rf"Link\s+{source_id}.*?Revision:\s*(\w+)", output, re.DOTALL)
    assert match, f"No pin for {source_id} in link list:\n{output}"
    return match.group(1)


def link_pin_at(repo: Lore, link_path: str) -> str:
    """The revision the link at `link_path` pins, read from `link list`."""
    return link_entry(repo, link_path)["revision"]


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


def folder_replaced_by_a_link(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent on `main` holding `shared/`, plus a `feature` branch where a link
    replaced that folder with a repository of its own, with `feature` checked out.

    The linked copy is byte-identical to the folder it replaces, which is what
    moving a folder out into its own repository leaves behind.
    """
    parent = make_repo(
        new_lore_repo,
        {
            "root.txt": "root\n",
            "shared/a.txt": "a original\n",
            "shared/b.txt": "b original\n",
        },
    )

    source = make_repo(
        new_lore_repo,
        {
            "a.txt": "a original\n",
            "b.txt": "b original\n",
        },
    )

    parent.branch_create("feature")
    parent.rmtree("shared")
    parent.stage("shared", scan=True)
    parent.link_add("shared", source.get_id(), "/")
    parent.commit("Replace shared/ with a link")
    parent.push()

    return parent, source


def link_replaced_by_its_folder(new_lore_repo) -> tuple[Lore, Lore]:
    """Parent on `main` holding a link at `shared/`, plus a `feature` branch where
    a folder of the link's content replaced it, with `main` checked out."""
    parent, source = make_parent_with_link(
        new_lore_repo, "shared", {"a.txt": "a original\n", "b.txt": "b original\n"}
    )
    parent.branch_create("feature")
    parent.link_remove("shared")
    parent.commit("Remove the link")
    parent.write_commit_push(
        "Replace the link with its folder",
        {"shared/a.txt": "a original\n", "shared/b.txt": "b original\n"},
    )
    parent.branch_switch("main")
    return parent, source


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


def parent_with_a_link_merging_into_a_failure(new_lore_repo) -> tuple[Lore, Lore]:
    """Returns (urc, nested) once `feature-branch` and main each add a file to the linked
    repository at `libs/first` and change its `data.txt`, and main adds a file to the one at
    `libs/other`, changes its `other.txt`, and mounts all of `nested` inside the link
    `libs/second`, with main checked out."""
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    first = make_repo(new_lore_repo, {"data.txt": "base\n"})
    other = make_repo(new_lore_repo, {"other.txt": "other\n"})
    second = make_repo(new_lore_repo, {"second.txt": "second\n"})
    nested = make_repo(new_lore_repo, {"sub/nested.txt": "nested\n"})
    urc.link_add("libs/first", first.get_id(), "/")
    urc.link_add("libs/other", other.get_id(), "/")
    urc.link_add("libs/second", second.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    def changes(branch: str) -> dict[str, str]:
        return {
            f"libs/first/{branch}.txt": f"{branch}\n",
            "libs/first/data.txt": f"{branch}\n",
        }

    urc.branch_create("feature-branch")
    urc.write_commit_push("Feature changes the first link", changes("feature"))
    urc.branch_switch("main")
    urc.write_commit_push(
        "Main changes the first and other links",
        {
            **changes("main"),
            "libs/other/main.txt": "main\n",
            "libs/other/other.txt": "main\n",
        },
    )
    urc.link_add("libs/second/whole", nested.get_id(), "/")
    urc.commit("Main mounts a repository inside the second link")
    urc.push()
    return urc, nested


def mount_part_inside_the_second_link_on_feature(urc: Lore, nested: Lore) -> Lore:
    """Mounts part of `nested` inside `libs/second` on `feature-branch`, which nests with the
    mount main holds there, so merging the link fails once the merge writes it. Returns the
    clone of `feature-branch` that pushed it."""
    feature = urc.clone(branch="feature-branch")
    feature.link_add("libs/second/part", nested.get_id(), "sub")
    feature.commit("Feature mounts part of a repository inside the second link")
    feature.push()
    return feature


def remove_the_second_link(feature: Lore):
    feature.link_remove("libs/second")
    feature.commit("Feature removes the second link")
    feature.push()


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
    assert sorted(os.listdir(os.path.join(repo.path, link_path))) == sorted(files)
    for name, content in files.items():
        with repo.open_file(f"{link_path}/{name}") as f:
            assert f.read() == content


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
    parent.link_update(SUBTREE_LINK)
    parent.commit("Pick up the linked feature branch")
    parent.push()
    parent.branch_switch("main")
    if conflicting:
        linked.branch_switch("main")
        linked.write_commit_push(
            "Outside change on main", {"config/settings.txt": "settings on main\n"}
        )
        parent.link_update(SUBTREE_LINK)
        parent.commit("Pick up the linked main branch")
        parent.push()
    return parent, linked


def assert_linked_main_merged_settings(parent: Lore, linked: Lore):
    """Linked main's latest is a merge holding feature's `config/settings.txt`, and it is the
    revision the parent's latest pins."""
    linked.branch_switch("main")
    merged = linked.revision_info()
    assert len(merged.merge.split()) == 2, (
        f"linked main must take a merge revision, got {merged}"
    )
    with linked.open_file("config/settings.txt") as f:
        assert f.read() == "settings on feature\n"
    assert committed_link_pin(parent, linked.get_id()) == merged.signature


MERGE_LINKS = ["linked/repo", "linked/other"]


def diverged_link_branches(new_lore_repo):
    urc, first_link = make_parent_with_link(new_lore_repo, MERGE_LINKS[0])
    link_repos = [first_link]
    for link_path in MERGE_LINKS[1:]:
        link_repo = make_repo(new_lore_repo, {"linked.txt": "linked content\n"})
        urc.link_add(link_path, link_repo.get_id(), "/")
        link_repos.append(link_repo)
    urc.commit("Add more links")
    urc.push()

    urc.branch_create("feature-branch")
    urc.write_commit_push(
        "Feature changes",
        {"feature.txt": "feature\n"}
        | {f"{link_path}/feature.txt": "feature\n" for link_path in MERGE_LINKS},
    )
    urc.branch_switch("main")
    urc.write_commit_push(
        "Main changes",
        {"main.txt": "main\n"}
        | {f"{link_path}/main.txt": "main\n" for link_path in MERGE_LINKS},
    )
    return urc, link_repos


def merge_a_branch_removing_a_link(
    new_lore_repo,
    added: bool,
    pinned: bool,
    conflict: str | None = None,
    link_messages: dict[str, str] | None = None,
) -> tuple[Lore, Lore]:
    """Returns (urc, link_repo) once main merges `feature-branch`, which removes the link at
    `libs/old` while each branch changes the linked repository through it, and links the
    repository again at `libs/new` where `added`.

    `pinned` links the first revision of the linked repository at both paths rather than its
    latest. `conflict` names the file both branches change apart from that: `main-file.txt`, or
    `shared.txt` inside the link. `link_messages` passes per-link messages to the merge.
    """
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    link_repo = make_repo(
        new_lore_repo, {"linked.txt": "linked base\n", "shared.txt": "shared base\n"}
    )
    first = link_repo.branch_info().local_latest
    urc.link_add("libs/old", link_repo.get_id(), "/", pin=first if pinned else None)
    urc.commit("Add link")
    urc.push()

    def changes(branch: str) -> dict[str, str]:
        files = {f"libs/old/{branch}.txt": f"{branch}\n"}
        if conflict == "main-file.txt":
            files["main-file.txt"] = f"{branch}\n"
        elif conflict == "shared.txt":
            files["libs/old/shared.txt"] = f"{branch}\n"
        return files

    urc.branch_create("feature-branch")
    urc.write_commit_push("Feature changes the link", changes("feature"))
    urc.link_remove("libs/old")
    if added:
        urc.link_add("libs/new", link_repo.get_id(), "/", pin=first if pinned else None)
    urc.commit("Feature removes the link")
    urc.push()

    urc.branch_switch("main")
    urc.write_commit_push("Main changes the link", changes("main"))

    urc.branch_merge(
        "feature-branch", message="Merge feature-branch", link_messages=link_messages
    )
    return urc, link_repo


def merge_a_branch_moving_a_link(
    new_lore_repo, pinned: bool, moved: tuple[str, str], conflict: bool = False
) -> tuple[Lore, Lore]:
    """Returns (urc, link_repo) once main merges `feature-branch`, which moves `moved[0]`, the
    link `libs/old` or the directory `libs` holding it, to `moved[1]`, while each branch adds a
    file to the linked repository through the link. `pinned` links the first revision of the
    linked repository rather than its latest, and `conflict` changes `main-file.txt` on both
    branches."""
    urc = make_repo(new_lore_repo, {"main-file.txt": "main base\n"})
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked base\n"})
    first = link_repo.branch_info().local_latest
    urc.link_add("libs/old", link_repo.get_id(), "/", pin=first if pinned else None)
    urc.commit("Add link")
    urc.push()

    def changes(branch: str) -> dict[str, str]:
        files = {f"libs/old/{branch}.txt": f"{branch}\n"}
        if conflict:
            files["main-file.txt"] = f"{branch}\n"
        return files

    urc.branch_create("feature-branch")
    urc.write_commit_push("Feature changes the link", changes("feature"))
    urc.move(*moved)
    if moved[0] == "libs/old":
        # `lore stage move` refuses a link, and a scan stages its rename in case.
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
