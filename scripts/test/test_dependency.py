# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from lore_parsers import parse_complete_json
from lore_parsers import parse_jsonl

from lore import Lore

logger = logging.getLogger(__name__)


# --- Dependency-specific parsing helpers ---


def parse_add_result(output: str) -> int:
    """Return the addedCount from a dependency add JSON output."""
    results = parse_jsonl(output, "fileDependencyAddEnd")
    assert len(results) == 1, f"Expected one addEnd event, got: {results}"
    return results[0]["addedCount"]


def parse_remove_result(output: str) -> int:
    """Return the removedCount from a dependency remove JSON output."""
    results = parse_jsonl(output, "fileDependencyRemoveEnd")
    assert len(results) == 1, f"Expected one removeEnd event, got: {results}"
    return results[0]["removedCount"]


def parse_list_entries(output: str) -> list[dict]:
    """Return all list entry dicts (path, tags, depth) from JSON output."""
    return parse_jsonl(output, "fileDependencyListEntry")


def listed_paths(output: str) -> set[str]:
    """Return the set of dependency paths from a list JSON output."""
    return {e["path"] for e in parse_list_entries(output)}


def create_files(repo: Lore, paths: list[str]):
    """Create, stage, and commit a set of files."""
    dirs = {os.path.dirname(p) for p in paths if os.path.dirname(p)}
    for d in dirs:
        repo.make_dirs(d)
    for path in paths:
        with repo.open_file(path, "w+") as f:
            f.write(f"// {path}\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Add test files", offline=True)


# --- Tests ---


@pytest.mark.smoke
def test_dependency_add_list_remove(new_lore_repo):
    """Test basic dependency add, list, and remove workflow."""
    repo: Lore = new_lore_repo()
    create_files(repo, ["src/main.rs", "src/lib.rs", "src/util.rs"])

    # Add dependencies: main.rs depends on lib.rs and util.rs
    output = repo.file_dependency_add(
        "src/main.rs", ["src/lib.rs", "src/util.rs"], json=True, offline=True
    )
    assert parse_add_result(output) == 2

    # List dependencies for main.rs
    output = repo.file_dependency_list("src/main.rs", json=True, offline=True)
    paths = listed_paths(output)
    assert "src/lib.rs" in paths, f"Expected src/lib.rs in list, got: {paths}"
    assert "src/util.rs" in paths, f"Expected src/util.rs in list, got: {paths}"

    # List reverse dependencies (dependents) for lib.rs
    output = repo.file_dependency_list(
        "src/lib.rs", reverse=True, json=True, offline=True
    )
    paths = listed_paths(output)
    assert "src/main.rs" in paths, f"Expected src/main.rs in reverse list, got: {paths}"

    # Remove one dependency
    output = repo.file_dependency_remove(
        "src/main.rs", "src/util.rs", json=True, offline=True
    )
    assert parse_remove_result(output) == 1

    # Verify util.rs is no longer listed
    output = repo.file_dependency_list("src/main.rs", json=True, offline=True)
    paths = listed_paths(output)
    assert "src/lib.rs" in paths, f"Expected src/lib.rs still in list, got: {paths}"
    assert "src/util.rs" not in paths, f"Expected src/util.rs removed, got: {paths}"


