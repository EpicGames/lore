# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Shared test utility functions."""

import json
import os
from pathlib import Path
from typing import TYPE_CHECKING

from lore_parsers import parse_jsonl, parse_status_json

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


def to_posix(path: str | Path) -> str:
    """Normalize a path to forward slashes for comparison against Lore output.

    Lore JSON output always uses forward slashes regardless of platform.
    Use this to normalize os.path.join results before comparing against
    paths extracted from Lore status, unstage events, etc.
    """
    return str(path).replace("\\", "/")


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


def status_by_path(repo: "Lore", **kwargs) -> dict[str, dict]:
    """The `status` entries keyed by path."""
    return {
        e.get("path"): e for e in parse_status_json(repo.status(json=True, **kwargs))
    }


def has_staged_anchor(repo: "Lore") -> bool:
    """Whether `status` reports a staged revision, the state every staged or dirty mark lives in."""
    zero_hash = "0" * 64
    for line in repo.status(json=True, offline=True).splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        revision_staged = event.get("data", {}).get("revisionStaged", "")
        if revision_staged and revision_staged != zero_hash:
            return True
    return False


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
