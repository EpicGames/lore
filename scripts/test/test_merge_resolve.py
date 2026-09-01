# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import logging

import pytest

from lore import Lore
from lore_parsers import parse_status_json

logger = logging.getLogger(__name__)


def parse_resolve_events(output: str) -> list[str]:
    """Parse JSON output from a resolve command and return the list of resolved file paths."""
    resolved = []
    for line in output.strip().split("\n"):
        line = line.strip()
        if not line:
            continue
        try:
            parsed = json.loads(line)
            if (
                parsed.get("tagName") == "branchMergeResolveFile"
                and "data" in parsed
            ):
                resolved.append(parsed["data"]["path"])
        except (json.JSONDecodeError, KeyError):
            continue
    return resolved


def setup_merge_conflict(repo: Lore, files: dict[str, tuple[list[str], list[str], list[str]]]):
    """Set up a merge conflict scenario with multiple files.

    Args:
        repo: Lore repository instance.
        files: dict mapping path -> (base_lines, current_lines, incoming_lines).
               Each value is a tuple of three line lists: base content, current branch
               modification, and incoming branch modification.
    """
    # Base commit with all files
    base_contents = {path: lines[0] for path, lines in files.items()}
    repo.write_commit_push("Base commit", base_contents, offline=True)

    # Create incoming branch and make changes there first
    repo.branch_create("incoming", offline=True)
    incoming_contents = {path: lines[2] for path, lines in files.items()}
    repo.write_commit_push("Incoming changes", incoming_contents, offline=True)

    # Switch back to main and create current branch with different changes
    repo.branch_switch("main", offline=True)
    repo.branch_create("current", offline=True)
    current_contents = {path: lines[1] for path, lines in files.items()}
    repo.write_commit_push("Current changes", current_contents, offline=True)

    # Merge incoming into current to trigger conflicts
    repo.branch_merge_start(
        "incoming", no_commit=True, check=False, offline=True
    )


def get_conflicted_paths(repo: Lore) -> set[str]:
    """Return the set of file paths that are unresolved conflicts via JSON status."""
    raw = repo.status(offline=True, json=True)
    entries = parse_status_json(raw)
    return {
        e["path"]
        for e in entries
        if e.get("flagConflictUnresolved") is True
    }


def get_resolved_paths(repo: Lore) -> set[str]:
    """Return the set of file paths that are resolved conflicts via JSON status."""
    raw = repo.status(offline=True, json=True)
    entries = parse_status_json(raw)
    return {
        e["path"]
        for e in entries
        if e.get("flagConflict") is True and e.get("flagConflictUnresolved") is False
    }


def get_merged_paths(repo: Lore) -> set[str]:
    """Return the set of file paths that are merged via JSON status."""
    raw = repo.status(offline=True, json=True)
    entries = parse_status_json(raw)
    return {
        e["path"]
        for e in entries
        if e.get("flagMerged") is True
    }


