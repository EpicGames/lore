# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""
Move/Copy Test Suite for Lore.

This is a DIAGNOSTIC test suite - tests are expected to fail initially.
They identify gaps in Lore implementation. DO NOT attempt to fix Lore based on
these failures - they are designed to document expected behavior.

Test categories:
- TestFileMoveCore (REQ-F-1 to REQ-F-7)
- TestFileCopyCore (REQ-F-8 to REQ-F-12)
- TestDirectoryMoveCore (REQ-F-13 to REQ-F-20)
- TestDirectoryCopyCore (REQ-F-21 to REQ-F-26)
- TestFileMerge (REQ-F-27 to REQ-F-34)
- TestDirectoryMerge (REQ-F-35 to REQ-F-41)
- TestFileMoveConflicts (REQ-F-42 to REQ-F-53)
- TestDirectoryMoveConflicts (REQ-F-54 to REQ-F-63)
- TestSyncBranchSwitch (REQ-F-64 to REQ-F-74)
- TestCherryPick (REQ-F-75 to REQ-F-81)
- TestNestedDirectory (REQ-F-82 to REQ-F-88)
- TestEdgeCases (REQ-F-89 to REQ-F-97)
"""

import json
import logging
import os
import random
import re
import string
from pathlib import Path
from typing import Optional

import pytest
import status_util
from error_types import CommitFailed
from status_util import verify_operations_in_status, Operation
from test_utils import posix_join, to_posix
from lore_parsers import parse_status_json

from lore import Lore, FileDescription

logger = logging.getLogger(__name__)


# =============================================================================
# Helper Functions
# =============================================================================


def parse_merge_json(merge_output: str) -> dict:
    """
    Parse JSON merge output to extract merge result information.

    The JSON output includes branchMergeStartEnd with conflict count:
    {"tagName":"branchMergeStartEnd","data":{"hasConflicts":0,...}}

    Returns the 'data' dictionary from branchMergeStartEnd, or empty dict.
    """
    for line in merge_output.strip().split("\n"):
        line = line.strip()
        if not line:
            continue
        try:
            parsed = json.loads(line)
            if parsed.get("tagName") == "branchMergeStartEnd" and "data" in parsed:
                return parsed["data"]
        except json.JSONDecodeError:
            continue
    return {}


def create_file_with_content(repo: Lore, path: str, content: str) -> FileDescription:
    """
    Create a file with specified content, stage it, commit it, and return file_info.

    Args:
        repo: Lore repository instance
        path: Path to the file (relative to repo root)
        content: Content to write to the file

    Returns:
        FileDescription for the committed file
    """
    # Ensure parent directory exists
    parent_dir = os.path.dirname(path)
    if parent_dir:
        repo.make_dirs(parent_dir)

    # Write the file
    with repo.open_file(path, "w") as f:
        f.write(content)

    # Stage the file
    repo.file_stage(path, offline=True)

    # Commit the file
    repo.commit(f"Create file: {path}", offline=True)

    # Return file info
    file_infos = repo.file_info(path, offline=True)
    if file_infos:
        return file_infos[0]
    raise ValueError(f"Failed to get file info for {path}")


def create_directory_with_files(
    repo: Lore, dir_path: str, file_count: int
) -> list[FileDescription]:
    """
    Create a directory with the specified number of files, stage, commit, and return file_infos.

    Args:
        repo: Lore repository instance
        dir_path: Path to the directory (relative to repo root)
        file_count: Number of files to create in the directory

    Returns:
        List of FileDescription for all committed files (excluding the directory itself)
    """
    repo.make_dirs(dir_path)

    # Create the files and keep track of their paths
    file_paths = []
    for i in range(file_count):
        file_path = posix_join(dir_path, f"file_{i}.txt")
        file_paths.append(file_path)
        with repo.open_file(file_path, "w") as f:
            f.write(f"Content for file {i} in {dir_path}\n")

    # Stage the directory
    repo.file_stage(dir_path, offline=True)

    # Commit the directory
    repo.commit(f"Create directory with {file_count} files: {dir_path}", offline=True)

    # Return file infos for all files in the directory (not the directory itself)
    file_infos = []
    for file_path in file_paths:
        infos = repo.file_info(file_path, offline=True)
        if infos:
            file_infos.extend(infos)
    return file_infos


def get_file_identity(repo: Lore, path: str) -> str:
    """
    Extract the file identity (context/file ID) from file_info.

    The 'context' field in file_info contains the file's unique identity
    that should be preserved across moves and copies.

    Args:
        repo: Lore repository instance
        path: Path to the file

    Returns:
        The context (file ID) string, or empty string if not found
    """
    file_infos = repo.file_info(path, offline=True)
    if file_infos and len(file_infos) > 0:
        return file_infos[0].context
    return ""


# =============================================================================
# History Entry Data Class and Parsing Functions
# =============================================================================


class HistoryEntry:
    """
    Represents a single entry in a file's history.

    History Format:
    ---------------
    Each history entry starts with an action prefix followed by a space and the path.
    Valid action prefixes:
    - A: Add - File was added (initial creation)
    - M: Modify - File was modified
    - D: Delete - File was deleted
    - V: moVe - File was moved/renamed

    Example history output after a move operation:
    ```
    V original_path.txt -> new_path.txt
    Revision 2
    Signature: ...
    Address: ...
    Branch    : ...
    Date      : ...
        Move file message

    A original_path.txt
    Revision 1
    Signature: ...
    Address: ...
    Branch    : ...
    Date      : ...
        Create file message
    ```

    In this format:
    - Latest (first) entry shows the new path with "V" prefix (move action)
    - Older (second) entry shows the original path with "A" prefix (add action)

    Attributes:
        action: The action type character (A, M, D, or V)
        path: The file path associated with this history entry
        revision: The revision number (if parsed)
        message: The commit message (if parsed)
    """

    # Valid action prefixes for history entries
    VALID_ACTIONS = frozenset({"A", "M", "D", "V"})

    # Human-readable action names
    ACTION_NAMES = {
        "A": "Add",
        "M": "Modify",
        "D": "Delete",
        "V": "Move/Rename",
    }

    def __init__(
        self,
        action: str,
        path: str,
        revision: Optional[int] = None,
        message: str = "",
        from_path: str = "",
    ):
        """
        Initialize a HistoryEntry.

        Args:
            action: The action type character (A, M, D, or V)
            path: The file path associated with this entry
            revision: Optional revision number
            message: Optional commit message
            from_path: Path the file moved from, for move entries
        """
        if action not in self.VALID_ACTIONS:
            raise ValueError(
                f"Invalid action '{action}'. Must be one of: {', '.join(self.VALID_ACTIONS)}"
            )
        self.action = action
        self.path = path
        self.revision = revision
        self.message = message
        self.from_path = from_path

    @property
    def action_name(self) -> str:
        """Get the human-readable action name."""
        return self.ACTION_NAMES.get(self.action, "Unknown")

    def __repr__(self) -> str:
        return (
            f"HistoryEntry(action='{self.action}', path='{self.path}', "
            f"from_path='{self.from_path}', revision={self.revision})"
        )

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, HistoryEntry):
            return False
        return self.action == other.action and self.path == other.path


def parse_history_entries(history_output: str) -> list[HistoryEntry]:
    """
    Parse file_history output into a list of HistoryEntry objects.

    This function parses the history output format used by Lore:
    - Each entry starts with an action prefix (A/M/D/V) followed by space and path
    - Entries are ordered from newest to oldest
    - Additional metadata (revision, date, message) follows each header line

    History Format Example:
    ```
    V original_file.txt -> moved_file.txt
    Revision 2
    Signature: abc123
    Address: ...
    Branch    : main
    Date      : 2025-01-15
        Move file to new location

    A original_file.txt
    Revision 1
    Signature: def456
    Address: ...
    Branch    : main
    Date      : 2025-01-14
        Create initial file
    ```

    Args:
        history_output: Raw string output from repo.file_history()

    Returns:
        List of HistoryEntry objects, ordered from newest to oldest
    """
    entries = []
    lines = history_output.strip().split("\n")

    current_entry: Optional[HistoryEntry] = None
    current_revision: Optional[int] = None

    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue

        # Check if this line is an entry header (action prefix + space + path)
        # Format: "A path/to/file.txt" or "V old_path.txt -> new_path.txt"
        if (
            len(stripped) > 2
            and stripped[0] in HistoryEntry.VALID_ACTIONS
            and stripped[1] == " "
        ):
            # Save previous entry if exists
            if current_entry is not None:
                current_entry.revision = current_revision
                entries.append(current_entry)

            # Start new entry
            action = stripped[0]
            path = stripped[2:]  # Everything after "X "
            # A move names both ends: "V old/path -> new/path"
            from_path, _, moved_to = path.partition(" -> ")
            if moved_to:
                path = moved_to
            else:
                from_path = ""
            current_entry = HistoryEntry(action=action, path=path, from_path=from_path)
            current_revision = None

        elif stripped.startswith("Revision ") and current_entry is not None:
            # Parse revision number, fails test if invalid number
            current_revision = int(stripped.split()[2])

    # Don't forget the last entry
    if current_entry is not None:
        current_entry.revision = current_revision
        entries.append(current_entry)

    return entries


def verify_history_contains_paths(
    repo: Lore, path: str, expected_paths: list[str]
) -> tuple[bool, str]:
    """
    Parse file_history output and verify that the expected paths appear.

    This is used to verify that move/copy operations preserve file history
    and the history shows the file's previous locations.

    History Format:
    ---------------
    History entries are parsed from lines starting with action prefix (A/M/D/V)
    followed by space and path. For example:
    ```
    V original_path.txt -> new_path.txt
    Revision 2
    ...

    A original_path.txt
    Revision 1
    ...
    ```

    Args:
        repo: Lore repository instance
        path: Path to check history for
        expected_paths: List of paths that should appear in the history

    Returns:
        True if all expected paths are found in history entries, False otherwise
    """
    history_output = repo.file_history(path, offline=True)
    entries = parse_history_entries(history_output)

    # Extract all paths from history entries
    found_paths = {entry.path for entry in entries}

    # Check that all expected paths were found
    for expected_path in expected_paths:
        if expected_path not in found_paths:
            return False, (
                f"DIAGNOSTIC: Path '{expected_path}' not found in history for '{path}'. "
                f"Found entries: {[str(e) for e in entries]}. "
                f"History output: {history_output}"
            )

    return True, "Found expected path in history"


def verify_history_entry_at_index(
    repo: Lore, path: str, index: int, expected_action: str, expected_path: str
) -> tuple[bool, str]:
    """
    Verify that a specific history entry matches expected action and path.

    This is useful for verifying the exact order and content of history entries.
    For move operations, the history should show:
    - Index 0 (latest): "V new_path" - the move action to the new path
    - Index 1 (older): "A original_path" - the original add action

    Args:
        repo: Lore repository instance
        path: Path to check history for
        index: Index of the entry to check (0 = newest/latest)
        expected_action: Expected action prefix (A, M, D, or V)
        expected_path: Expected path in the entry

    Returns:
        Tuple of (success: bool, message: str)
    """
    history_output = repo.file_history(path, offline=True)
    entries = parse_history_entries(history_output)

    if index >= len(entries):
        return False, (
            f"Expected at least {index + 1} history entries but found {len(entries)}. "
            f"History output:\n{history_output}"
        )

    entry = entries[index]

    if entry.action != expected_action:
        return False, (
            f"Entry at index {index}: expected action '{expected_action}' "
            f"({HistoryEntry.ACTION_NAMES.get(expected_action, 'Unknown')}) "
            f"but got '{entry.action}' ({entry.action_name}). "
            f"Full entry: {entry}"
        )

    if entry.path != expected_path:
        return False, (
            f"Entry at index {index}: expected path '{expected_path}' "
            f"but got '{entry.path}'. Full entry: {entry}"
        )

    return True, f"Entry at index {index} matches: {entry}"


def verify_move_history(
    repo: Lore, new_path: str, original_path: str
) -> tuple[bool, str]:
    """
    Verify that a file's history correctly shows a move operation.

    After a file is moved, its history should show:
    - Latest (index 0) entry: "V new_path" - Move action showing the new location
    - Previous (index 1) entry: "A original_path" - Add action showing original location

    History Format for Moved File:
    ```
    V original_path.txt -> new_path.txt
    Revision 2
    Signature: ...
    Address: ...
    Branch    : ...
    Date      : ...
        Move file message

    A original_path.txt
    Revision 1
    Signature: ...
    Address: ...
    Branch    : ...
    Date      : ...
        Create file message
    ```

    Args:
        repo: Lore repository instance
        new_path: The current/new path of the moved file
        original_path: The original path before the move

    Returns:
        Tuple of (success: bool, message: str) describing the result
    """
    history_output = repo.file_history(new_path, offline=True)
    entries = parse_history_entries(history_output)

    if len(entries) < 2:
        return False, (
            f"Expected at least 2 history entries for moved file but found {len(entries)}. "
            f"Move history should show: [V {new_path}] then [A {original_path}]. "
            f"History output:\n{history_output}"
        )

    # Check latest entry is the move action with new path
    latest_entry = entries[0]
    if latest_entry.action != "V":
        return False, (
            f"Latest history entry should have 'V' (move) action but has '{latest_entry.action}' "
            f"({latest_entry.action_name}). Expected: 'V {new_path}'. "
            f"Got: '{latest_entry.action} {latest_entry.path}'"
        )

    if latest_entry.path != new_path:
        return False, (
            f"Latest history entry should show new path '{new_path}' but shows '{latest_entry.path}'"
        )

    # Check second entry is the original add action with original path
    original_entry = entries[1]
    if original_entry.action != "A":
        return False, (
            f"Second history entry should have 'A' (add) action but has '{original_entry.action}' "
            f"({original_entry.action_name}). Expected: 'A {original_path}'. "
            f"Got: '{original_entry.action} {original_entry.path}'"
        )

    if original_entry.path != original_path:
        return False, (
            f"Second history entry should show original path '{original_path}' but shows '{original_entry.path}'"
        )

    return True, (
        f"Move history verified: [{latest_entry.action} {latest_entry.path}] <- "
        f"[{original_entry.action} {original_entry.path}]"
    )


def verify_sequential_move_history(
    repo: Lore, final_path: str, path_sequence: list[str]
) -> tuple[bool, str]:
    """
    Verify history for a file that was moved multiple times sequentially.

    For a file moved through path1 -> path2 -> path3 -> final_path, the history
    should show (from newest to oldest):
    - V final_path (latest move)
    - V path3 (previous move)
    - V path2 (earlier move)
    - A path1 (original creation)

    Args:
        repo: Lore repository instance
        final_path: The current/final path of the file
        path_sequence: List of paths in order from original to final,
                       e.g., ["path1", "path2", "path3", "final_path"]

    Returns:
        Tuple of (success: bool, message: str) describing the result
    """
    if len(path_sequence) < 2:
        return False, "path_sequence must contain at least 2 paths (original and final)"

    history_output = repo.file_history(final_path, offline=True)
    entries = parse_history_entries(history_output)

    expected_count = len(path_sequence)
    if len(entries) < expected_count:
        return False, (
            f"Expected at least {expected_count} history entries but found {len(entries)}. "
            f"Path sequence: {path_sequence}. "
            f"History output:\n{history_output}"
        )

    # Reverse path_sequence to match history order (newest first)
    expected_entries = list(reversed(path_sequence))

    for i, expected_path in enumerate(expected_entries):
        entry = entries[i]

        # First entry (i=0, latest) should be V for move, unless it's also the original
        # Last entry in our check should be A for original add
        if i == len(expected_entries) - 1:
            expected_action = "A"  # Original file creation
        else:
            expected_action = "V"  # Move action

        if entry.action != expected_action:
            return False, (
                f"Entry at index {i}: expected action '{expected_action}' but got '{entry.action}'. "
                f"Expected path: {expected_path}, actual: {entry.path}"
            )

        if entry.path != expected_path:
            return False, (
                f"Entry at index {i}: expected path '{expected_path}' but got '{entry.path}'"
            )

    return True, f"Sequential move history verified for {len(path_sequence)} paths"


def verify_content_hash_equal(repo: Lore, path1: str, path2: str) -> bool:
    """
    Compare content hashes of two files.

    Args:
        repo: Lore repository instance
        path1: Path to first file
        path2: Path to second file

    Returns:
        True if content hashes are equal, False otherwise
    """
    info1 = repo.file_info(path1, offline=True)
    info2 = repo.file_info(path2, offline=True)

    if not info1 or not info2:
        logger.warning(
            f"DIAGNOSTIC: Could not get file info for comparison. "
            f"path1 info: {info1}, path2 info: {info2}"
        )
        return False

    hash1 = info1[0].hash if info1 else ""
    hash2 = info2[0].hash if info2 else ""

    if hash1 != hash2:
        logger.warning(
            f"DIAGNOSTIC: Content hashes differ. "
            f"'{path1}' hash: {hash1}, '{path2}' hash: {hash2}"
        )
        return False

    return True


# =============================================================================
# MoveCopyMergeTester - Base class for move/copy merge tests
# =============================================================================


class MoveCopyMergeTester:
    """
    Base class for move/copy merge testing, extending the MergeTester pattern.

    This class provides move/copy-specific methods for testing file and directory
    operations during merge scenarios.
    """

    def __init__(self, repo: Lore, name: str):
        self.repo = repo
        self.name = name
        self.branch_current = "current_" + "".join(random.choices(string.digits, k=8))
        self.branch_incoming = "incoming_" + "".join(random.choices(string.digits, k=8))
        self.result_merge = ""
        self.result_merge_succeeded = False
        self.result_status = ""

    def try_stage(self) -> bool:
        """Stage all changes in the repository."""
        self.repo.file_stage(scan=True, offline=True)
        return True

    def try_commit(self, message: str) -> bool:
        """Commit staged changes with a message."""
        output = self.repo.commit(message, offline=True)
        return message in output

    def try_fail_commit(self, message: str) -> bool:
        """Expect a commit to fail."""
        with pytest.raises(CommitFailed):
            self.repo.commit(message, offline=True)
        return True

    def try_branch_create(self, branch: str) -> bool:
        """Create a new branch."""
        self.repo.branch_create(branch, offline=True)
        return True

    def try_branch_switch(self, branch: str) -> bool:
        """Switch to a branch."""
        self.repo.branch_switch(branch, offline=True)
        return True

    def try_resolve_path(self, path: str) -> bool:
        """Resolve a merge conflict for a path."""
        output = self.repo.branch_merge_resolve(path, offline=True)
        return "No conflicts resolved" not in output

    def try_resolve_mine(self, path: str) -> bool:
        """Resolve a merge conflict using 'mine' version."""
        output = self.repo.branch_merge_resolve_mine(path, offline=True)
        return "No changes staged" not in output

    def try_resolve_theirs(self, path: str) -> bool:
        """Resolve a merge conflict using 'theirs' version."""
        output = self.repo.branch_merge_resolve_theirs(path, offline=True)
        return "No changes staged" not in output

    # =========================================================================
    # Move/Copy-specific methods
    # =========================================================================

    def prepare_moved_file(
        self, original_path: str, new_path: str, content: str
    ) -> tuple[str, str]:
        """
        Create a file and move it using file_stage_move.

        Args:
            original_path: Original path for the file
            new_path: Destination path after move
            content: Content to write to the file

        Returns:
            Tuple of (original_identity, new_identity) - file IDs before and after move
        """
        # Create the original file
        parent_dir = os.path.dirname(original_path)
        if parent_dir:
            self.repo.make_dirs(parent_dir)

        with self.repo.open_file(original_path, "w") as f:
            f.write(content)

        self.repo.file_stage(original_path, offline=True)
        self.repo.commit(f"Create file at {original_path}", offline=True)

        # Get original identity
        original_identity = get_file_identity(self.repo, original_path)

        # Move the file using filesystem operation
        new_parent = os.path.dirname(new_path)
        if new_parent:
            self.repo.make_dirs(new_parent)
        self.repo.move(original_path, new_path)

        # Stage the move
        self.repo.file_stage_move(original_path, new_path, offline=True)
        self.repo.commit(f"Move file from {original_path} to {new_path}", offline=True)

        # Get new identity
        new_identity = get_file_identity(self.repo, new_path)

        return (original_identity, new_identity)

    def verify_move_preserved(
        self, original_identity: str, new_path: str, original_path: Optional[str] = None
    ) -> bool:
        """
        Verify that a move operation preserved file identity and history.

        This method verifies:
        1. File identity (context) is preserved across the move
        2. File history shows the correct move pattern (if original_path provided):
           - Latest entry: "V new_path" (move action)
           - Previous entry: "A original_path" (original add action)

        History Format for Moves:
        -------------------------
        After a move, the file history should show:
        ```
        V original_path.txt -> new_path.txt
        Revision 2
        ...
            Move file message

        A original_path.txt
        Revision 1
        ...
            Create file message
        ```

        Args:
            original_identity: The file identity (context) before the move
            new_path: The path after the move
            original_path: The original path (for history verification)

        Returns:
            True if move preserved identity and history, False otherwise
        """
        # Get current identity
        current_identity = get_file_identity(self.repo, new_path)

        if current_identity != original_identity:
            logger.warning(
                f"DIAGNOSTIC: File identity changed during move. "
                f"Original: {original_identity}, Current: {current_identity}. "
                f"Move operations should preserve file identity."
            )
            return False

        # Verify history shows correct move pattern if original path provided
        if original_path:
            success, message = verify_move_history(self.repo, new_path, original_path)
            if not success:
                logger.warning(
                    f"DIAGNOSTIC: Move history verification failed for '{new_path}'. "
                    f"{message}"
                )
                return False
            logger.info(f"Move history verified: {message}")

        return True

    def verify_identity(self, path1: str, path2: str) -> bool:
        """
        Verify that two paths have the same file identity (context).

        Args:
            path1: First path to check
            path2: Second path to check

        Returns:
            True if identities match, False otherwise
        """
        id1 = get_file_identity(self.repo, path1)
        id2 = get_file_identity(self.repo, path2)

        if not id1 or not id2:
            logger.warning(
                f"DIAGNOSTIC: Could not get identity for paths. "
                f"'{path1}' identity: {id1}, '{path2}' identity: {id2}"
            )
            return False

        if id1 != id2:
            logger.warning(
                f"DIAGNOSTIC: File identities do not match. "
                f"'{path1}' identity: {id1}, '{path2}' identity: {id2}"
            )
            return False

        return True

    # =========================================================================
    # Status checking methods (following MergeTester pattern)
    # =========================================================================

    def is_conflicted_count(self, count: int) -> bool:
        """Check if the merge resulted in the expected number of conflicts."""
        if count > 0:
            if f"{count} conflicted" in self.result_merge:
                return True
            return False
        else:
            if " 0 conflicted" in self.result_merge:
                return True
            if " conflicted" in self.result_merge:
                return False
            return True

    def is_staged(self, path: str) -> bool:
        """Check if a path is staged."""
        check = False
        output = self.result_status
        for line in output.split("\n"):
            if line.startswith("Changes"):
                check = False
            if len(line) == 0:
                check = False
            if line.startswith("Changes staged for commit:"):
                check = True
            if check and path in line:
                return True
        return False

    def is_merged(self, path: str) -> bool:
        """Check if a path appears in merge status."""
        output = self.result_status
        for line in output.split("\n"):
            if path in line:
                return True
        return False

    def is_conflicted(self, path: str) -> bool:
        """Check if a path is in conflict."""
        output = self.result_status
        for line in output.split("\n"):
            if path in line and "!" in line:
                return True
        return False

    def is_containing_text(self, path: str, contents: str) -> bool:
        """Check if a file contains the expected text."""
        with self.repo.open_file(path, "r") as f:
            data = f.read()
            return data == contents

    # =========================================================================
    # Expectation methods with diagnostic messages
    # =========================================================================

    def expect_merge_failed(self):
        """Assert that the merge failed."""
        assert not self.result_merge_succeeded, (
            f"DIAGNOSTIC [{self.name}]: Expected merge to fail but it succeeded. "
            f"This may indicate Lore behavior differs from expected."
        )

    def expect_merge_succeeded(self):
        """Assert that the merge succeeded."""
        assert self.result_merge_succeeded, (
            f"DIAGNOSTIC [{self.name}]: Expected merge to succeed but it failed. "
            f"Merge output: {self.result_merge}"
        )

    def expect_conflict_count(self, count: int):
        """Assert the expected number of conflicts."""
        assert self.is_conflicted_count(count), (
            f"DIAGNOSTIC [{self.name}]: Expected {count} conflict(s) but got different count. "
            f"Merge output: {self.result_merge}"
        )

    def expect_merged(self, path: str):
        """Assert that a path was merged."""
        assert self.is_merged(path), (
            f"DIAGNOSTIC [{self.name}]: Expected '{path}' to be merged but it was not. "
            f"Status: {self.result_status}"
        )

    def expect_not_merged(self, path: str):
        """Assert that a path was not merged."""
        assert not self.is_merged(path), (
            f"DIAGNOSTIC [{self.name}]: Expected '{path}' to NOT be merged but it was. "
            f"Status: {self.result_status}"
        )

    def expect_staged(self, path: str):
        """Assert that a path is staged."""
        assert self.is_staged(path), (
            f"DIAGNOSTIC [{self.name}]: Expected '{path}' to be staged but it was not. "
            f"Status: {self.result_status}"
        )

    def expect_not_staged(self, path: str):
        """Assert that a path is not staged."""
        assert not self.is_staged(path), (
            f"DIAGNOSTIC [{self.name}]: Expected '{path}' to NOT be staged but it was. "
            f"Status: {self.result_status}"
        )

    def expect_conflicted(self, path: str):
        """Assert that a path is in conflict."""
        assert self.is_conflicted(path), (
            f"DIAGNOSTIC [{self.name}]: Expected '{path}' to be in conflict but it was not. "
            f"Status: {self.result_status}"
        )

    def expect_not_conflicted(self, path: str):
        """Assert that a path is not in conflict."""
        assert not self.is_conflicted(path), (
            f"DIAGNOSTIC [{self.name}]: Expected '{path}' to NOT be in conflict but it was. "
            f"Status: {self.result_status}"
        )

    def expect_exists(self, path: str):
        """Assert that a path exists on the filesystem."""
        full_path = os.path.join(self.repo.path, path)
        assert os.path.exists(full_path), (
            f"DIAGNOSTIC [{self.name}]: Expected '{path}' to exist but it does not."
        )

    def expect_not_exists(self, path: str):
        """Assert that a path does not exist on the filesystem."""
        full_path = os.path.join(self.repo.path, path)
        assert not os.path.exists(full_path), (
            f"DIAGNOSTIC [{self.name}]: Expected '{path}' to NOT exist but it does."
        )

    def expect_text_contents(self, path: str, contents: str):
        """Assert that a file contains the expected text."""
        assert self.is_containing_text(path, contents), (
            f"DIAGNOSTIC [{self.name}]: Expected different text contents for '{path}'."
        )

    def expect_history_contains(self, path: str, expected_paths: list[str]):
        """Assert that file history contains expected paths."""
        success, message = verify_history_contains_paths(
            self.repo, path, expected_paths
        )
        assert success, (
            f"DIAGNOSTIC [{self.name}]: File history for '{path}' failed. {message}\n"
            f"expected paths: {expected_paths}. "
            f"Move operations should preserve history lineage."
        )

    def expect_move_history(self, new_path: str, original_path: str):
        """Assert that file history shows correct move pattern.

        Verifies:
        - Latest entry is 'V new_path' (move action)
        - Previous entry is 'A original_path' (original add action)

        History Format Example:
        ```
        V original_path.txt -> new_path.txt
        Revision 2
        ...

        A original_path.txt
        Revision 1
        ...
        ```
        """
        success, message = verify_move_history(self.repo, new_path, original_path)
        assert success, (
            f"DIAGNOSTIC [{self.name}]: Move history verification failed. {message}\n"
            f"Expected: 'V {new_path}' (latest) followed by 'A {original_path}' (previous)."
        )

    def expect_sequential_move_history(self, final_path: str, path_sequence: list[str]):
        """Assert that file history shows correct sequential move pattern.

        For a file moved through path1 -> path2 -> ... -> final_path, verifies:
        - Latest entries show 'V' actions for each move in reverse order
        - Oldest entry shows 'A' action for original creation

        Args:
            final_path: The current/final path of the file
            path_sequence: List of paths in chronological order [original, ..., final]
        """
        success, message = verify_sequential_move_history(
            self.repo, final_path, path_sequence
        )
        assert success, (
            f"DIAGNOSTIC [{self.name}]: Sequential move history verification failed. {message}\n"
            f"Expected path sequence (oldest to newest): {path_sequence}"
        )

    def expect_history_entry(
        self, path: str, index: int, expected_action: str, expected_path: str
    ):
        """Assert that a specific history entry matches expected values.

        Args:
            path: Path to check history for
            index: Index of entry to check (0 = newest/latest)
            expected_action: Expected action prefix (A/M/D/V)
            expected_path: Expected path in the entry
        """
        success, message = verify_history_entry_at_index(
            self.repo, path, index, expected_action, expected_path
        )
        assert success, (
            f"DIAGNOSTIC [{self.name}]: History entry verification failed. {message}"
        )


# =============================================================================
# Test Classes - Stubbed for 97 functional requirements
# =============================================================================


class TestFileMoveCore:
    """
    Tests for core file move functionality (REQ-F-1 to REQ-F-7).

    These tests verify basic file move operations including:
    - File identity preservation across moves
    - History tracking through moves
    - File content preservation
    - Stage move command functionality
    """

    @pytest.mark.smoke
    def test_stage_move_records_action(self, new_lore_repo):
        """REQ-F-1: stage_move correctly records move action.

        Verifies that after staging a file move, the status shows the file as moved
        with the 'V ' action indicator, from path, and to path.

        Status output format for move actions is:
        "{action_short} {from_path} -> {to_path} {merged_string}"
        Where action_as_string_short for move is "V"
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        original_path = "original.txt"
        content = "Test content for move operation\n"
        create_file_with_content(repo, original_path, content)

        # Move the file on the filesystem
        new_path = "moved.txt"
        repo.move(original_path, new_path)

        # Stage the move
        result = repo.file_stage_move(original_path, new_path, offline=True)

        # Verify the staging output indicates a move
        assert "moved" in result.lower() or "1" in result, (
            f"DIAGNOSTIC: stage_move did not record move action. "
            f"Expected 'moved' in output, got: {result}"
        )

        # Check status
        status = repo.status(offline=True)
        logger.info(f"Status after stage_move: {status}")

        # The moved file should appear in staged changes
        assert new_path in status or "moved" in status.lower(), (
            f"DIAGNOSTIC: Status does not show moved file. Status: {status}"
        )

        # Verify the status output contains the move action indicator "V "
        # Status format for moves: "V  {from_path} -> {to_path}"
        assert "V " in status, (
            f"DIAGNOSTIC: Status does not show 'V ' move action indicator. "
            f"Expected 'V ' in status output. Status: {status}"
        )

        # Verify the correct order of from_path and to_path using regex pattern:
        # "V" at start, then from_path BEFORE "->", then to_path AFTER "->"
        move_pattern = rf"V\s+{re.escape(original_path)}\s+->\s+{re.escape(new_path)}"
        assert re.search(move_pattern, status), (
            f"DIAGNOSTIC: Status does not show correct move format with from_path before -> and to_path after. "
            f"Expected pattern: 'V {original_path} -> {new_path}'. Status: {status}"
        )

    @pytest.mark.smoke
    def test_commit_move_updates_file_location(self, new_lore_repo):
        """REQ-F-2: Committed move updates file location.

        After committing a move, the file should exist at the new location
        and file_info should show the new path. Before committing, the status
        should show the 'V ' action indicator, from path, to path, and '->' separator.

        Status output format for move actions is:
        "{action_short} {from_path} -> {to_path} {merged_string}"
        Where action_as_string_short for move is "V"
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        original_path = "file_to_move.txt"
        content = "Content that will be moved\n"
        create_file_with_content(repo, original_path, content)

        # Move the file
        new_path = "file_after_move.txt"
        repo.move(original_path, new_path)
        repo.file_stage_move(original_path, new_path, offline=True)

        # Verify the status output contains move action indicator before commit
        status = repo.status(offline=True)
        logger.info(f"Status after stage_move (before commit): {status}")

        # Verify the move action indicator "V " is present
        assert "V " in status, (
            f"DIAGNOSTIC: Status does not show 'V ' move action indicator. "
            f"Expected 'V ' in status output. Status: {status}"
        )

        # Verify the correct order of from_path and to_path using regex pattern:
        # "V" at start, then from_path BEFORE "->", then to_path AFTER "->"
        move_pattern = rf"V\s+{re.escape(original_path)}\s+->\s+{re.escape(new_path)}"
        assert re.search(move_pattern, status), (
            f"DIAGNOSTIC: Status does not show correct move format with from_path before -> and to_path after. "
            f"Expected pattern: 'V {original_path} -> {new_path}'. Status: {status}"
        )

        repo.commit("Move file to new location", offline=True)

        # Verify file exists at new location
        assert repo.file_exists(new_path), (
            f"DIAGNOSTIC: File does not exist at new path '{new_path}' after committed move"
        )

        # Verify file does not exist at original location
        assert not repo.file_exists(original_path), (
            f"DIAGNOSTIC: File still exists at original path '{original_path}' after committed move"
        )

        # Verify file_info works at new path
        file_infos = repo.file_info(new_path, offline=True)
        assert file_infos and len(file_infos) > 0, (
            f"DIAGNOSTIC: file_info returned no results for moved file at '{new_path}'"
        )

    @pytest.mark.smoke
    def test_move_preserves_file_identity(self, new_lore_repo):
        """REQ-F-3: Move preserves file identity (same context/file ID).

        The file's context (identity) should remain the same after a move.
        This is what distinguishes a move from a delete+add.
        The status should show the 'V ' action indicator, from path, to path,
        and '->' separator for the moved file.

        Status output format for move actions is:
        "{action_short} {from_path} -> {to_path} {merged_string}"
        Where action_as_string_short for move is "V"
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        original_path = "identity_test.txt"
        content = "Content for identity preservation test\n"
        create_file_with_content(repo, original_path, content)

        # Get original identity
        original_identity = get_file_identity(repo, original_path)
        assert original_identity, (
            f"DIAGNOSTIC: Could not get file identity for '{original_path}' before move"
        )
        logger.info(f"Original file identity: {original_identity}")

        # Move the file
        new_path = "identity_test_moved.txt"
        repo.move(original_path, new_path)
        repo.file_stage_move(original_path, new_path, offline=True)

        # Verify the status output contains move action indicator before commit
        status = repo.status(offline=True)
        logger.info(f"Status after stage_move (before commit): {status}")

        # Verify the move action indicator "V " is present
        assert "V " in status, (
            f"DIAGNOSTIC: Status does not show 'V ' move action indicator. "
            f"Expected 'V ' in status output. Status: {status}"
        )

        # Verify the correct order of from_path and to_path using regex pattern:
        # "V" at start, then from_path BEFORE "->", then to_path AFTER "->"
        move_pattern = rf"V\s+{re.escape(original_path)}\s+->\s+{re.escape(new_path)}"
        assert re.search(move_pattern, status), (
            f"DIAGNOSTIC: Status does not show correct move format with from_path before -> and to_path after. "
            f"Expected pattern: 'V {original_path} -> {new_path}'. Status: {status}"
        )

        repo.commit("Move file for identity test", offline=True)

        # Get new identity
        new_identity = get_file_identity(repo, new_path)
        assert new_identity, (
            f"DIAGNOSTIC: Could not get file identity for '{new_path}' after move"
        )
        logger.info(f"New file identity: {new_identity}")

        # Verify identity is preserved
        assert original_identity == new_identity, (
            f"DIAGNOSTIC: File identity changed during move. "
            f"Original: {original_identity}, After move: {new_identity}. "
            f"Move operations should preserve file identity."
        )

    @pytest.mark.smoke
    def test_move_preserves_file_history(self, new_lore_repo):
        """REQ-F-4: Move preserves complete file history.

        After a move, file_history should show both the original and new paths
        with the correct prefixes:
        - Latest (first) entry: Shows the new path with "V" prefix (move/rename action)
        - Second (older) entry: Shows the old path with "A" prefix (add action)

        Example history output format:
        ```
        V history_test_moved.txt
        Revision 2
        Signature: ...
        Address: ...
        Branch    : ...
        Date      : ...
            Move file for history test

        A history_test.txt
        Revision 1
        Signature: ...
        Address: ...
        Branch    : ...
        Date      : ...
            Create file: history_test.txt
        ```
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        original_path = "history_test.txt"
        content = "Content for history preservation test\n"
        create_file_with_content(repo, original_path, content)

        # Move the file
        new_path = "history_test_moved.txt"
        repo.move(original_path, new_path)
        repo.file_stage_move(original_path, new_path, offline=True)
        repo.commit("Move file for history test", offline=True)

        # Use the centralized helper to verify move history
        success, message = verify_move_history(repo, new_path, original_path)

        logger.info(f"Move history verification: {message}")

        assert success, (
            f"DIAGNOSTIC: {message}\n"
            f"Move operations should preserve history with 'V' prefix for new path "
            f"and 'A' prefix for original path."
        )

    @pytest.mark.smoke
    def test_move_to_different_directory(self, new_lore_repo):
        """REQ-F-5: Move to different directory works.

        A file can be moved to a different directory while preserving identity.
        """
        repo: Lore = new_lore_repo()

        # Create a file in root
        original_path = "root_file.txt"
        content = "File to be moved to subdirectory\n"
        create_file_with_content(repo, original_path, content)

        # Get original identity
        original_identity = get_file_identity(repo, original_path)

        # Create target directory and move file
        target_dir = "subdir"
        new_path = posix_join(target_dir, "root_file.txt")
        repo.make_dirs(target_dir)
        repo.move(original_path, new_path)
        repo.file_stage_move(original_path, new_path, offline=True)
        repo.commit("Move file to subdirectory", offline=True)

        # Verify file exists at new location
        assert repo.file_exists(new_path), (
            f"DIAGNOSTIC: File does not exist at new path '{new_path}' after move to directory"
        )

        # Verify identity is preserved
        new_identity = get_file_identity(repo, new_path)
        assert original_identity == new_identity, (
            f"DIAGNOSTIC: File identity changed during cross-directory move. "
            f"Original: {original_identity}, After move: {new_identity}"
        )

    @pytest.mark.smoke
    def test_multiple_sequential_moves(self, new_lore_repo):
        """REQ-F-6: Multiple sequential moves track all locations.

        A file moved multiple times should have all paths in its history.
        The history entries should show (from newest to oldest):
        - V path4 (final location)
        - V path3 (third location)
        - V path2 (second location)
        - A path1 (original creation)
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        path1 = "sequential_move_1.txt"
        content = "Content for sequential move test\n"
        create_file_with_content(repo, path1, content)

        # Get original identity
        original_identity = get_file_identity(repo, path1)

        # First move
        path2 = "sequential_move_2.txt"
        repo.move(path1, path2)
        repo.file_stage_move(path1, path2, offline=True)
        repo.commit("First move", offline=True)

        # Second move
        path3 = "sequential_move_3.txt"
        repo.move(path2, path3)
        repo.file_stage_move(path2, path3, offline=True)
        repo.commit("Second move", offline=True)

        # Third move
        path4 = "sequential_move_final.txt"
        repo.move(path3, path4)
        repo.file_stage_move(path3, path4, offline=True)
        repo.commit("Third move", offline=True)

        # Verify final file exists
        assert repo.file_exists(path4), (
            f"DIAGNOSTIC: File does not exist at final path '{path4}' after sequential moves"
        )

        # Verify identity is preserved through all moves
        final_identity = get_file_identity(repo, path4)
        assert original_identity == final_identity, (
            f"DIAGNOSTIC: File identity changed during sequential moves. "
            f"Original: {original_identity}, Final: {final_identity}"
        )

        # Use the centralized helper to verify sequential move history
        path_sequence = [path1, path2, path3, path4]
        success, message = verify_sequential_move_history(repo, path4, path_sequence)

        logger.info(f"Sequential move history verification: {message}")

        assert success, (
            f"DIAGNOSTIC: {message}\n"
            f"Sequential moves should preserve history showing all paths in order."
        )

    @pytest.mark.smoke
    def test_move_with_concurrent_content_change(self, new_lore_repo):
        """REQ-F-7: Move with concurrent content change.

        A file can be moved and its content modified in the same commit.
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        original_path = "content_change.txt"
        original_content = "Original content before move\n"
        create_file_with_content(repo, original_path, original_content)

        # Get original identity
        original_identity = get_file_identity(repo, original_path)

        # Move the file
        new_path = "content_change_moved.txt"
        repo.move(original_path, new_path)

        # Modify content at new location
        new_content = "Modified content after move\n"
        with repo.open_file(new_path, "w") as f:
            f.write(new_content)

        # Stage move and content change
        repo.file_stage_move(original_path, new_path, offline=True)
        repo.commit("Move and modify file", offline=True)

        # Verify content is updated
        with repo.open_file(new_path, "r") as f:
            actual_content = f.read()
        assert actual_content == new_content, (
            f"DIAGNOSTIC: File content not updated after move+modify. "
            f"Expected: {new_content!r}, Got: {actual_content!r}"
        )

        # Verify identity is preserved
        new_identity = get_file_identity(repo, new_path)
        assert original_identity == new_identity, (
            f"DIAGNOSTIC: File identity changed during move+modify. "
            f"Original: {original_identity}, After: {new_identity}"
        )


class TestFileCopyCore:
    """
    Tests for core file copy functionality (REQ-F-8 to REQ-F-12).

    These tests verify basic file copy operations including:
    - Copy creates a new file identity (unlike move)
    - Content is identical to source
    - Source file remains unchanged
    - Stage copy command functionality

    NOTE: The 'repo file stage copy' command is not yet implemented in Lore.
    These tests will fail until the command is added. Tests use fallback
    staging as new files when the copy command fails.
    """

    @pytest.mark.smoke
    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist"
    )
    def test_stage_copy_records_action(self, new_lore_repo):
        """REQ-F-8: stage_copy correctly records copy action.

        Verifies that after staging a file copy, the copy is recorded.
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        source_path = "source_file.txt"
        content = "Test content for copy operation\n"
        create_file_with_content(repo, source_path, content)

        # Copy the file on the filesystem
        dest_path = "copied_file.txt"
        repo.copy_file(source_path, dest_path)

        # Stage the copy
        result = repo.file_stage_copy(source_path, dest_path, offline=True)
        logger.info(f"stage_copy result: {result}")

        # Verify the staging worked
        status = repo.status(offline=True)
        logger.info(f"Status after stage_copy: {status}")

        # The copied file should appear in staged changes
        assert (
            dest_path in status
            or "added" in status.lower()
            or "copied" in status.lower()
        ), f"DIAGNOSTIC: Status does not show copied file. Status: {status}"

    @pytest.mark.smoke
    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist"
    )
    def test_commit_copy_creates_new_file(self, new_lore_repo):
        """REQ-F-9: Committed copy creates new file at destination.

        After committing a copy, both source and destination files should exist.
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        source_path = "copy_source.txt"
        content = "Content that will be copied\n"
        create_file_with_content(repo, source_path, content)

        # Copy the file
        dest_path = "copy_destination.txt"
        repo.copy_file(source_path, dest_path)
        repo.file_stage_copy(source_path, dest_path, offline=True)
        repo.commit("Copy file to new location", offline=True)

        # Verify both files exist
        assert repo.file_exists(source_path), (
            f"DIAGNOSTIC: Source file '{source_path}' does not exist after copy"
        )
        assert repo.file_exists(dest_path), (
            f"DIAGNOSTIC: Destination file '{dest_path}' does not exist after copy"
        )

        # Verify file_info works for both
        source_infos = repo.file_info(source_path, offline=True)
        dest_infos = repo.file_info(dest_path, offline=True)

        assert source_infos and len(source_infos) > 0, (
            f"DIAGNOSTIC: file_info returned no results for source file '{source_path}'"
        )
        assert dest_infos and len(dest_infos) > 0, (
            f"DIAGNOSTIC: file_info returned no results for destination file '{dest_path}'"
        )

    @pytest.mark.smoke
    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist"
    )
    def test_copy_creates_new_identity(self, new_lore_repo):
        """REQ-F-10: Copy creates new file identity (different context/file ID from source).

        Unlike a move, a copy should create a new file identity.
        The copied file should have a different context than the source.
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        source_path = "identity_source.txt"
        content = "Content for copy identity test\n"
        create_file_with_content(repo, source_path, content)

        # Get source identity
        source_identity = get_file_identity(repo, source_path)
        assert source_identity, (
            f"DIAGNOSTIC: Could not get file identity for source '{source_path}'"
        )
        logger.info(f"Source file identity: {source_identity}")

        # Copy the file
        dest_path = "identity_destination.txt"
        repo.copy_file(source_path, dest_path)
        repo.file_stage_copy(source_path, dest_path, offline=True)
        repo.commit("Copy file for identity test", offline=True)

        # Get destination identity
        dest_identity = get_file_identity(repo, dest_path)
        assert dest_identity, (
            f"DIAGNOSTIC: Could not get file identity for destination '{dest_path}'"
        )
        logger.info(f"Destination file identity: {dest_identity}")

        # Verify identities are DIFFERENT for a copy
        assert source_identity != dest_identity, (
            f"DIAGNOSTIC: File identities are the same after copy. "
            f"Source: {source_identity}, Destination: {dest_identity}. "
            f"Copy operations should create NEW file identities."
        )

    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist"
    )
    def test_copy_preserves_content(self, new_lore_repo):
        """REQ-F-11: Copy preserves content (same content hash).

        The copied file should have the same content hash as the source.
        This enables content deduplication.
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        source_path = "content_source.txt"
        content = "Content that should be preserved through copy\n"
        create_file_with_content(repo, source_path, content)

        # Get source content hash
        source_infos = repo.file_info(source_path, offline=True)
        source_hash = source_infos[0].hash if source_infos else ""
        logger.info(f"Source content hash: {source_hash}")

        # Copy the file
        dest_path = "content_destination.txt"
        repo.copy_file(source_path, dest_path)
        repo.file_stage_copy(source_path, dest_path, offline=True)
        repo.commit("Copy file for content test", offline=True)

        # Get destination content hash
        dest_infos = repo.file_info(dest_path, offline=True)
        dest_hash = dest_infos[0].hash if dest_infos else ""
        logger.info(f"Destination content hash: {dest_hash}")

        # Verify content hashes match
        assert source_hash == dest_hash, (
            f"DIAGNOSTIC: Content hashes differ after copy. "
            f"Source: {source_hash}, Destination: {dest_hash}. "
            f"Copy operations should preserve content hash."
        )

        # Also verify the actual content matches
        with repo.open_file(source_path, "r") as f:
            source_content = f.read()
        with repo.open_file(dest_path, "r") as f:
            dest_content = f.read()

        assert source_content == dest_content, (
            f"DIAGNOSTIC: File contents differ after copy. "
            f"Source: {source_content!r}, Destination: {dest_content!r}"
        )

    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist"
    )
    def test_source_unchanged_after_copy(self, new_lore_repo):
        """REQ-F-12: Source file unchanged after copy.

        The source file should remain unchanged after a copy operation.
        Its identity, content, and path should all remain the same.
        """
        repo: Lore = new_lore_repo()

        # Create and commit a file
        source_path = "unchanged_source.txt"
        content = "Source content that should remain unchanged\n"
        create_file_with_content(repo, source_path, content)

        # Get source info before copy
        source_identity_before = get_file_identity(repo, source_path)
        source_infos_before = repo.file_info(source_path, offline=True)
        source_hash_before = source_infos_before[0].hash if source_infos_before else ""

        # Copy the file
        dest_path = "copy_of_unchanged.txt"
        repo.copy_file(source_path, dest_path)
        repo.file_stage_copy(source_path, dest_path, offline=True)
        repo.commit("Copy file to verify source unchanged", offline=True)

        # Modify the destination to ensure source is independent
        with repo.open_file(dest_path, "w") as f:
            f.write("Modified destination content\n")
        repo.file_stage(dest_path, offline=True)
        repo.commit("Modify destination file", offline=True)

        # Verify source is unchanged
        assert repo.file_exists(source_path), (
            f"DIAGNOSTIC: Source file '{source_path}' no longer exists after copy and dest modification"
        )

        source_identity_after = get_file_identity(repo, source_path)
        assert source_identity_before == source_identity_after, (
            f"DIAGNOSTIC: Source file identity changed after copy. "
            f"Before: {source_identity_before}, After: {source_identity_after}"
        )

        source_infos_after = repo.file_info(source_path, offline=True)
        source_hash_after = source_infos_after[0].hash if source_infos_after else ""
        assert source_hash_before == source_hash_after, (
            f"DIAGNOSTIC: Source file content hash changed after copy. "
            f"Before: {source_hash_before}, After: {source_hash_after}"
        )

        # Verify source content is unchanged
        with repo.open_file(source_path, "r") as f:
            source_content_after = f.read()
        assert source_content_after == content, (
            f"DIAGNOSTIC: Source file content changed after copy. "
            f"Expected: {content!r}, Got: {source_content_after!r}"
        )


class TestDirectoryMoveCore:
    """
    Tests for core directory move functionality (REQ-F-13 to REQ-F-20).

    These tests verify directory move operations including:
    - All files in directory maintain their identities
    - Directory structure is preserved
    - Nested directories are handled correctly
    - History tracking for all files in moved directory
    """

    @pytest.mark.smoke
    def test_stage_directory_move_records_action(self, new_lore_repo):
        """REQ-F-13: stage_move on directory records move action.

        Verifies that after staging a directory move, the status shows the directory
        as moved with the 'V ' action indicator but excluding any contained files.

        Status output format for move actions is:
        "{action_short} {from_path} -> {to_path} {merged_string}"
        Where action_as_string_short for move is "V"
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        dir_path = "dir_to_move"
        file_infos = create_directory_with_files(repo, dir_path, 3)
        assert len(file_infos) >= 3, "Failed to create directory with files"

        # Move the directory on the filesystem
        new_dir_path = "dir_moved"
        repo.move(dir_path, new_dir_path)

        # Stage the directory move
        result = repo.file_stage_move(dir_path, new_dir_path, offline=True)
        logger.info(f"stage_move directory result: {result}")

        # Verify the staging output indicates a directory move
        assert "moved" in result.lower() or "director" in result.lower(), (
            f"DIAGNOSTIC: stage_move did not record directory move action. "
            f"Expected 'moved' or 'directory' in output, got: {result}"
        )

        # Check status
        status = repo.status(offline=True)
        logger.info(f"Status after directory stage_move: {status}")

        # Verify that the directory is marked as moved but the file is not.
        regex, explanation = status_util.make_regex(
            status_util.Operation.MOVE, dir_path, new_dir_path
        )
        assert re.search(
            regex,
            status,
        ), (
            f"DIAGNOSTIC: Status does not show correct move format with from_path before -> and to_path after. "
            f"Expected pattern: '{explanation}'. Status: {status}"
        )

        # Verify the status output does not contain "V " indicator for files in the moved directory
        old_file_path = posix_join(dir_path, "file_0.txt")
        new_file_path = posix_join(new_dir_path, "file_0.txt")
        regex, explanation = status_util.make_regex(
            status_util.Operation.MOVE, old_file_path, new_file_path
        )
        assert not re.search(regex, status), (
            f"DIAGNOSTIC: Status has erroneous move with from_path -> to_path. "
            f"Expected missing pattern: '{explanation}'. Status: {status}"
        )

    @pytest.mark.smoke
    def test_directory_move_preserves_directory_identity(self, new_lore_repo):
        """REQ-F-15: Directory move preserves directory identity.

        Lore treats directories as first-class objects. The directory's identity
        should be preserved across moves.
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        dir_path = "identity_dir"
        create_directory_with_files(repo, dir_path, 2)

        # Get directory identity before move
        # Note: Directories are first-class objects, so file_info should work on them
        dir_infos = repo.file_info(dir_path, offline=True)
        original_dir_identity = ""
        for info in dir_infos:
            if info.type == "directory" or info.path.rstrip("/") == dir_path:
                original_dir_identity = info.context
                break

        if not original_dir_identity:
            # If we can't get directory identity, check files instead
            logger.warning(
                "DIAGNOSTIC: Could not get directory identity directly. "
                "This may indicate directories are not first-class objects in current implementation."
            )
        else:
            logger.info(f"Original directory identity: {original_dir_identity}")

        # Move the directory
        new_dir_path = "identity_dir_moved"
        repo.move(dir_path, new_dir_path)
        repo.file_stage_move(dir_path, new_dir_path, offline=True)
        repo.commit("Move directory for identity test", offline=True)

        # Get directory identity after move
        new_dir_infos = repo.file_info(new_dir_path, offline=True)
        new_dir_identity = ""
        for info in new_dir_infos:
            if info.type == "directory" or info.path.rstrip("/") == new_dir_path:
                new_dir_identity = info.context
                break

        if original_dir_identity and new_dir_identity:
            assert original_dir_identity == new_dir_identity, (
                f"DIAGNOSTIC: Directory identity changed during move. "
                f"Original: {original_dir_identity}, After move: {new_dir_identity}. "
                f"Directory move operations should preserve directory identity."
            )

    @pytest.mark.smoke
    def test_directory_move_preserves_history(self, new_lore_repo):
        """REQ-F-16: Directory move preserves directory history.

        After a directory move, file_history should show the move.
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        dir_path = "history_dir"
        create_directory_with_files(repo, dir_path, 2)

        # Move the directory
        new_dir_path = "history_dir_moved"
        repo.move(dir_path, new_dir_path)
        repo.file_stage_move(dir_path, new_dir_path, offline=True)
        repo.commit("Move directory for history test", offline=True)

        # Get directory history (file_history works on directories too)
        history_output = repo.file_history(new_dir_path, offline=True)
        logger.info(f"Directory history after move:\n{history_output}")

        # Verify history contains information about the directory
        assert history_output, (
            f"DIAGNOSTIC: No history returned for moved directory '{new_dir_path}'"
        )

    @pytest.mark.smoke
    def test_files_in_moved_directory_update_paths(self, new_lore_repo):
        """REQ-F-17: Files within moved directory update paths.

        When a directory is moved, all contained files should have their
        paths updated to reflect the new location.

        Status output format for move actions is:
        "{action_short} {from_path} -> {to_path} {merged_string}"
        Where action_as_string_short for move is "V"
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        dir_path = "files_update_dir"
        create_directory_with_files(repo, dir_path, 3)

        # Move the directory
        new_dir_path = "files_update_dir_moved"
        repo.move(dir_path, new_dir_path)
        repo.file_stage_move(dir_path, new_dir_path, offline=True)

        repo.commit("Move directory to test file path updates", offline=True)

        # Verify files exist at new paths
        for i in range(3):
            old_file_path = posix_join(dir_path, f"file_{i}.txt")
            new_file_path = posix_join(new_dir_path, f"file_{i}.txt")

            assert not repo.file_exists(old_file_path), (
                f"DIAGNOSTIC: File still exists at old path '{old_file_path}' after directory move"
            )
            assert repo.file_exists(new_file_path), (
                f"DIAGNOSTIC: File does not exist at new path '{new_file_path}' after directory move"
            )

    @pytest.mark.smoke
    def test_files_in_moved_directory_preserve_identity(self, new_lore_repo):
        """REQ-F-18: Files within moved directory preserve identity.

        When a directory is moved, the identity (context) of each file within
        should be preserved - they should not be treated as new files.
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        dir_path = "preserve_identity_dir"
        create_directory_with_files(repo, dir_path, 2)

        # Get file identities before move
        original_identities = {}
        for i in range(2):
            file_path = posix_join(dir_path, f"file_{i}.txt")
            identity = get_file_identity(repo, file_path)
            original_identities[f"file_{i}.txt"] = identity
            logger.info(f"Original identity for {file_path}: {identity}")

        # Move the directory
        new_dir_path = "preserve_identity_dir_moved"
        repo.move(dir_path, new_dir_path)
        repo.file_stage_move(dir_path, new_dir_path, offline=True)
        repo.commit("Move directory to test file identity preservation", offline=True)

        # Verify file identities are preserved
        for i in range(2):
            new_file_path = posix_join(new_dir_path, f"file_{i}.txt")
            new_identity = get_file_identity(repo, new_file_path)
            original_identity = original_identities[f"file_{i}.txt"]

            assert original_identity == new_identity, (
                f"DIAGNOSTIC: File identity changed for '{new_file_path}' during directory move. "
                f"Original: {original_identity}, After: {new_identity}. "
                f"Files in moved directories should preserve their identities."
            )

    @pytest.mark.smoke
    def test_nested_directories_move_correctly(self, new_lore_repo):
        """REQ-F-19: Nested directories move correctly.

        When a directory containing subdirectories is moved, all nested
        content should move correctly.
        """
        repo: Lore = new_lore_repo()

        # Create nested directory structure
        parent_dir = "nested_parent"
        child_dir = posix_join(parent_dir, "child")
        grandchild_dir = posix_join(child_dir, "grandchild")

        repo.make_dirs(grandchild_dir)

        # Create files at each level
        file_in_parent = posix_join(parent_dir, "parent_file.txt")
        file_in_child = posix_join(child_dir, "child_file.txt")
        file_in_grandchild = posix_join(grandchild_dir, "grandchild_file.txt")

        with repo.open_file(file_in_parent, "w") as f:
            f.write("Parent file content\n")
        with repo.open_file(file_in_child, "w") as f:
            f.write("Child file content\n")
        with repo.open_file(file_in_grandchild, "w") as f:
            f.write("Grandchild file content\n")

        repo.file_stage(parent_dir, offline=True)
        repo.commit("Create nested directory structure", offline=True)

        # Get file identities before move
        original_identities = {
            "parent_file.txt": get_file_identity(repo, file_in_parent),
            "child_file.txt": get_file_identity(repo, file_in_child),
            "grandchild_file.txt": get_file_identity(repo, file_in_grandchild),
        }

        # Move the parent directory
        new_parent_dir = "nested_parent_moved"
        repo.move(parent_dir, new_parent_dir)
        repo.file_stage_move(parent_dir, new_parent_dir, offline=True)
        repo.commit("Move nested directory structure", offline=True)

        # Verify all files exist at new locations
        new_file_in_parent = posix_join(new_parent_dir, "parent_file.txt")
        new_file_in_child = posix_join(new_parent_dir, "child", "child_file.txt")
        new_file_in_grandchild = posix_join(
            new_parent_dir, "child", "grandchild", "grandchild_file.txt"
        )

        assert repo.file_exists(new_file_in_parent), (
            f"DIAGNOSTIC: Parent file not found at '{new_file_in_parent}' after nested directory move"
        )
        assert repo.file_exists(new_file_in_child), (
            f"DIAGNOSTIC: Child file not found at '{new_file_in_child}' after nested directory move"
        )
        assert repo.file_exists(new_file_in_grandchild), (
            f"DIAGNOSTIC: Grandchild file not found at '{new_file_in_grandchild}' after nested directory move"
        )

        # Verify identities are preserved
        assert original_identities["parent_file.txt"] == get_file_identity(
            repo, new_file_in_parent
        ), "DIAGNOSTIC: Parent file identity not preserved in nested directory move"
        assert original_identities["child_file.txt"] == get_file_identity(
            repo, new_file_in_child
        ), "DIAGNOSTIC: Child file identity not preserved in nested directory move"
        assert original_identities["grandchild_file.txt"] == get_file_identity(
            repo, new_file_in_grandchild
        ), "DIAGNOSTIC: Grandchild file identity not preserved in nested directory move"

    @pytest.mark.smoke
    def test_directory_move_with_content_changes(self, new_lore_repo):
        """REQ-F-20: Directory move with content changes in files.

        A directory can be moved while also modifying content of files within it.
        This test verifies that file history is correctly maintained when:
        1. A file is created (A prefix)
        2. The file's parent directory is moved (V prefix)
        3. The file is modified after the move (M prefix)

        Expected history (newest to oldest):
        - M new_path (modify action after the move)
        - V new_path (move action showing new location)
        - A original_path (original creation)
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        dir_path = "content_change_dir"
        create_directory_with_files(repo, dir_path, 2)

        # Get original path and identity for the file we'll track
        original_file_path = posix_join(dir_path, "file_0.txt")
        original_identity = get_file_identity(repo, original_file_path)

        # Move the directory
        new_dir_path = "content_change_dir_moved"
        repo.move(dir_path, new_dir_path)

        # Modify a file in the moved directory
        new_file_path = posix_join(new_dir_path, "file_0.txt")
        new_content = "Modified content in moved directory\n"
        with repo.open_file(new_file_path, "w") as f:
            f.write(new_content)

        # Stage the move
        repo.file_stage_move(dir_path, new_dir_path, offline=True)
        repo.commit("Move directory and modify file", offline=True)

        # Verify content is updated
        with repo.open_file(new_file_path, "r") as f:
            actual_content = f.read()
        assert actual_content == new_content, (
            f"DIAGNOSTIC: File content not updated after directory move+modify. "
            f"Expected: {new_content!r}, Got: {actual_content!r}"
        )

        # Verify identity is preserved
        new_identity = get_file_identity(repo, new_file_path)
        assert original_identity == new_identity, (
            f"DIAGNOSTIC: File identity changed during directory move+modify. "
            f"Original: {original_identity}, After: {new_identity}"
        )

        # =================================================================
        # Verify file history is correct after directory move + file modify
        # =================================================================
        #
        # Expected history pattern (newest to oldest):
        # 1. M new_file_path - Modify action after the move
        # 2. V new_file_path - Move action showing new location
        # 3. A original_file_path - Original creation (add action)
        #
        # Note: In the current implementation, the move and modify happen in
        # the same commit, so we may see either:
        # - Two entries: V new_path, A old_path (if modify is absorbed into move)
        # - Three entries: M new_path, V new_path, A old_path (if modify is separate)

        # Use the centralized helper to parse history
        history_output = repo.file_history(new_file_path, offline=True)
        entries = parse_history_entries(history_output)

        logger.info(f"File history for {new_file_path}:")
        for i, entry in enumerate(entries):
            logger.info(f"  [{i}] {entry.action} ({entry.action_name}): {entry.path}")

        # Verify we have at least 2 history entries (move + create)
        assert len(entries) >= 2, (
            f"DIAGNOSTIC: Expected at least 2 history entries (move + create) but found {len(entries)}. "
            f"History output:\n{history_output}"
        )

        # Verify the oldest entry shows the original path with 'A' action (creation)
        oldest_index = len(entries) - 1
        success, message = verify_history_entry_at_index(
            repo, new_file_path, oldest_index, "A", original_file_path
        )
        assert success, (
            f"DIAGNOSTIC: {message}\n"
            f"The oldest history entry should show 'A {original_file_path}' (original creation)."
        )
        logger.info(f"Verified oldest entry [{oldest_index}]: A {original_file_path}")

        # Verify the latest entry shows either:
        # - 'M' action if modify is tracked separately, OR
        # - 'V' action if modify is absorbed into move
        latest_entry = entries[0]
        assert latest_entry.action in ("M", "V"), (
            f"DIAGNOSTIC: Latest history entry should be 'M' (modify) or 'V' (move) "
            f"but got '{latest_entry.action}' ({latest_entry.action_name}). "
            f"Full entry: {latest_entry}"
        )

        # The latest entry path should be the new path
        assert latest_entry.path == new_file_path, (
            f"DIAGNOSTIC: Latest history entry should show new path '{new_file_path}' "
            f"but shows '{latest_entry.path}'"
        )
        logger.info(
            f"Verified latest entry [0]: {latest_entry.action} {latest_entry.path}"
        )

        # Check if there's a separate move entry (when modify is separate from move)
        if latest_entry.action == "M":
            # If latest is 'M', the second entry should be 'V' (move)
            success, message = verify_history_entry_at_index(
                repo, new_file_path, 1, "V", new_file_path
            )
            assert success, (
                f"DIAGNOSTIC: {message}\n"
                f"When modify is separate, entry [1] should show 'V {new_file_path}' (move action)."
            )
            logger.info(f"Verified move entry [1]: V {new_file_path}")
        else:
            # If latest is 'V', this is the move entry
            # Just verify using the move history helper for the basic move pattern
            logger.info(
                f"Move and modify combined in single entry. "
                f"Latest entry shows 'V {new_file_path}'."
            )

        # Additional verification: ensure history shows the complete lineage
        # The history should contain both the original and new paths
        found_paths = {entry.path for entry in entries}
        assert original_file_path in found_paths, (
            f"DIAGNOSTIC: History does not contain original path '{original_file_path}'. "
            f"Found paths: {found_paths}. "
            f"Directory move should preserve file history lineage."
        )
        assert new_file_path in found_paths, (
            f"DIAGNOSTIC: History does not contain new path '{new_file_path}'. "
            f"Found paths: {found_paths}."
        )

        logger.info(
            f"History verification complete. "
            f"File lineage preserved: {original_file_path} -> {new_file_path}"
        )

    def test_status_after_move(self, new_lore_repo):
        """
        Moves a directory with various other changes nested inside an ensures that the changes show up in the status
        correctly

        a/b/
            nested_before/
                file.txt
            nested_after/
                file.txt
            nested_before.txt
            nested_after.txt
            modified.txt
            kept.txt
            deleted.txt

        Move a/b/nested_before -> a/b/nested_before_moved
        Move a/b/nested_before.txt -> a/b/nested_before_moved.txt
        Move a/b -> a/c
        Move a/c/nested_after -> a/c/nested_after_moved
        Move a/c/nested_after.txt -> a/c/nested_after_moved.txt
        Modify a/c/modified.txt
        Delete a/c/deleted.txt
        Add a/c/added.txt
        """
        repo: Lore = new_lore_repo()

        b = Path("a") / "b"
        c = Path("a") / "c"
        nested_before = "nested_before"
        nested_before_moved = "nested_before_moved"
        nested_after = "nested_after"
        nested_after_moved = "nested_after_moved"
        nested_file = "file.txt"
        nested_file_before = "nested_before.txt"
        nested_file_before_moved = "nested_before_moved.txt"
        nested_file_after = "nested_after.txt"
        nested_file_after_moved = "nested_after_moved.txt"
        modified = "modified.txt"
        kept = "kept.txt"
        deleted = "deleted.txt"
        added = "added.txt"
        repo.write_commit_push(
            None,
            {
                b / nested_before / nested_file: os.urandom(1001),
                b / nested_after / nested_file: os.urandom(1002),
                b / nested_file_before: os.urandom(1003),
                b / nested_file_after: os.urandom(1004),
                b / modified: os.urandom(1005),
                b / kept: os.urandom(1006),
                b / deleted: os.urandom(1007),
            },
        )

        repo.stage_move(str(b / nested_before), str(b / nested_before_moved))
        repo.stage_move(str(b / nested_file_before), str(b / nested_file_before_moved))
        repo.stage_move(str(b), str(c))
        repo.stage_move(str(c / nested_after), str(c / nested_after_moved))
        repo.stage_move(str(c / nested_file_after), str(c / nested_file_after_moved))

        repo.remove_file(c / deleted)
        repo.write_files({c / modified: os.urandom(2001), c / added: os.urandom(2002)})

        repo.stage(".", scan=True)

        verify_operations_in_status(
            repo.status(),
            [
                (
                    Operation.MOVE,
                    to_posix(b),
                    to_posix(c),
                ),
                (
                    Operation.MOVE,
                    to_posix(b / nested_before),
                    to_posix(c / nested_before_moved),
                ),
                (
                    Operation.MOVE,
                    to_posix(b / nested_file_before),
                    to_posix(c / nested_file_before_moved),
                ),
                (
                    Operation.MOVE,
                    to_posix(b / nested_after),
                    to_posix(c / nested_after_moved),
                ),
                (
                    Operation.MOVE,
                    to_posix(b / nested_file_after),
                    to_posix(c / nested_file_after_moved),
                ),
                (Operation.MODIFY, to_posix(c / modified)),
                (Operation.DELETE, to_posix(c / deleted)),
                (Operation.ADD, to_posix(c / added)),
            ],
        )


class TestDirectoryCopyCore:
    """
    Tests for core directory copy functionality (REQ-F-21 to REQ-F-26).

    These tests verify directory copy operations including:
    - All files get new identities (unlike move)
    - Directory structure is preserved
    - Source directory remains unchanged
    - Nested directories are handled correctly

    NOTE: The 'repo file stage copy' command is not yet implemented in Lore.
    Directory copy tests use fallback staging when the copy command fails.
    """

    @pytest.mark.smoke
    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist for directories"
    )
    def test_stage_directory_copy(self, new_lore_repo):
        """REQ-F-21: Copy directory stages correctly.

        Verifies that after staging a directory copy, the copy is recorded.
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        source_dir = "source_dir"
        create_directory_with_files(repo, source_dir, 2)

        # Copy the directory on the filesystem
        dest_dir = "dest_dir"
        repo.copy_tree(source_dir, dest_dir)

        # Stage the copy - for directories, we may need to stage each file's copy
        # or use a directory copy command if available
        # First try staging the destination directory
        try:
            # Try to stage as a copy if that command exists
            result = repo.file_stage_copy(source_dir, dest_dir, offline=True)
            logger.info(f"stage_copy directory result: {result}")
        except Exception as e:
            # If stage_copy doesn't work for directories, stage as new files
            logger.warning(
                f"DIAGNOSTIC: file_stage_copy failed for directory: {e}. "
                f"Attempting to stage individual files."
            )
            repo.file_stage(dest_dir, offline=True)
            result = "staged individual files"

        # Check status
        status = repo.status(offline=True)
        logger.info(f"Status after directory copy staging: {status}")

        # The copied directory should appear in staged changes
        assert (
            dest_dir in status
            or "added" in status.lower()
            or "copied" in status.lower()
        ), f"DIAGNOSTIC: Status does not show copied directory. Status: {status}"

    @pytest.mark.smoke
    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist for directories"
    )
    def test_commit_directory_copy_creates_new_directory(self, new_lore_repo):
        """REQ-F-22: Committed directory copy creates new directory.

        After committing a directory copy, both source and destination
        directories should exist with the same structure.
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        source_dir = "copy_source_dir"
        create_directory_with_files(repo, source_dir, 3)

        # Copy the directory
        dest_dir = "copy_dest_dir"
        repo.copy_tree(source_dir, dest_dir)

        # Stage the copy
        try:
            repo.file_stage_copy(source_dir, dest_dir, offline=True)
        except Exception:
            # Fallback: stage destination as new files
            repo.file_stage(dest_dir, offline=True)

        repo.commit("Copy directory to new location", offline=True)

        # Verify both directories exist
        assert repo.path_exists(source_dir), (
            f"DIAGNOSTIC: Source directory '{source_dir}' does not exist after copy"
        )
        assert repo.path_exists(dest_dir), (
            f"DIAGNOSTIC: Destination directory '{dest_dir}' does not exist after copy"
        )

        # Verify files exist in both directories
        for i in range(3):
            source_file = posix_join(source_dir, f"file_{i}.txt")
            dest_file = posix_join(dest_dir, f"file_{i}.txt")

            assert repo.file_exists(source_file), (
                f"DIAGNOSTIC: Source file '{source_file}' does not exist after directory copy"
            )
            assert repo.file_exists(dest_file), (
                f"DIAGNOSTIC: Destination file '{dest_file}' does not exist after directory copy"
            )

    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist for directories"
    )
    def test_copied_directory_has_new_identity(self, new_lore_repo):
        """REQ-F-23: Copied directory has new identity.

        Unlike a move, a copied directory should have a new identity.
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        source_dir = "identity_source_dir"
        create_directory_with_files(repo, source_dir, 2)

        # Get source directory identity if directories have identities
        source_dir_infos = repo.file_info(source_dir, offline=True)
        source_dir_identity = ""
        for info in source_dir_infos:
            if info.type == "directory":
                source_dir_identity = info.context
                break

        if source_dir_identity:
            logger.info(f"Source directory identity: {source_dir_identity}")

        # Copy the directory
        dest_dir = "identity_dest_dir"
        repo.copy_tree(source_dir, dest_dir)

        try:
            repo.file_stage_copy(source_dir, dest_dir, offline=True)
        except Exception:
            repo.file_stage(dest_dir, offline=True)

        repo.commit("Copy directory for identity test", offline=True)

        # Get destination directory identity
        dest_dir_infos = repo.file_info(dest_dir, offline=True)
        dest_dir_identity = ""
        for info in dest_dir_infos:
            if info.type == "directory":
                dest_dir_identity = info.context
                break

        if source_dir_identity and dest_dir_identity:
            assert source_dir_identity != dest_dir_identity, (
                f"DIAGNOSTIC: Directory identities are the same after copy. "
                f"Source: {source_dir_identity}, Destination: {dest_dir_identity}. "
                f"Copy operations should create NEW directory identities."
            )
        else:
            logger.warning(
                "DIAGNOSTIC: Could not get directory identities to compare. "
                "Directories may not be tracked as first-class objects."
            )

    @pytest.mark.smoke
    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist for directories"
    )
    def test_files_in_copied_directory_have_new_identities(self, new_lore_repo):
        """REQ-F-24: Files in copied directory have new identities.

        Unlike a directory move, files in a copied directory should have
        new identities (contexts).
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        source_dir = "file_identity_source"
        create_directory_with_files(repo, source_dir, 2)

        # Get source file identities
        source_identities = {}
        for i in range(2):
            file_path = posix_join(source_dir, f"file_{i}.txt")
            identity = get_file_identity(repo, file_path)
            source_identities[f"file_{i}.txt"] = identity
            logger.info(f"Source identity for {file_path}: {identity}")

        # Copy the directory
        dest_dir = "file_identity_dest"
        repo.copy_tree(source_dir, dest_dir)

        try:
            repo.file_stage_copy(source_dir, dest_dir, offline=True)
        except Exception:
            repo.file_stage(dest_dir, offline=True)

        repo.commit("Copy directory to test file identities", offline=True)

        # Verify file identities are DIFFERENT
        for i in range(2):
            dest_file_path = posix_join(dest_dir, f"file_{i}.txt")
            dest_identity = get_file_identity(repo, dest_file_path)
            source_identity = source_identities[f"file_{i}.txt"]

            assert source_identity != dest_identity, (
                f"DIAGNOSTIC: File identity is the same for '{dest_file_path}' after directory copy. "
                f"Source: {source_identity}, Destination: {dest_identity}. "
                f"Files in copied directories should have NEW identities."
            )

    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist for directories"
    )
    def test_files_in_copied_directory_preserve_content(self, new_lore_repo):
        """REQ-F-25: Files in copied directory preserve content.

        All files in a copied directory should have the same content (hash)
        as their source counterparts.
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        source_dir = "content_source_dir"
        create_directory_with_files(repo, source_dir, 2)

        # Get source content hashes
        source_hashes = {}
        for i in range(2):
            file_path = posix_join(source_dir, f"file_{i}.txt")
            file_infos = repo.file_info(file_path, offline=True)
            if file_infos:
                source_hashes[f"file_{i}.txt"] = file_infos[0].hash
                logger.info(f"Source hash for {file_path}: {file_infos[0].hash}")

        # Copy the directory
        dest_dir = "content_dest_dir"
        repo.copy_tree(source_dir, dest_dir)

        try:
            repo.file_stage_copy(source_dir, dest_dir, offline=True)
        except Exception:
            repo.file_stage(dest_dir, offline=True)

        repo.commit("Copy directory to test content preservation", offline=True)

        # Verify content hashes match
        for i in range(2):
            dest_file_path = posix_join(dest_dir, f"file_{i}.txt")
            dest_infos = repo.file_info(dest_file_path, offline=True)
            if dest_infos:
                dest_hash = dest_infos[0].hash
                source_hash = source_hashes.get(f"file_{i}.txt", "")

                assert source_hash == dest_hash, (
                    f"DIAGNOSTIC: Content hash differs for '{dest_file_path}' after directory copy. "
                    f"Source hash: {source_hash}, Destination hash: {dest_hash}. "
                    f"Directory copy should preserve file content."
                )

    @pytest.mark.skip(
        reason="Lore feature not implemented: 'repo file stage copy' command does not exist for directories"
    )
    def test_source_directory_unchanged_after_copy(self, new_lore_repo):
        """REQ-F-26: Source directory unchanged after copy.

        The source directory and all its files should remain unchanged
        after a copy operation.
        """
        repo: Lore = new_lore_repo()

        # Create directory with files
        source_dir = "unchanged_source_dir"
        create_directory_with_files(repo, source_dir, 2)

        # Get source info before copy
        source_identities = {}
        source_hashes = {}
        for i in range(2):
            file_path = posix_join(source_dir, f"file_{i}.txt")
            source_identities[f"file_{i}.txt"] = get_file_identity(repo, file_path)
            file_infos = repo.file_info(file_path, offline=True)
            if file_infos:
                source_hashes[f"file_{i}.txt"] = file_infos[0].hash

        # Copy the directory
        dest_dir = "copy_of_unchanged"
        repo.copy_tree(source_dir, dest_dir)

        try:
            repo.file_stage_copy(source_dir, dest_dir, offline=True)
        except Exception:
            repo.file_stage(dest_dir, offline=True)

        repo.commit("Copy directory to verify source unchanged", offline=True)

        # Modify the destination to ensure source is independent
        dest_file = posix_join(dest_dir, "file_0.txt")
        with repo.open_file(dest_file, "w") as f:
            f.write("Modified destination content\n")
        repo.file_stage(dest_file, offline=True)
        repo.commit("Modify destination file in copied directory", offline=True)

        # Verify source directory and files are unchanged
        assert repo.path_exists(source_dir), (
            f"DIAGNOSTIC: Source directory '{source_dir}' no longer exists after copy"
        )

        for i in range(2):
            file_path = posix_join(source_dir, f"file_{i}.txt")

            # Verify file exists
            assert repo.file_exists(file_path), (
                f"DIAGNOSTIC: Source file '{file_path}' no longer exists after copy"
            )

            # Verify identity unchanged
            current_identity = get_file_identity(repo, file_path)
            assert source_identities[f"file_{i}.txt"] == current_identity, (
                f"DIAGNOSTIC: Source file identity changed after copy for '{file_path}'. "
                f"Before: {source_identities[f'file_{i}.txt']}, After: {current_identity}"
            )

            # Verify content hash unchanged
            file_infos = repo.file_info(file_path, offline=True)
            if file_infos:
                current_hash = file_infos[0].hash
                assert source_hashes[f"file_{i}.txt"] == current_hash, (
                    f"DIAGNOSTIC: Source file content hash changed after copy for '{file_path}'. "
                    f"Before: {source_hashes[f'file_{i}.txt']}, After: {current_hash}"
                )


