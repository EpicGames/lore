# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Shared test utility functions."""

import os
import subprocess
from pathlib import Path
from typing import TYPE_CHECKING

from lore_parsers import (
    SharedStoreList,
    parse_jsonl,
    parse_shared_store_list,
    parse_status_json,
    to_posix,
)

if TYPE_CHECKING:
    from lore import Lore

_LORE_DIRECTORIES = frozenset({".urc", ".lore"})


def posix_join(*parts: str) -> str:
    """Join path components using forward slashes.

    Lore always returns paths with forward slashes regardless of platform.
    Using os.path.join on Windows would produce backslashes, causing
    mismatches in path comparisons and regex patterns against Lore output.
    """
    return "/".join(parts)


def commit_and_push_files(
    repo: "Lore", files: list[str], directories: tuple[str, ...] = ()
) -> None:
    """Commits and pushes `files`, each holding its own path, and the empty
    `directories`."""
    repo.write_files({path: path for path in files})
    for path in directories:
        repo.make_dirs(path)
    repo.stage(scan=True)
    repo.commit()
    repo.push()


def working_tree_files(
    repo: "Lore", root: str = "", directories: bool = False
) -> list[str]:
    """Every file the working tree holds below `root`, and every directory where
    `directories`, repo-relative and sorted.

    Lore's own directories are left out, so the result is the content a
    consumer of the working tree sees. A missing `root` holds nothing.
    """
    base = Path(repo.path)
    found = []
    for directory, sub_directories, file_names in os.walk(base / root):
        sub_directories[:] = [
            name for name in sub_directories if name not in _LORE_DIRECTORIES
        ]
        for name in file_names + (sub_directories if directories else []):
            found.append(to_posix(Path(directory, name).relative_to(base)))
    return sorted(found)


def unstaged_entries(repo: "Lore") -> list[dict]:
    """The entries `status --unstaged` reports that are not also staged, sorted by path.

    The command reports both, so the staged ones have to be filtered out to be
    left with what is only on the file system. Its JSON output is the event
    stream, in the order the scan reports entries, so they are sorted here.
    """
    return sorted(
        (
            entry
            for entry in parse_status_json(repo.status(json=True, unstaged=True))
            if not entry.get("flagStaged", False)
        ),
        key=lambda entry: entry["path"],
    )


def view_file(scratch_dir, name: str, rules: list[str]) -> str:
    """A view filter file holding `rules`, one per line, beside the repositories.

    Outside every working tree, since a file inside one would be part of the tree
    the view change carries.
    """
    directory = scratch_dir("view", create=True)
    path = os.path.join(directory, name + ".txt")
    with open(path, "w+") as output_file:
        output_file.writelines(rule + "\n" for rule in rules)
    return path


def status_by_path(repo: "Lore", **kwargs) -> dict[str, dict]:
    """The `status` entries keyed by path."""
    return {
        e.get("path"): e for e in parse_status_json(repo.status(json=True, **kwargs))
    }


def status_revisions(repo: "Lore") -> dict:
    """What `repo` reports of the revisions it stands on."""
    entries = parse_jsonl(
        repo.status(json=True, offline=True, revision_only=True),
        "repositoryStatusRevision",
    )
    return entries[-1]


def staged_revision(repo: "Lore") -> str:
    """The staged state `repo` holds, which is zero while it holds none."""
    return status_revisions(repo)["revisionStaged"]


def has_staged_anchor(repo: "Lore") -> bool:
    """Whether `status` reports a staged revision, the state every staged or dirty mark lives in."""
    return staged_revision(repo) != "0" * 64


def get_unstage_counts(output: str) -> dict:
    """Extract the count object from the fileUnstageEnd event in JSON output."""
    events = parse_jsonl(output, "fileUnstageEnd")
    assert len(events) == 1, f"Expected 1 fileUnstageEnd event, got {len(events)}"
    return events[0]["count"]


def revision_metadata_keys(repo: "Lore") -> set[str]:
    """The keys of the current revision's metadata."""
    events = parse_jsonl(repo.revision_metadata_get(json=True), "metadata")
    return {event["key"] for event in events}


def registered_links(repo: "Lore") -> dict[str, str]:
    """The link registry of `repo`, as the linked repository each link path names."""
    return {
        entry["linkPath"]: entry["link"]
        for entry in parse_jsonl(repo.link_list(json=True), "linkEntry")
    }


def commit_unstaged_below_a_staged_deletion(repo: "Lore", r: str) -> None:
    """Removes the committed directory `r`, holding `a/y`, `a/z` and `keep`, stages its
    deletion, unstages `r/a` below it and commits. Asserts that status shows `r/keep` staged
    for deletion and `r/a` as a deletion not staged before the commit, and that a scan after
    it finds the rest of `r` deleted against the revision but `r/keep`, which the commit
    deleted."""
    repo.rmtree(r)
    repo.stage(r, scan=True)
    repo.unstage(posix_join(r, "a"))

    entries = parse_status_json(repo.status(json=True))
    assert sorted(
        (to_posix(e["path"]), e["action"], e["flagStaged"]) for e in entries
    ) == [
        (posix_join(r, "a"), "delete", False),
        (posix_join(r, "keep"), "delete", True),
    ]
    repo.commit("Delete all but a")
    scanned = parse_status_json(repo.status(json=True, scan=True))
    assert sorted(to_posix(e["path"]) for e in scanned if e["action"] == "delete") == [
        r,
        posix_join(r, "a"),
        posix_join(r, "a", "y"),
        posix_join(r, "a", "z"),
    ]


def store_listing(stores: SharedStoreList, instance: "Lore") -> str | None:
    """The path of the shared store in `stores` that lists `instance`, or `None`
    where none lists it."""
    wanted = os.path.realpath(instance.path)
    for store, entry in stores.entries.items():
        if any(os.path.realpath(path) == wanted for _, path in entry.instances or []):
            return store
    return None


def shared_stores_listed_outside_the_service(
    repo: "Lore",
) -> tuple[SharedStoreList, str]:
    """The shared stores with their instances, as `lore shared-store list` lists
    them run in `repo` with relaying off, and the command's output.

    Runs the CLI directly, bounded, as a listing that waits for a store the
    service holds would otherwise run until the test's own timeout."""
    listed = subprocess.run(
        [
            *(repo.lore_executable_path, "shared-store", "list"),
            *("--include-instances", "true"),
        ],
        capture_output=True,
        check=False,
        text=True,
        env=repo.sandboxed_env(LORE_USE_SERVICE="0"),
        cwd=repo.path,
        timeout=60,
    )
    output = listed.stdout + listed.stderr
    assert listed.returncode == 0, output
    return parse_shared_store_list(listed.stdout), output