# ---------------------------------------------------------------------------
# Test: resolve with directory path
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_directory(new_lore_repo):
    """Resolve all conflicts under a directory path."""
    repo: Lore = new_lore_repo()

    base = ["line1\n", "line2\n", "line3\n"]
    current = ["line1\n", "line2 current\n", "line3\n"]
    incoming = ["line1\n", "line2 incoming\n", "line3\n"]

    files = {
        "src/a.txt": (base, current, incoming),
        "src/b.txt": (base, current, incoming),
        "other/c.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    # All three files should be conflicted
    conflicted = get_conflicted_paths(repo)
    assert "src/a.txt" in conflicted, f"src/a.txt should be conflicted, got {conflicted}"
    assert "src/b.txt" in conflicted, f"src/b.txt should be conflicted, got {conflicted}"
    assert "other/c.txt" in conflicted, f"other/c.txt should be conflicted, got {conflicted}"

    # Manually resolve the files under src/ by writing clean content
    for path in ["src/a.txt", "src/b.txt"]:
        with repo.open_file(path, "w+") as f:
            f.writelines(["line1\n", "line2 merged\n", "line3\n"])

    # Resolve the src/ directory
    output = repo.branch_merge_resolve("src", offline=True, json=True)
    resolved_events = parse_resolve_events(output)

    assert "src/a.txt" in resolved_events, (
        f"src/a.txt should be in resolve events, got {resolved_events}"
    )
    assert "src/b.txt" in resolved_events, (
        f"src/b.txt should be in resolve events, got {resolved_events}"
    )
    assert "other/c.txt" not in resolved_events, (
        f"other/c.txt should NOT be in resolve events, got {resolved_events}"
    )

    # Verify via status: src/ files resolved, other/c.txt still conflicted
    still_conflicted = get_conflicted_paths(repo)
    assert "src/a.txt" not in still_conflicted, "src/a.txt should be resolved"
    assert "src/b.txt" not in still_conflicted, "src/b.txt should be resolved"
    assert "other/c.txt" in still_conflicted, "other/c.txt should still be conflicted"

    resolved = get_resolved_paths(repo)
    assert "src/a.txt" in resolved, "src/a.txt should show as resolved"
    assert "src/b.txt" in resolved, "src/b.txt should show as resolved"

    repo.branch_merge_abort(offline=True)


# ---------------------------------------------------------------------------
# Test: resolve with "." to resolve all
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_dot(new_lore_repo):
    """Resolve all conflicts in the repository using '.'."""
    repo: Lore = new_lore_repo()

    base = ["line1\n", "line2\n", "line3\n"]
    current = ["line1\n", "line2 current\n", "line3\n"]
    incoming = ["line1\n", "line2 incoming\n", "line3\n"]

    files = {
        "a.txt": (base, current, incoming),
        "dir/b.txt": (base, current, incoming),
        "dir/sub/c.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    conflicted = get_conflicted_paths(repo)
    assert len(conflicted) == 3, f"Expected 3 conflicted files, got {conflicted}"

    # Write clean content to all files
    for path in files:
        with repo.open_file(path, "w+") as f:
            f.writelines(["line1\n", "line2 merged\n", "line3\n"])

    # Resolve all using "."
    output = repo.branch_merge_resolve(".", offline=True, json=True)
    resolved_events = parse_resolve_events(output)

    assert len(resolved_events) == 3, (
        f"Expected 3 resolve events, got {resolved_events}"
    )
    for path in files:
        assert path in resolved_events, (
            f"{path} should be in resolve events, got {resolved_events}"
        )

    # All conflicts should be resolved now
    still_conflicted = get_conflicted_paths(repo)
    assert len(still_conflicted) == 0, (
        f"No files should remain conflicted, got {still_conflicted}"
    )

    repo.branch_merge_abort(offline=True)


# ---------------------------------------------------------------------------
# Test: resolve with multiple explicit paths
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_multiple_paths(new_lore_repo):
    """Resolve specific files using multiple explicit paths."""
    repo: Lore = new_lore_repo()

    base = ["line1\n", "line2\n", "line3\n"]
    current = ["line1\n", "line2 current\n", "line3\n"]
    incoming = ["line1\n", "line2 incoming\n", "line3\n"]

    files = {
        "a.txt": (base, current, incoming),
        "b.txt": (base, current, incoming),
        "c.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    # Write clean content to a.txt and c.txt only
    for path in ["a.txt", "c.txt"]:
        with repo.open_file(path, "w+") as f:
            f.writelines(["line1\n", "line2 merged\n", "line3\n"])

    # Resolve a.txt and c.txt explicitly
    output = repo.branch_merge_resolve(
        ["a.txt", "c.txt"], offline=True, json=True
    )
    resolved_events = parse_resolve_events(output)

    assert "a.txt" in resolved_events, f"a.txt should be resolved, got {resolved_events}"
    assert "c.txt" in resolved_events, f"c.txt should be resolved, got {resolved_events}"
    assert "b.txt" not in resolved_events, (
        f"b.txt should NOT be resolved, got {resolved_events}"
    )

    still_conflicted = get_conflicted_paths(repo)
    assert "b.txt" in still_conflicted, "b.txt should still be conflicted"
    assert "a.txt" not in still_conflicted, "a.txt should be resolved"
    assert "c.txt" not in still_conflicted, "c.txt should be resolved"

    repo.branch_merge_abort(offline=True)


# ---------------------------------------------------------------------------
# Test: resolve mine with directory path
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_mine_directory(new_lore_repo):
    """Resolve all conflicts under a directory using 'mine'."""
    repo: Lore = new_lore_repo()

    base = ["base\n"]
    current = ["current\n"]
    incoming = ["incoming\n"]

    files = {
        "src/a.txt": (base, current, incoming),
        "src/b.txt": (base, current, incoming),
        "other/c.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    conflicted = get_conflicted_paths(repo)
    assert len(conflicted) == 3, f"Expected 3 conflicted files, got {conflicted}"

    # Resolve src/ with mine
    repo.branch_merge_resolve_mine("src", offline=True, json=True)

    # src/ files should be resolved, other/c.txt still conflicted
    still_conflicted = get_conflicted_paths(repo)
    assert "other/c.txt" in still_conflicted, "other/c.txt should still be conflicted"
    assert "src/a.txt" not in still_conflicted, "src/a.txt should be resolved"
    assert "src/b.txt" not in still_conflicted, "src/b.txt should be resolved"

    # Verify content is "mine" (current branch version)
    for path in ["src/a.txt", "src/b.txt"]:
        with repo.open_file(path, "r") as f:
            content = f.read()
        assert content == "current\n", (
            f"{path} should have 'mine' content, got {content!r}"
        )

    repo.branch_merge_abort(offline=True)


# ---------------------------------------------------------------------------
# Test: resolve theirs with directory path
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_theirs_directory(new_lore_repo):
    """Resolve all conflicts under a directory using 'theirs'."""
    repo: Lore = new_lore_repo()

    base = ["base\n"]
    current = ["current\n"]
    incoming = ["incoming\n"]

    files = {
        "src/a.txt": (base, current, incoming),
        "src/b.txt": (base, current, incoming),
        "other/c.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    conflicted = get_conflicted_paths(repo)
    assert len(conflicted) == 3, f"Expected 3 conflicted files, got {conflicted}"

    # Resolve src/ with theirs
    repo.branch_merge_resolve_theirs("src", offline=True, json=True)

    # src/ files should be resolved, other/c.txt still conflicted
    still_conflicted = get_conflicted_paths(repo)
    assert "other/c.txt" in still_conflicted, "other/c.txt should still be conflicted"
    assert "src/a.txt" not in still_conflicted, "src/a.txt should be resolved"
    assert "src/b.txt" not in still_conflicted, "src/b.txt should be resolved"

    # Verify content is "theirs" (incoming branch version)
    for path in ["src/a.txt", "src/b.txt"]:
        with repo.open_file(path, "r") as f:
            content = f.read()
        assert content == "incoming\n", (
            f"{path} should have 'theirs' content, got {content!r}"
        )

    repo.branch_merge_abort(offline=True)


# ---------------------------------------------------------------------------
# Test: resolve mine with "." to resolve all
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_mine_dot(new_lore_repo):
    """Resolve all conflicts in the repository using 'mine' with '.'."""
    repo: Lore = new_lore_repo()

    base = ["base\n"]
    current = ["current\n"]
    incoming = ["incoming\n"]

    files = {
        "a.txt": (base, current, incoming),
        "dir/b.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    conflicted = get_conflicted_paths(repo)
    assert len(conflicted) == 2, f"Expected 2 conflicted files, got {conflicted}"

    # Resolve all with mine using "."
    repo.branch_merge_resolve_mine(".", offline=True, json=True)

    still_conflicted = get_conflicted_paths(repo)
    assert len(still_conflicted) == 0, (
        f"No files should remain conflicted, got {still_conflicted}"
    )

    # Verify all files have "mine" content
    for path in files:
        with repo.open_file(path, "r") as f:
            content = f.read()
        assert content == "current\n", (
            f"{path} should have 'mine' content, got {content!r}"
        )

    repo.commit("Resolved all with mine", offline=True)


# ---------------------------------------------------------------------------
# Test: resolve theirs with "." to resolve all
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_theirs_dot(new_lore_repo):
    """Resolve all conflicts in the repository using 'theirs' with '.'."""
    repo: Lore = new_lore_repo()

    base = ["base\n"]
    current = ["current\n"]
    incoming = ["incoming\n"]

    files = {
        "a.txt": (base, current, incoming),
        "dir/b.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    conflicted = get_conflicted_paths(repo)
    assert len(conflicted) == 2, f"Expected 2 conflicted files, got {conflicted}"

    # Resolve all with theirs using "."
    repo.branch_merge_resolve_theirs(".", offline=True, json=True)

    still_conflicted = get_conflicted_paths(repo)
    assert len(still_conflicted) == 0, (
        f"No files should remain conflicted, got {still_conflicted}"
    )

    # Verify all files have "theirs" content
    for path in files:
        with repo.open_file(path, "r") as f:
            content = f.read()
        assert content == "incoming\n", (
            f"{path} should have 'theirs' content, got {content!r}"
        )

    repo.commit("Resolved all with theirs", offline=True)


# ---------------------------------------------------------------------------
# Test: resolve mine with multiple explicit paths
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_mine_multiple_paths(new_lore_repo):
    """Resolve specific files using 'mine' with multiple paths."""
    repo: Lore = new_lore_repo()

    base = ["base\n"]
    current = ["current\n"]
    incoming = ["incoming\n"]

    files = {
        "a.txt": (base, current, incoming),
        "b.txt": (base, current, incoming),
        "c.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    # Resolve only a.txt and b.txt with mine
    repo.branch_merge_resolve_mine(["a.txt", "b.txt"], offline=True, json=True)

    still_conflicted = get_conflicted_paths(repo)
    assert "c.txt" in still_conflicted, "c.txt should still be conflicted"
    assert "a.txt" not in still_conflicted, "a.txt should be resolved"
    assert "b.txt" not in still_conflicted, "b.txt should be resolved"

    for path in ["a.txt", "b.txt"]:
        with repo.open_file(path, "r") as f:
            content = f.read()
        assert content == "current\n", (
            f"{path} should have 'mine' content, got {content!r}"
        )

    repo.branch_merge_abort(offline=True)


# ---------------------------------------------------------------------------
# Test: resolve theirs with multiple explicit paths
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_theirs_multiple_paths(new_lore_repo):
    """Resolve specific files using 'theirs' with multiple paths."""
    repo: Lore = new_lore_repo()

    base = ["base\n"]
    current = ["current\n"]
    incoming = ["incoming\n"]

    files = {
        "a.txt": (base, current, incoming),
        "b.txt": (base, current, incoming),
        "c.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    # Resolve only a.txt and b.txt with theirs
    repo.branch_merge_resolve_theirs(["a.txt", "b.txt"], offline=True, json=True)

    still_conflicted = get_conflicted_paths(repo)
    assert "c.txt" in still_conflicted, "c.txt should still be conflicted"
    assert "a.txt" not in still_conflicted, "a.txt should be resolved"
    assert "b.txt" not in still_conflicted, "b.txt should be resolved"

    for path in ["a.txt", "b.txt"]:
        with repo.open_file(path, "r") as f:
            content = f.read()
        assert content == "incoming\n", (
            f"{path} should have 'theirs' content, got {content!r}"
        )

    repo.branch_merge_abort(offline=True)


# ---------------------------------------------------------------------------
# Test: resolve with nested directory structure
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_nested_directories(new_lore_repo):
    """Resolve conflicts in deeply nested directory structure."""
    repo: Lore = new_lore_repo()

    base = ["base\n"]
    current = ["current\n"]
    incoming = ["incoming\n"]

    files = {
        "src/core/a.txt": (base, current, incoming),
        "src/core/util/b.txt": (base, current, incoming),
        "src/other/c.txt": (base, current, incoming),
        "docs/d.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    conflicted = get_conflicted_paths(repo)
    assert len(conflicted) == 4, f"Expected 4 conflicted files, got {conflicted}"

    # Resolve only src/core/ with theirs (should get a.txt and util/b.txt)
    repo.branch_merge_resolve_theirs("src/core", offline=True, json=True)

    still_conflicted = get_conflicted_paths(repo)
    assert "src/core/a.txt" not in still_conflicted, "src/core/a.txt should be resolved"
    assert "src/core/util/b.txt" not in still_conflicted, (
        "src/core/util/b.txt should be resolved"
    )
    assert "src/other/c.txt" in still_conflicted, "src/other/c.txt should still be conflicted"
    assert "docs/d.txt" in still_conflicted, "docs/d.txt should still be conflicted"

    # Verify content of resolved files
    for path in ["src/core/a.txt", "src/core/util/b.txt"]:
        with repo.open_file(path, "r") as f:
            content = f.read()
        assert content == "incoming\n", (
            f"{path} should have 'theirs' content, got {content!r}"
        )

    repo.branch_merge_abort(offline=True)


# ---------------------------------------------------------------------------
# Test: resolve with no paths resolves all (empty args)
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_merge_resolve_mine_no_paths(new_lore_repo):
    """Resolve all conflicts when no paths are specified (defaults to all)."""
    repo: Lore = new_lore_repo()

    base = ["base\n"]
    current = ["current\n"]
    incoming = ["incoming\n"]

    files = {
        "a.txt": (base, current, incoming),
        "dir/b.txt": (base, current, incoming),
    }
    setup_merge_conflict(repo, files)

    conflicted = get_conflicted_paths(repo)
    assert len(conflicted) == 2, f"Expected 2 conflicted files, got {conflicted}"

    # Resolve all with mine using no paths (pass repo root path which _fix_paths produces for None)
    repo.branch_merge_resolve_mine(None, offline=True, json=True)

    still_conflicted = get_conflicted_paths(repo)
    assert len(still_conflicted) == 0, (
        f"No files should remain conflicted, got {still_conflicted}"
    )

    for path in files:
        with repo.open_file(path, "r") as f:
            content = f.read()
        assert content == "current\n", (
            f"{path} should have 'mine' content, got {content!r}"
        )

    repo.commit("Resolved all with mine (no paths)", offline=True)
# ---------------------------------------------------------------------------
# Test: a final line without a newline survives resolve unchanged
# ---------------------------------------------------------------------------

# `IncompleteHunkStyle::Git` inserts a newline after an incomplete final line so
# the following conflict marker starts at column 0. These pin that the newline
# belongs to the marker rendering only: resolving restores the side byte for
# byte, exactly as it was committed.

BASE_NO_EOL = b"line 1\nline 2"
MINE_NO_EOL = b"line 1\nline 2 mine"
THEIRS_NO_EOL = b"line 1\nline 2 theirs"


@pytest.mark.smoke
def test_merge_conflict_markers_own_line_without_trailing_newline(new_lore_repo):
    """Every marker starts a line even when the conflicting hunk has no EOL."""
    repo: Lore = new_lore_repo()
    setup_merge_conflict(
        repo, {"a.txt": (BASE_NO_EOL, MINE_NO_EOL, THEIRS_NO_EOL)}
    )

    with repo.open_file("a.txt", "rb") as f:
        conflicted = f.read().decode()

    for marker in ["<<<<<<< ours", "||||||| original", "=======", ">>>>>>> theirs"]:
        assert any(line == marker for line in conflicted.split("\n")), (
            f"{marker!r} must occupy a whole line, got {conflicted!r}"
        )

    repo.branch_merge_abort(offline=True)


@pytest.mark.smoke
def test_merge_resolve_mine_restores_missing_trailing_newline(new_lore_repo):
    """resolve mine gives back the committed bytes, without the marker newline."""
    repo: Lore = new_lore_repo()
    setup_merge_conflict(
        repo, {"a.txt": (BASE_NO_EOL, MINE_NO_EOL, THEIRS_NO_EOL)}
    )

    repo.branch_merge_resolve_mine(["a.txt"], offline=True, json=True)

    with repo.open_file("a.txt", "rb") as f:
        content = f.read()
    assert content == MINE_NO_EOL, (
        f"resolve mine must restore the exact bytes, got {content!r}"
    )

    repo.branch_merge_abort(offline=True)


@pytest.mark.smoke
def test_merge_resolve_theirs_restores_missing_trailing_newline(new_lore_repo):
    """resolve theirs gives back the committed bytes, without the marker newline."""
    repo: Lore = new_lore_repo()
    setup_merge_conflict(
        repo, {"a.txt": (BASE_NO_EOL, MINE_NO_EOL, THEIRS_NO_EOL)}
    )

    repo.branch_merge_resolve_theirs(["a.txt"], offline=True, json=True)

    with repo.open_file("a.txt", "rb") as f:
        content = f.read()
    assert content == THEIRS_NO_EOL, (
        f"resolve theirs must restore the exact bytes, got {content!r}"
    )

    repo.branch_merge_abort(offline=True)