# =============================================================================
# Move Merge Tests - Test move operations during merge with status verification
# =============================================================================


class TestMoveMerge:
    """
    Tests for move operations during merge.

    These tests verify move operations during merges by:
    - Using no_commit=True to keep changes staged
    - Verifying status during merge (staged state)
    - Verifying status after merge commit
    - Correct handling of moved files in merge scenarios

    Test scenarios:
    - Move on one branch, no changes on other branch - verify clean merge
    - Move on one branch, modify same file on other branch - verify merge handles correctly
    - Move on both branches to different locations - verify conflict handling
    """

    @pytest.mark.smoke
    def test_move_on_one_branch_no_changes_on_other(self, new_lore_repo):
        """
        Test case: Move on one branch, no changes on other branch - clean merge.

        Scenario:
        1. Create a file on main branch
        2. Create feature branch
        3. Move the file on feature branch
        4. Switch back to main (file at original location)
        5. Merge feature branch with no_commit=True
        6. Verify status shows moved file staged
        7. Commit merge and verify final state
        """
        repo: Lore = new_lore_repo()

        # Setup: Create initial file on main branch
        original_path = "clean_merge_file.txt"
        content = "Content for clean merge test\n"
        create_file_with_content(repo, original_path, content)

        # Get original identity for later verification
        original_identity = get_file_identity(repo, original_path)
        logger.info(f"Original file identity: {original_identity}")

        # Create feature branch and switch to it
        feature_branch = "feature_move_clean"
        repo.branch_create(feature_branch, offline=True)

        # Move the file on feature branch
        new_path = "moved_clean_merge_file.txt"
        repo.move(original_path, new_path)
        repo.file_stage_move(original_path, new_path, offline=True)
        repo.commit("Move file on feature branch", offline=True)

        # Switch back to main branch
        repo.branch_switch("main", offline=True)

        # Verify file is at original location on main
        assert repo.file_exists(original_path), (
            f"File should exist at '{original_path}' on main branch before merge"
        )
        assert not repo.file_exists(new_path), (
            f"File should NOT exist at '{new_path}' on main branch before merge"
        )

        # Merge feature branch with no_commit=True to keep changes staged
        merge_output = repo.branch_merge_start(
            feature_branch, no_commit=True, offline=True, json=True
        )

        # Verify merge succeeded (no errors)
        assert "Error" not in merge_output, (
            f"DIAGNOSTIC: Merge failed unexpectedly. Output: {merge_output}"
        )

        # Check status during merge (staged state) using JSON output
        status_during_merge_raw = repo.status(offline=True, json=True)
        status_entries = parse_status_json(status_during_merge_raw)
        logger.info(f"Status entries during merge: {status_entries}")

        # Verify we have status entries (staged changes exist)
        assert len(status_entries) == 1, (
            f"DIAGNOSTIC: Status should show 1 staged move change during merge. "
            f"Raw status output: {status_during_merge_raw}"
        )

        # Find the entry for the moved file
        moved_file_entry = None
        for entry in status_entries:
            if entry.get("path") == new_path:
                moved_file_entry = entry
                break

        assert moved_file_entry is not None, (
            f"DIAGNOSTIC: Moved file '{new_path}' should appear in status. "
            f"Status entries: {status_entries}"
        )

        # Verify the moved file has correct action and flags using structured JSON data
        assert moved_file_entry.get("action") == "move", (
            f"DIAGNOSTIC: File action should be 'move'. "
            f"Got: {moved_file_entry.get('action')}. Entry: {moved_file_entry}"
        )
        assert moved_file_entry.get("type") == "file", (
            f"DIAGNOSTIC: Entry type should be 'file'. "
            f"Got: {moved_file_entry.get('type')}. Entry: {moved_file_entry}"
        )
        assert moved_file_entry.get("flagStaged") is True, (
            f"DIAGNOSTIC: File should be staged (flagStaged=true). "
            f"Got: {moved_file_entry.get('flagStaged')}. Entry: {moved_file_entry}"
        )
        assert moved_file_entry.get("flagMerged") is True, (
            f"DIAGNOSTIC: File should be marked as merged (flagMerged=true). "
            f"Got: {moved_file_entry.get('flagMerged')}. Entry: {moved_file_entry}"
        )
        assert moved_file_entry.get("flagConflict") is False, (
            f"DIAGNOSTIC: File should not be in conflict (flagConflict=false). "
            f"Got: {moved_file_entry.get('flagConflict')}. Entry: {moved_file_entry}"
        )

        # Commit the merge
        repo.commit("Merge feature branch with moved file", offline=True)

        # Check status after merge commit using JSON output
        status_after_merge_raw = repo.status(offline=True, json=True)
        status_entries_after = parse_status_json(status_after_merge_raw)
        logger.info(f"Status entries after merge: {status_entries_after}")

        # After a clean commit, there should be no staged/pending changes
        assert len(status_entries_after) == 0, (
            f"Status is not clean after merge commit, got entries: {status_entries_after}"
        )

        # Verify final state - file should be at new location
        assert repo.file_exists(new_path), (
            f"DIAGNOSTIC: File should exist at '{new_path}' after merge commit"
        )
        assert not repo.file_exists(original_path), (
            f"DIAGNOSTIC: File should NOT exist at '{original_path}' after merge commit"
        )

        # Verify file identity is preserved through merge
        merged_identity = get_file_identity(repo, new_path)
        assert original_identity == merged_identity, (
            f"DIAGNOSTIC: File identity should be preserved through merge. "
            f"Original: {original_identity}, After merge: {merged_identity}"
        )

        # Verify content is preserved
        with repo.open_file(new_path, "r") as f:
            merged_content = f.read()
        assert merged_content == content, (
            f"DIAGNOSTIC: File content should be preserved through merge. "
            f"Expected: {content!r}, Got: {merged_content!r}"
        )

    @pytest.mark.smoke
    def test_move_conflicts_with_add_different_content_at_target(self, new_lore_repo):
        """
        Test: Move file conflicts with add of different file at same target location.

        Scenario:
        1. Create a file `source.txt` with content "original content" on main branch
        2. Create a feature branch
        3. On feature branch: Move `source.txt` to `target.txt`
        4. On main branch: Add a NEW file `target.txt` with DIFFERENT content ("different content")
        5. Merge feature branch into main with no_commit=True

        Expected Behavior:
        This MUST result in a **conflict** because:
        - The move wants to place a file at `target.txt`
        - A different file already exists at `target.txt` with different content

        Test Requirements:
        1. Assert that a **conflict IS reported** (check for "conflicted" in merge output or "!" in status)
        2. Use `no_commit=True` for the merge
        3. Abort the merge after verifying conflict detection
        """
        repo: Lore = new_lore_repo()

        # Setup: Create source.txt with content "original content" on main branch
        source_path = "source.txt"
        target_path = "target.txt"
        original_content = "original content"
        moved_content = "moved content"
        different_content = "different content"

        # Create source.txt on main branch
        with repo.open_file(source_path, "w+") as f:
            f.write(original_content)
        repo.file_stage(source_path, offline=True)
        repo.commit("Create source.txt with original content", offline=True)

        logger.info(f"Created '{source_path}' with content: {original_content!r}")

        # Create feature branch and switch to it
        feature_branch = "feature_move_to_target"
        repo.branch_create(feature_branch, offline=True)

        # On feature branch: Edit source.txt
        with repo.open_file(source_path, "w+") as f:
            f.write(moved_content)
        repo.file_stage(source_path, offline=True)
        repo.commit("Edit source.txt", offline=True)

        # On feature branch: Move source.txt to target.txt
        repo.move(source_path, target_path)
        repo.file_stage_move(source_path, target_path, offline=True)
        repo.commit("Move source.txt to target.txt", offline=True)

        logger.info(f"Moved '{source_path}' to '{target_path}' on feature branch")

        # Verify on feature branch: source.txt does not exist, target.txt exists
        assert not repo.file_exists(source_path), (
            "source.txt should NOT exist on feature branch after move"
        )
        assert repo.file_exists(target_path), (
            "target.txt should exist on feature branch after move"
        )

        # Switch back to main branch
        repo.branch_switch("main", offline=True)

        # Verify on main: source.txt exists, target.txt does NOT exist yet
        assert repo.file_exists(source_path), "source.txt should exist on main branch"
        assert not repo.file_exists(target_path), (
            "target.txt should NOT exist on main branch before adding"
        )

        # On main branch: Add a NEW file target.txt with DIFFERENT content
        with repo.open_file(target_path, "w+") as f:
            f.write(different_content)
        repo.file_stage(target_path, offline=True)
        repo.commit("Add target.txt with different content", offline=True)

        logger.info(
            f"Added '{target_path}' with content: {different_content!r} on main branch"
        )

        # Verify on main: both source.txt and target.txt exist
        assert repo.file_exists(source_path), (
            "source.txt should still exist on main branch"
        )
        assert repo.file_exists(target_path), (
            "target.txt should exist on main branch after adding"
        )

        # Merge feature branch with no_commit=True to inspect merge state
        merge_output = repo.branch_merge_start(
            feature_branch, no_commit=True, offline=True, json=True
        )

        # Check status during merge
        status_during_merge = repo.status(offline=True, json=True)

        # =================================================================
        # ASSERT: A conflict MUST be reported
        # =================================================================
        # When a move wants to place a file at a location where a DIFFERENT
        # file already exists with different content, this MUST be a conflict.
        #
        # Parse JSON output to check for conflicts

        # Parse merge output - look for branchMergeStartEnd with hasConflicts > 0
        has_conflicts_in_output = False
        for line in merge_output.strip().split("\n"):
            try:
                data = json.loads(line)
                if data.get("tagName") == "branchMergeStartEnd":
                    file_data = data.get("data", {})
                    has_conflicts_in_output = file_data.get("hasConflicts", 0) > 0
                    break
            except json.JSONDecodeError:
                continue

        # Parse status output - look for files with flagConflict=true and flagConflictUnresolved=true
        has_conflicts_in_status = False
        for line in status_during_merge.strip().split("\n"):
            try:
                data = json.loads(line)
                if data.get("tagName") == "repositoryStatusFile":
                    file_data = data.get("data", {})
                    if file_data.get("flagConflict"):
                        has_conflicts_in_status = True
                        break
            except json.JSONDecodeError:
                continue

        logger.info(
            f"Conflict detection: merge_output_has_conflict={has_conflicts_in_output}, "
            f"status_has_conflict_indicator={has_conflicts_in_status}"
        )

        # Assert that a conflict IS reported
        assert has_conflicts_in_output and has_conflicts_in_status, (
            f"EXPECTED BEHAVIOR NOT MET: Move to existing path with DIFFERENT content MUST result in a conflict.\n"
            f"Feature branch moved: {source_path} -> {target_path}\n"
            f"Main branch added: {target_path} with different content\n"
            f"Move content: {original_content!r}\n"
            f"Existing content: {different_content!r}\n"
            f"This situation requires manual resolution - Lore should NOT auto-resolve.\n"
            f"Merge output: {merge_output}\n"
            f"Status during merge:\n{status_during_merge}\n"
            f"\n"
            f"NOTE: If this test fails, Lore's merge logic needs to be updated to\n"
            f"detect move-to-existing-path-with-different-content as a conflict."
        )

        logger.info(
            "SUCCESS: Move to existing path with different content correctly detected as conflict."
        )

        # Abort the merge to leave the repository in a clean state
        repo.branch_merge_abort(offline=True)

        # Verify we're back to a clean state after abort
        status_after_abort = repo.status(offline=True)

    @pytest.mark.smoke
    def test_move_merges_cleanly_with_add_same_content_at_target(self, new_lore_repo):
        """
        Test: Move file merges cleanly with add of identical content file at same target location.

        Scenario:
        1. Create a file `source.txt` with content "same content" on main branch
        2. Create a feature branch
        3. On feature branch: Move `source.txt` to `target.txt`
        4. On main branch: Add a NEW file `target.txt` with the EXACT SAME content ("same content")
        5. Merge feature branch into main with no_commit=True

        Expected Behavior:
        This should merge **cleanly without conflict** because:
        - Both branches result in `target.txt` with identical content
        - The content is the same, so there's no real conflict

        Test Requirements:
        1. Assert that **NO conflict** occurs (no "conflicted" in merge output, no "!" in status)
        2. Use `no_commit=True` for the merge to verify staged state
        3. Verify `target.txt` exists with the correct content
        4. Verify `source.txt` does NOT exist (it was moved/deleted)
        5. Commit the merge and verify final state
        """
        repo: Lore = new_lore_repo()

        # Setup: Create source.txt with content "same content" on main branch
        source_path = "source.txt"
        target_path = "target.txt"
        same_content = "same content"

        # Create source.txt on main branch
        with repo.open_file(source_path, "w") as f:
            f.write(same_content)
        repo.file_stage(source_path, offline=True)
        repo.commit("Create source.txt with same content", offline=True)

        logger.info(f"Created '{source_path}' with content: {same_content!r}")

        # Create feature branch and switch to it
        feature_branch = "feature_move_to_target_same_content"
        repo.branch_create(feature_branch, offline=True)

        # On feature branch: Move source.txt to target.txt
        repo.move(source_path, target_path)
        repo.file_stage_move(source_path, target_path, offline=True)
        repo.commit("Move source.txt to target.txt", offline=True)

        logger.info(f"Moved '{source_path}' to '{target_path}' on feature branch")

        # Verify on feature branch: source.txt does not exist, target.txt exists
        assert not repo.file_exists(source_path), (
            "source.txt should NOT exist on feature branch after move"
        )
        assert repo.file_exists(target_path), (
            "target.txt should exist on feature branch after move"
        )

        # Switch back to main branch
        repo.branch_switch("main", offline=True)

        # Verify on main: source.txt exists, target.txt does NOT exist yet
        assert repo.file_exists(source_path), "source.txt should exist on main branch"
        assert not repo.file_exists(target_path), (
            "target.txt should NOT exist on main branch before adding"
        )

        # On main branch: Add a NEW file target.txt with the EXACT SAME content
        with repo.open_file(target_path, "w") as f:
            f.write(same_content)
        repo.file_stage(target_path, offline=True)
        repo.commit("Add target.txt with same content", offline=True)

        logger.info(
            f"Added '{target_path}' with content: {same_content!r} on main branch"
        )

        # Verify on main: both source.txt and target.txt exist
        assert repo.file_exists(source_path), (
            "source.txt should still exist on main branch"
        )
        assert repo.file_exists(target_path), (
            "target.txt should exist on main branch after adding"
        )

        # Merge feature branch with no_commit=True to inspect merge state
        merge_output = repo.branch_merge_start(
            feature_branch, no_commit=True, offline=True, check=False
        )
        logger.info(f"Merge output: {merge_output}")

        # Check status during merge
        status_during_merge = repo.status(offline=True)
        logger.info(f"Status during merge:\n{status_during_merge}")

        # =================================================================
        # ASSERT: NO conflict should occur
        # =================================================================
        # When a move wants to place a file at a location where a file with
        # IDENTICAL content already exists, this should NOT be a conflict.
        # Both branches result in the same final state.

        # Check for non-zero conflict count in merge output
        conflict_match = re.search(r"(\d+)\s+conflicted", merge_output.lower())
        has_conflicts_in_output = conflict_match and int(conflict_match.group(1)) > 0

        # Check for conflict indicator in status
        has_conflicts_in_status = "!" in status_during_merge

        logger.info(
            f"Conflict detection: merge_output_has_conflict={has_conflicts_in_output}, "
            f"status_has_conflict_indicator={has_conflicts_in_status}"
        )

        # Assert that NO conflict is reported
        assert not has_conflicts_in_output, (
            f"EXPECTED BEHAVIOR NOT MET: Move to existing path with SAME content should NOT conflict.\n"
            f"Feature branch moved: {source_path} -> {target_path}\n"
            f"Main branch added: {target_path} with same content\n"
            f"Both branches have identical content at {target_path}: {same_content!r}\n"
            f"This should merge cleanly without conflict.\n"
            f"Merge output: {merge_output}\n"
            f"\n"
            f"NOTE: If this test fails, Lore's merge logic may need to be updated to\n"
            f"allow clean merge when move target has identical content."
        )

        assert not has_conflicts_in_status, (
            f"EXPECTED BEHAVIOR NOT MET: Status should NOT show conflict indicators.\n"
            f"Status during merge:\n{status_during_merge}\n"
            f"Expected: No '!' conflict indicators in status."
        )

        # =================================================================
        # Verify target.txt exists with the correct content
        # =================================================================
        assert repo.file_exists(target_path), (
            f"EXPECTED BEHAVIOR NOT MET: '{target_path}' should exist after merge.\n"
            f"Both branches converge on having this file with identical content."
        )

        with repo.open_file(target_path, "r") as f:
            actual_content = f.read()

        assert actual_content == same_content, (
            f"EXPECTED BEHAVIOR NOT MET: '{target_path}' should have the expected content.\n"
            f"Expected: {same_content!r}\n"
            f"Got: {actual_content!r}"
        )

        # =================================================================
        # Verify source.txt does NOT exist (it was moved/deleted)
        # =================================================================
        assert not repo.file_exists(source_path), (
            f"EXPECTED BEHAVIOR NOT MET: '{source_path}' should NOT exist after merge.\n"
            f"The file was moved to '{target_path}' on the feature branch.\n"
            f"After merge, only '{target_path}' should exist."
        )

        # =================================================================
        # Commit the merge and verify final state
        # =================================================================
        repo.commit(
            "Merge feature branch with move to same content target", offline=True
        )

        # Check status after merge commit
        status_after_merge = repo.status(offline=True)
        logger.info(f"Status after merge commit:\n{status_after_merge}")

        # Verify final state after commit

        # 1. target.txt should exist with correct content
        assert repo.file_exists(target_path), (
            f"'{target_path}' should exist after merge commit"
        )

        with repo.open_file(target_path, "r") as f:
            final_content = f.read()

        assert final_content == same_content, (
            f"'{target_path}' content should match after merge commit.\n"
            f"Expected: {same_content!r}\n"
            f"Got: {final_content!r}"
        )

        # 2. source.txt should NOT exist
        assert not repo.file_exists(source_path), (
            f"'{source_path}' should NOT exist after merge commit"
        )

        logger.info(
            f"SUCCESS: Move to existing path with same content merged cleanly.\n"
            f"'{target_path}' exists with content: {same_content!r}\n"
            f"'{source_path}' correctly does not exist."
        )

    @pytest.mark.smoke
    def test_move_on_one_branch_modify_same_file_on_other(self, new_lore_repo):
        """
        Test case: Move on one branch, modify same file on other branch.

        Scenario:
        1. Create a file on main branch
        2. Create feature branch
        3. Move the file on feature branch
        4. Switch back to main and modify the same file (at original location)
        5. Merge feature branch with no_commit=True
        6. Verify merge completes without conflicts
        7. Verify file is at new location with modified content
        8. Commit merge and verify final state

        Expected behavior: Lore should recognize that the modified file on main
        is the same file that was moved on the feature branch (by identity/context),
        and the merge should apply both the move AND the modifications cleanly.
        """
        repo: Lore = new_lore_repo()

        # Setup: Create initial file on main branch
        original_path = "move_modify_file.txt"
        original_content = "Original content line 1\nOriginal content line 2\n"
        create_file_with_content(repo, original_path, original_content)

        # Get original identity for later verification
        original_identity = get_file_identity(repo, original_path)
        logger.info(f"Original file identity: {original_identity}")

        # Create feature branch and switch to it
        feature_branch = "feature_move_modify"
        repo.branch_create(feature_branch, offline=True)

        # Move the file on feature branch
        new_path = "moved_move_modify_file.txt"
        repo.move(original_path, new_path)
        repo.file_stage_move(original_path, new_path, offline=True)
        repo.commit("Move file on feature branch", offline=True)

        # Switch back to main branch
        repo.branch_switch("main", offline=True)

        # Verify file is at original location on main
        assert repo.file_exists(original_path), (
            f"File should exist at '{original_path}' on main branch"
        )

        # Modify the file on main branch (at original location)
        modified_content = (
            "Modified content line 1\nOriginal content line 2\nNew line 3\n"
        )
        with repo.open_file(original_path, "w") as f:
            f.write(modified_content)
        repo.file_stage(original_path, offline=True)
        repo.commit("Modify file on main branch", offline=True)

        # Merge feature branch with no_commit=True to keep changes staged, using JSON output
        merge_output = repo.branch_merge_start(
            feature_branch, no_commit=True, offline=True, json=True
        )

        # Parse merge output to check for conflicts
        merge_result = parse_merge_json(merge_output)

        # Check status during merge (staged state) using JSON output
        status_during_merge_raw = repo.status(offline=True, json=True)
        status_entries = parse_status_json(status_during_merge_raw)
        logger.info(f"Status entries during merge: {status_entries}")

        # =================================================================
        # Verify the new_path is marked with correct action and flags
        # =================================================================
        # JSON status format:
        # {"tagName":"repositoryStatusFile","data":{"path":"...","action":"move","type":"file",
        #  "flagStaged":true,"flagMerged":true,"flagConflict":false,...}}
        #
        # Find the entry for the moved file
        moved_file_entry = None
        for entry in status_entries:
            if entry.get("path") == new_path:
                moved_file_entry = entry
                break

        assert moved_file_entry is not None, (
            f"DIAGNOSTIC: Status should contain an entry for '{new_path}'. "
            f"Status entries: {status_entries}. "
            f"Raw status output:\n{status_during_merge_raw}"
        )

        # Verify the moved file has correct action and flags using structured JSON data
        assert moved_file_entry.get("action") == "move", (
            f"DIAGNOSTIC: File action should be 'move'. "
            f"Got: {moved_file_entry.get('action')}. Entry: {moved_file_entry}"
        )
        assert moved_file_entry.get("type") == "file", (
            f"DIAGNOSTIC: Entry type should be 'file'. "
            f"Got: {moved_file_entry.get('type')}. Entry: {moved_file_entry}"
        )
        assert moved_file_entry.get("flagStaged") is True, (
            f"DIAGNOSTIC: File should be staged (flagStaged=true). "
            f"Got: {moved_file_entry.get('flagStaged')}. Entry: {moved_file_entry}"
        )
        assert moved_file_entry.get("flagMerged") is True, (
            f"DIAGNOSTIC: File should be marked as merged (flagMerged=true). "
            f"Got: {moved_file_entry.get('flagMerged')}. Entry: {moved_file_entry}"
        )
        moved_from_path = moved_file_entry.get("fromPath")
        assert moved_from_path == original_path, (
            f"DIAGNOSTIC: File should be marked as move from {original_path} "
            f"Got: {moved_from_path}. Entry: {moved_file_entry}"
        )

        logger.info(
            f"Verified: '{new_path}' has action='move', flagStaged=true, flagMerged=true. "
            f"Entry: {moved_file_entry}"
        )

        # =================================================================
        # Assert no conflicts occurred - the merge should be clean
        # =================================================================
        # Check conflict count from parsed merge result
        conflict_count = merge_result.get("hasConflicts", 0)
        assert conflict_count == 0, (
            f"DIAGNOSTIC: Merge should complete without conflicts. "
            f"Move + modify on same file should merge cleanly. Found {conflict_count} conflicts. "
            f"Merge result: {merge_result}"
        )

        # Verify no files have flagConflict=true in status
        conflicted_entries = [
            e for e in status_entries if e.get("flagConflict") is True
        ]
        assert len(conflicted_entries) == 0, (
            f"DIAGNOSTIC: Status should not show any conflicted files. "
            f"Found {len(conflicted_entries)} conflicted entries: {conflicted_entries}"
        )

        # Verify file exists at new path (from the move) during staged state
        assert repo.file_exists(new_path), (
            f"DIAGNOSTIC: File should exist at new path '{new_path}' after merge (staged state). "
            f"The move from the feature branch should be applied."
        )

        # Verify file does NOT exist at old path during staged state
        assert not repo.file_exists(original_path), (
            f"DIAGNOSTIC: File should NOT exist at original path '{original_path}' after merge (staged state). "
            f"The file was moved to '{new_path}'."
        )

        # Verify file content matches the modified content from main branch
        with repo.open_file(new_path, "r") as f:
            actual_content = f.read()
        assert actual_content == modified_content, (
            f"DIAGNOSTIC: File content at '{new_path}' should match the modified content from main branch. "
            f"Expected: {modified_content!r}, Got: {actual_content!r}"
        )

        # Commit the merge
        repo.commit(
            "Merge feature branch with moved file (move + modify)", offline=True
        )

        # Check status after merge commit using JSON output
        status_after_merge_raw = repo.status(offline=True, json=True)
        status_entries_after = parse_status_json(status_after_merge_raw)
        logger.info(f"Status entries after merge commit: {status_entries_after}")

        # Verify final state after commit - file should be at new location
        assert repo.file_exists(new_path), (
            f"DIAGNOSTIC: File should exist at '{new_path}' after merge commit"
        )

        # Verify file does NOT exist at old path after commit
        assert not repo.file_exists(original_path), (
            f"DIAGNOSTIC: File should NOT exist at original path '{original_path}' after merge commit"
        )

        # Verify file content matches the modified content after commit
        with repo.open_file(new_path, "r") as f:
            final_content = f.read()
        assert final_content == modified_content, (
            f"DIAGNOSTIC: File content at '{new_path}' should match the modified content after commit. "
            f"Expected: {modified_content!r}, Got: {final_content!r}"
        )

        # Verify file identity is preserved through merge
        merged_identity = get_file_identity(repo, new_path)
        assert original_identity == merged_identity, (
            f"DIAGNOSTIC: File identity should be preserved through merge. "
            f"Original: {original_identity}, After merge: {merged_identity}"
        )

        logger.info(
            f"SUCCESS: Move + modify merge completed cleanly. "
            f"File moved from '{original_path}' to '{new_path}' with modified content preserved."
        )

    @pytest.mark.smoke
    def test_move_and_modify_on_one_branch_modify_same_file_on_other_is_conflict(
        self, new_lore_repo
    ):
        """
        Test case: Modify and move on one branch, modify same file on other branch.

        Scenario:
        1. Create a file on main branch
        2. Create feature branch
        3. Modify and move the file on feature branch
        4. Switch back to main and modify the same file (at original location)
        5. Merge feature branch with no_commit=True
        6. Verify merge results in a conflict

        Expected behavior: Lore should recognize that both branches modified the
        file content differently, and one also moved it. Since the content was
        changed on both branches (divergent modifications), this is a conflict
        that requires manual resolution.
        """
        repo: Lore = new_lore_repo()

        # Setup: Create initial file on main branch
        original_path = "move_modify_file.txt"
        original_content = "Original content line 1\nOriginal content line 2\n"
        create_file_with_content(repo, original_path, original_content)

        # Get original identity for later verification
        original_identity = get_file_identity(repo, original_path)
        logger.info(f"Original file identity: {original_identity}")

        # Create feature branch and switch to it
        feature_branch = "feature_move_modify"
        repo.branch_create(feature_branch, offline=True)

        # Modify the file on the feature branch
        modified_branch_content = "Modified content line 1 on branch\nOriginal content line 2\nNew line 3 on branch\n"
        with repo.open_file(original_path, "w") as f:
            f.write(modified_branch_content)
        repo.file_stage(original_path, offline=True)
        repo.commit("Modify file on feature branch", offline=True)

        # Move the file on feature branch
        new_path = "moved_move_modify_file.txt"
        repo.move(original_path, new_path)
        repo.file_stage_move(original_path, new_path, offline=True)
        repo.commit("Move file on feature branch", offline=True)

        # Switch back to main branch
        repo.branch_switch("main", offline=True)

        # Verify file is at original location on main
        assert repo.file_exists(original_path), (
            f"File should exist at '{original_path}' on main branch"
        )

        # Modify the file on main branch (at original location)
        modified_main_content = (
            "Modified content line 1\nOriginal content line 2\nNew line 3\n"
        )
        with repo.open_file(original_path, "w") as f:
            f.write(modified_main_content)
        repo.file_stage(original_path, offline=True)
        repo.commit("Modify file on main branch", offline=True)

        # Merge feature branch with no_commit=True, using check=False since
        # we expect the merge to report conflicts
        merge_output = repo.branch_merge_start(
            feature_branch, no_commit=True, offline=True, json=True, check=False
        )

        # Parse merge output to check for conflicts
        merge_result = parse_merge_json(merge_output)

        # Check status during merge (staged state) using JSON output
        status_during_merge_raw = repo.status(offline=True, json=True)
        status_entries = parse_status_json(status_during_merge_raw)
        logger.info(f"Status entries during merge: {status_entries}")

        # =================================================================
        # ASSERT: Move+modify on feature vs modify on main MUST conflict
        # =================================================================
        # Both branches modified the file content differently. One branch
        # also moved it. The divergent content modifications make this a
        # conflict requiring manual resolution.

        has_conflict_in_merge = merge_result.get("hasConflicts", 0) > 0
        conflicted_entries = [
            e for e in status_entries if e.get("flagConflict") is True
        ]
        has_conflict_in_status = len(conflicted_entries) > 0

        logger.info(
            f"Conflict detection: merge_has_conflict={has_conflict_in_merge}, "
            f"status_has_conflict={has_conflict_in_status}, "
            f"conflicted_entries={conflicted_entries}"
        )

        assert has_conflict_in_merge and has_conflict_in_status, (
            f"EXPECTED BEHAVIOR NOT MET: Move+modify on feature branch vs modify on "
            f"main branch MUST result in a conflict.\n"
            f"Feature branch: modified and moved '{original_path}' to '{new_path}'\n"
            f"Main branch: modified '{original_path}'\n"
            f"Both branches changed content differently - this requires manual resolution.\n"
            f"Merge result: {merge_result}\n"
            f"Status entries: {status_entries}"
        )

        assert has_conflict_in_status, (
            f"Status MUST show flagConflict=true for the conflicting file.\n"
            f"Status entries: {status_entries}\n"
            f"Expected: At least one entry with flagConflict=true."
        )

        # Abort the merge to leave the repository in a clean state
        repo.branch_merge_abort(offline=True)

        # Verify we're back to a clean state after abort
        status_after_abort_raw = repo.status(offline=True, json=True)
        status_entries_after = parse_status_json(status_after_abort_raw)
        logger.info(f"Status entries after merge abort: {status_entries_after}")

        # File should still be at original path on main branch after abort
        assert repo.file_exists(original_path), (
            f"File should exist at '{original_path}' after merge abort (main branch state)"
        )

    @pytest.mark.smoke
    def test_move_on_both_branches_to_different_locations(self, new_lore_repo):
        """
        Test case: Move on both branches to different locations - MUST conflict.

        Scenario:
        1. Create a file on main branch
        2. Create feature branch
        3. Move the file to location A on feature branch
        4. Switch back to main and move same file to location B
        5. Merge feature branch with no_commit=True
        6. ASSERT that merge reports a conflict
        7. Verify status shows conflict indicator ('!')

        Expected behavior: When both branches move the same file to DIFFERENT
        locations, this MUST be detected as a conflict. The merge should NOT
        automatically resolve this - it requires manual intervention to decide
        which location should be kept.
        """
        repo: Lore = new_lore_repo()

        # Setup: Create initial file on main branch
        original_path = "divergent_move_file.txt"
        content = "Content for divergent move test\n"
        create_file_with_content(repo, original_path, content)

        # Get original identity for later verification
        original_identity = get_file_identity(repo, original_path)
        logger.info(f"Original file identity: {original_identity}")

        # Create feature branch and switch to it
        feature_branch = "feature_move_divergent"
        repo.branch_create(feature_branch, offline=True)

        # Move the file to location A on feature branch
        path_a = "location_a/divergent_file.txt"
        repo.make_dirs("location_a")
        repo.move(original_path, path_a)
        repo.file_stage_move(original_path, path_a, offline=True)
        repo.commit("Move file to location A on feature branch", offline=True)

        # Switch back to main branch
        repo.branch_switch("main", offline=True)

        # Verify file is at original location on main
        assert repo.file_exists(original_path), (
            f"File should exist at '{original_path}' on main branch"
        )

        # Move the same file to location B on main branch
        path_b = "location_b/divergent_file.txt"
        repo.make_dirs("location_b")
        repo.move(original_path, path_b)
        repo.file_stage_move(original_path, path_b, offline=True)
        repo.commit("Move file to location B on main branch", offline=True)

        # Verify file is at location B on main
        assert repo.file_exists(path_b), (
            f"File should exist at '{path_b}' on main branch after move"
        )

        # Merge feature branch with no_commit=True to keep changes staged, using JSON output
        merge_output = repo.branch_merge_start(
            feature_branch, no_commit=True, offline=True, json=True, check=False
        )

        # Parse merge output to check for conflicts
        merge_result = parse_merge_json(merge_output)

        # Check status during merge (staged state) using JSON output
        status_during_merge_raw = repo.status(offline=True, json=True)
        status_entries = parse_status_json(status_during_merge_raw)
        logger.info(f"Status entries during merge: {status_entries}")

        # =================================================================
        # ASSERT: Divergent moves MUST result in a conflict
        # =================================================================
        # When the same file is moved to different locations on different
        # branches, this is an unresolvable situation that requires human
        # decision. Lore MUST detect this as a conflict.
        #
        # Check for conflict indicators in either:
        # 1. The merge result JSON (hasConflicts > 0)
        # 2. The status entries (flagConflict=true on one or more entries)

        has_conflict_in_merge = merge_result.get("hasConflicts", 0) > 0
        conflicted_entries = [
            e for e in status_entries if e.get("flagConflict") is True
        ]
        has_conflict_in_status = len(conflicted_entries) > 0

        logger.info(
            f"Conflict detection: merge_has_conflict={has_conflict_in_merge}, "
            f"status_has_conflict={has_conflict_in_status}, "
            f"conflicted_entries={conflicted_entries}"
        )

        # Assert that a conflict IS reported - this is the expected behavior
        # for divergent moves (same file moved to different destinations)
        assert has_conflict_in_merge and has_conflict_in_status, (
            f"EXPECTED BEHAVIOR NOT MET: Divergent moves MUST result in a conflict.\n"
            f"Branch A moved file to: {path_a}\n"
            f"Branch B moved file to: {path_b}\n"
            f"This situation requires manual resolution - Lore should NOT auto-resolve.\n"
            f"Merge result: {merge_result}\n"
            f"Status entries: {status_entries}\n"
            f"\n"
            f"NOTE: If this test fails, Lore's merge logic needs to be updated to\n"
            f"detect divergent moves as conflicts."
        )

        # Additional verification: status should show conflict flag for the conflicted file(s)
        assert has_conflict_in_status, (
            f"Status MUST show flagConflict=true for divergent moves.\n"
            f"Status entries: {status_entries}\n"
            f"Expected: At least one entry with flagConflict=true."
        )

        # Abort the merge to leave the repository in a clean state
        # We do NOT resolve the conflict in this test - the purpose is only
        # to verify that the conflict IS detected
        repo.branch_merge_abort(offline=True)

        # Verify we're back to a clean state after abort
        status_after_abort_raw = repo.status(offline=True, json=True)
        status_entries_after = parse_status_json(status_after_abort_raw)
        logger.info(f"Status entries after merge abort: {status_entries_after}")

        # File should still be at location B (main branch's location) after abort
        assert repo.file_exists(path_b), (
            f"File should exist at '{path_b}' after merge abort (main branch state)"
        )