@pytest.mark.smoke
def test_dependency_tags(new_lore_repo):
    """Test dependency operations with tags across multiple files."""
    repo: Lore = new_lore_repo()
    create_files(
        repo,
        [
            "src/app.rs",
            "src/core.rs",
            "src/db.rs",
            "src/api.rs",
            "src/cache.rs",
        ],
    )

    # app.rs -> core.rs with tags [build, test]
    output = repo.file_dependency_add(
        "src/app.rs", "src/core.rs", tags=["build", "test"], json=True, offline=True
    )
    assert parse_add_result(output) == 1

    # app.rs -> db.rs with tag [runtime]
    output = repo.file_dependency_add(
        "src/app.rs", "src/db.rs", tags=["runtime"], json=True, offline=True
    )
    assert parse_add_result(output) == 1

    # app.rs -> api.rs with tags [build, runtime]
    output = repo.file_dependency_add(
        "src/app.rs", "src/api.rs", tags=["build", "runtime"], json=True, offline=True
    )
    assert parse_add_result(output) == 1

    # app.rs -> cache.rs with no tags
    output = repo.file_dependency_add(
        "src/app.rs", "src/cache.rs", json=True, offline=True
    )
    assert parse_add_result(output) == 1

    # List all dependencies (no tag filter) — should show all 4
    output = repo.file_dependency_list("src/app.rs", json=True, offline=True)
    paths = listed_paths(output)
    assert paths == {"src/core.rs", "src/db.rs", "src/api.rs", "src/cache.rs"}, (
        f"Expected all 4 deps, got: {paths}"
    )

    # Filter by "build" — should match core.rs and api.rs
    output = repo.file_dependency_list(
        "src/app.rs", tags=["build"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert paths == {"src/core.rs", "src/api.rs"}, (
        f"Expected build-tagged deps, got: {paths}"
    )

    # Filter by "runtime" — should match db.rs and api.rs
    output = repo.file_dependency_list(
        "src/app.rs", tags=["runtime"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert paths == {"src/db.rs", "src/api.rs"}, (
        f"Expected runtime-tagged deps, got: {paths}"
    )

    # Filter by "test" — should match only core.rs
    output = repo.file_dependency_list(
        "src/app.rs", tags=["test"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert paths == {"src/core.rs"}, f"Expected test-tagged deps, got: {paths}"

    # Filter by non-existent tag — should return nothing
    output = repo.file_dependency_list(
        "src/app.rs", tags=["deploy"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert len(paths) == 0, f"Expected no deps for 'deploy' tag, got: {paths}"

    # Filter by multiple tags (OR semantics) — "test" OR "runtime"
    output = repo.file_dependency_list(
        "src/app.rs", tags=["test", "runtime"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert paths == {"src/core.rs", "src/db.rs", "src/api.rs"}, (
        f"Expected test|runtime deps, got: {paths}"
    )

    # Verify tags are present in the list entry data
    entries = parse_list_entries(output)
    core_entry = next(e for e in entries if e["path"] == "src/core.rs")
    assert "test" in core_entry["tags"], (
        f"Expected 'test' tag on core.rs entry, got: {core_entry['tags']}"
    )


@pytest.mark.smoke
def test_dependency_tag_remove(new_lore_repo):
    """Test removing specific tags from a dependency edge."""
    repo: Lore = new_lore_repo()
    create_files(repo, ["src/a.rs", "src/b.rs"])

    # Add a -> b with tags [build, test, lint]
    repo.file_dependency_add(
        "src/a.rs", "src/b.rs", tags=["build", "test", "lint"], json=True, offline=True
    )

    # Remove only the "test" tag
    output = repo.file_dependency_remove(
        "src/a.rs", "src/b.rs", tags=["test"], json=True, offline=True
    )
    # Edge should still exist (has remaining tags), so removedCount is 0
    assert parse_remove_result(output) == 0

    # Verify "test" tag filter no longer matches
    output = repo.file_dependency_list(
        "src/a.rs", tags=["test"], json=True, offline=True
    )
    assert len(listed_paths(output)) == 0, "Expected no deps after removing 'test' tag"

    # Verify "build" tag filter still matches
    output = repo.file_dependency_list(
        "src/a.rs", tags=["build"], json=True, offline=True
    )
    assert listed_paths(output) == {"src/b.rs"}, "Expected b.rs still tagged with 'build'"

    # Remove remaining tags by removing the whole edge
    output = repo.file_dependency_remove(
        "src/a.rs", "src/b.rs", json=True, offline=True
    )
    assert parse_remove_result(output) == 1

    # Verify edge is gone
    output = repo.file_dependency_list("src/a.rs", json=True, offline=True)
    assert len(listed_paths(output)) == 0, "Expected no deps after full removal"


@pytest.mark.smoke
def test_dependency_tag_merge(new_lore_repo):
    """Test that adding the same edge with new tags merges them."""
    repo: Lore = new_lore_repo()
    create_files(repo, ["src/a.rs", "src/b.rs"])

    # First add with tag [build]
    output = repo.file_dependency_add(
        "src/a.rs", "src/b.rs", tags=["build"], json=True, offline=True
    )
    assert parse_add_result(output) == 1

    # Second add with tag [test] — same edge, should merge tags, not add new edge
    output = repo.file_dependency_add(
        "src/a.rs", "src/b.rs", tags=["test"], json=True, offline=True
    )
    assert parse_add_result(output) == 0  # edge already existed

    # Both tags should be present
    output = repo.file_dependency_list(
        "src/a.rs", tags=["build"], json=True, offline=True
    )
    assert listed_paths(output) == {"src/b.rs"}

    output = repo.file_dependency_list(
        "src/a.rs", tags=["test"], json=True, offline=True
    )
    assert listed_paths(output) == {"src/b.rs"}

    # Full listing should show both tags on the entry
    output = repo.file_dependency_list("src/a.rs", json=True, offline=True)
    entries = parse_list_entries(output)
    assert len(entries) == 1, f"Expected 1 entry, got: {entries}"
    assert set(entries[0]["tags"]) == {"build", "test"}, (
        f"Expected merged tags, got: {entries[0]['tags']}"
    )


@pytest.mark.smoke
def test_dependency_cycle_detection(new_lore_repo):
    """Test that cycle detection prevents circular dependencies."""
    repo: Lore = new_lore_repo()
    create_files(repo, ["src/a.rs", "src/b.rs"])

    # Add A -> B (should succeed)
    output = repo.file_dependency_add(
        "src/a.rs", "src/b.rs", json=True, offline=True
    )
    assert parse_add_result(output) == 1

    # Add B -> A (should fail with cycle detection)
    output = repo.file_dependency_add(
        "src/b.rs", "src/a.rs", check=False, json=True, offline=True
    )
    # The cycle is rejected: the operation completes with a non-zero status,
    # carrying the failure detail on the complete event.
    complete = parse_complete_json(output)
    assert complete is not None and complete.get("status") != 0, (
        f"Expected non-zero complete for cycle, got: {output}"
    )

    # Add B -> A with --force (should succeed, bypassing cycle detection)
    output = repo.file_dependency_add(
        "src/b.rs", "src/a.rs", force=True, json=True, offline=True
    )
    assert parse_add_result(output) == 1


@pytest.mark.smoke
def test_dependency_recursive_list(new_lore_repo):
    """Test recursive dependency listing with depth limit."""
    repo: Lore = new_lore_repo()
    create_files(repo, ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"])

    # Create chain: a -> b -> c -> d
    repo.file_dependency_add("src/a.rs", "src/b.rs", json=True, offline=True)
    repo.file_dependency_add("src/b.rs", "src/c.rs", json=True, offline=True)
    repo.file_dependency_add("src/c.rs", "src/d.rs", json=True, offline=True)

    # Recursive list from a should show b, c, d
    output = repo.file_dependency_list(
        "src/a.rs", recursive=True, json=True, offline=True
    )
    paths = listed_paths(output)
    assert {"src/b.rs", "src/c.rs", "src/d.rs"} <= paths, (
        f"Expected b, c, d in recursive list, got: {paths}"
    )

    # Recursive list with depth limit 1 should show only b
    output = repo.file_dependency_list(
        "src/a.rs", recursive=True, depth=1, json=True, offline=True
    )
    paths = listed_paths(output)
    assert "src/b.rs" in paths, f"Expected b.rs in depth-1 list, got: {paths}"
    assert "src/d.rs" not in paths, f"Expected d.rs NOT in depth-1 list, got: {paths}"


@pytest.mark.smoke
def test_dependency_multiple_sources_with_tags(new_lore_repo):
    """Test dependencies from multiple source files with diverse tags."""
    repo: Lore = new_lore_repo()
    create_files(
        repo,
        [
            "src/main.rs",
            "src/server.rs",
            "src/config.rs",
            "src/db.rs",
            "src/auth.rs",
            "src/logging.rs",
        ],
    )

    # main.rs -> config.rs [init], server.rs [runtime], logging.rs [runtime, debug]
    repo.file_dependency_add(
        "src/main.rs", "src/config.rs", tags=["init"], json=True, offline=True
    )
    repo.file_dependency_add(
        "src/main.rs", "src/server.rs", tags=["runtime"], json=True, offline=True
    )
    repo.file_dependency_add(
        "src/main.rs", "src/logging.rs", tags=["runtime", "debug"],
        json=True, offline=True,
    )

    # server.rs -> db.rs [runtime], auth.rs [runtime, security]
    repo.file_dependency_add(
        "src/server.rs", "src/db.rs", tags=["runtime"], json=True, offline=True
    )
    repo.file_dependency_add(
        "src/server.rs", "src/auth.rs", tags=["runtime", "security"],
        json=True, offline=True,
    )

    # Verify main.rs "runtime" deps
    output = repo.file_dependency_list(
        "src/main.rs", tags=["runtime"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert paths == {"src/server.rs", "src/logging.rs"}, (
        f"Expected runtime deps for main, got: {paths}"
    )

    # Verify main.rs "debug" deps
    output = repo.file_dependency_list(
        "src/main.rs", tags=["debug"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert paths == {"src/logging.rs"}, f"Expected debug deps for main, got: {paths}"

    # Verify server.rs "security" deps
    output = repo.file_dependency_list(
        "src/server.rs", tags=["security"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert paths == {"src/auth.rs"}, (
        f"Expected security deps for server, got: {paths}"
    )

    # Verify reverse: who depends on auth.rs?
    output = repo.file_dependency_list(
        "src/auth.rs", reverse=True, json=True, offline=True
    )
    paths = listed_paths(output)
    assert "src/server.rs" in paths, (
        f"Expected server.rs as dependent of auth.rs, got: {paths}"
    )

    # Verify reverse with tag filter: who depends on logging.rs with "debug"?
    output = repo.file_dependency_list(
        "src/logging.rs", reverse=True, tags=["debug"], json=True, offline=True
    )
    paths = listed_paths(output)
    assert "src/main.rs" in paths, (
        f"Expected main.rs as debug-dependent of logging.rs, got: {paths}"
    )


@pytest.mark.smoke
def test_dependency_list_at_revision(new_lore_repo):
    """Test listing dependencies at a specific historical revision."""
    repo: Lore = new_lore_repo()

    # Revision 1: create files and add main.rs -> lib.rs dependency
    for path in ["src/main.rs", "src/lib.rs", "src/util.rs"]:
        d = os.path.dirname(path)
        if d:
            repo.make_dirs(d)
        with repo.open_file(path, "w+") as f:
            f.write(f"// {path}\n")
    repo.stage(scan=True, offline=True)
    repo.file_dependency_add(
        "src/main.rs", "src/lib.rs", json=True, offline=True
    )
    repo.commit("Create files with lib dependency", offline=True)

    # Revision 2: add main.rs -> util.rs dependency (touch file to have a stageable change)
    with repo.open_file("src/main.rs", "w") as f:
        f.write("// src/main.rs v2\n")
    repo.stage(scan=True, offline=True)
    repo.file_dependency_add(
        "src/main.rs", "src/util.rs", json=True, offline=True
    )
    repo.commit("Add util dependency", offline=True)

    # Current state should show both dependencies
    output = repo.file_dependency_list("src/main.rs", json=True, offline=True)
    paths = listed_paths(output)
    assert paths == {"src/lib.rs", "src/util.rs"}, (
        f"Expected both deps at HEAD, got: {paths}"
    )

    # Revision 1 should show only lib.rs
    output = repo.file_dependency_list(
        "src/main.rs", revision="@1", json=True, offline=True
    )
    paths = listed_paths(output)
    assert paths == {"src/lib.rs"}, (
        f"Expected only lib.rs at @1, got: {paths}"
    )
