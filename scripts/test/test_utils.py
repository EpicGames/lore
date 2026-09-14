# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Shared test utility functions."""

import os
from pathlib import Path
from typing import TYPE_CHECKING

from lore_parsers import parse_status_json

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


def working_tree_files(repo: "Lore", root: str = "") -> list[str]:
    """Every file the working tree holds below `root`, repo-relative and sorted.

    Lore's own directories are left out, so the result is the content a
    consumer of the working tree sees. A missing `root` holds nothing.
    """
    base = Path(repo.path)
    found = []
    for directory, sub_directories, file_names in os.walk(base / root):
        sub_directories[:] = [
            name for name in sub_directories if name not in _LORE_DIRECTORIES
        ]
        for file_name in file_names:
            found.append(to_posix(Path(directory, file_name).relative_to(base)))
    return sorted(found)


def unstaged_entries(repo: "Lore") -> list[dict]:
    """The entries `status --unstaged` reports that are not also staged.

    The command reports both, so the staged ones have to be filtered out to be
    left with what is only on the file system.
    """
    return [
        entry
        for entry in parse_status_json(repo.status(json=True, unstaged=True))
        if not entry.get("flagStaged", False)
    ]