class TestFileMerge:
    """
    Tests for file move/copy during merge (REQ-F-27 to REQ-F-34).

    These tests verify how moves/copies interact with merge operations:
    - Moved files are correctly identified during merge
    - File identity is used for merge tracking (not path)
    - Move + modify scenarios
    - Cross-branch move detection
    """

    pass


class TestDirectoryMerge:
    """
    Tests for directory move/copy during merge (REQ-F-35 to REQ-F-41).

    These tests verify how directory operations interact with merge:
    - Moved directories are correctly tracked
    - Files inside moved directories merge correctly
    - Partial directory moves
    - Directory rename during merge
    """

    pass


@pytest.mark.smoke
class TestFileMoveConflicts:
    """
    Tests for file move conflict scenarios (REQ-F-42 to REQ-F-53).

    These tests verify conflict handling for file moves:
    - Same file moved to different locations on different branches
    - File modified on one branch, moved on another
    - File deleted on one branch, moved on another
    - Move to same destination from different sources
    """

    pass


class TestDirectoryMoveConflicts:
    """
    Tests for directory move conflict scenarios (REQ-F-54 to REQ-F-63).

    These tests verify conflict handling for directory moves:
    - Same directory moved to different locations
    - Directory modified on one branch, moved on another
    - Partial moves causing conflicts
    - Nested directory conflicts
    """

    pass


class TestSyncBranchSwitch:
    """
    Tests for sync and branch switch with moves (REQ-F-64 to REQ-F-74).

    These tests verify move tracking during sync/switch operations:
    - Sync to revision with moved files
    - Branch switch with pending move
    - Forward-changes with moved files
    - Reset behavior with moved files
    """

    pass


class TestCherryPick:
    """
    Tests for cherry-pick with moves (REQ-F-75 to REQ-F-81).

    These tests verify how cherry-pick handles moves:
    - Cherry-pick a commit that moves a file
    - Cherry-pick a commit that modifies a moved file
    - Cherry-pick onto a branch where file was moved
    - Conflict detection during cherry-pick with moves
    """

    pass


class TestNestedDirectory:
    """
    Tests for nested directory operations (REQ-F-82 to REQ-F-88).

    These tests verify complex nested directory scenarios:
    - Move directory into another directory
    - Move parent and child directories
    - Multiple levels of nesting
    - Flatten nested structure
    """

    pass


class TestEdgeCases:
    """
    Tests for edge cases and boundary conditions (REQ-F-89 to REQ-F-97).

    These tests verify unusual or boundary scenarios:
    - Move file to same path (no-op)
    - Move to path that was previously used
    - Case-sensitivity issues
    - Very long path names
    - Special characters in paths
    - Empty directories
    - Symbolic links (if supported)
    - Large number of files
    - Move during partial sync
    """

    pass
