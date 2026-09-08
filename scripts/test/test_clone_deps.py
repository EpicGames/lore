# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from lore_parsers import parse_jsonl

from lore import Lore

logger = logging.getLogger(__name__)


def write_files(repo: Lore, paths: list[str]):
    """Create files with unique content on disk (no stage/commit)."""
    dirs = {os.path.dirname(p) for p in paths if os.path.dirname(p)}
    for d in sorted(dirs):
        repo.make_dirs(d)
    for path in paths:
        with repo.open_file(path, "w+") as f:
            f.write(f"content of {path}\n")


def setup_repo_with_deps(repo, files, deps, tag_deps=None):
    """Create files, commit, add dependencies, push.

    ``deps`` is a dict mapping source -> list of targets.
    ``tag_deps`` is a dict mapping source -> list of (targets, tags) tuples.
    Dependencies are added after commit so NodeIDs are resolved, then the
    modified state is pushed via a second commit (touch a marker file to
    make stage/commit happy).
    """
    write_files(repo, files)
    repo.stage(scan=True, offline=True)
    repo.commit("Add files", offline=True)

    for source, targets in (deps or {}).items():
        repo.file_dependency_add(source, targets, offline=True)
    for source, tagged_list in (tag_deps or {}).items():
        for targets, tags in tagged_list:
            repo.file_dependency_add(source, targets, tags=tags, offline=True)

    if deps or tag_deps:
        # Touch a marker file to trigger a second commit capturing the dep metadata
        with repo.open_file(".deps_marker", "w+") as f:
            f.write("deps")
        repo.stage(scan=True, offline=True)
        repo.commit("Add dependencies", offline=True)

    repo.push()


def cloned_files(clone: Lore) -> set[str]:
    """Return the set of files present in a cloned repository (relative paths)."""
    result = set()
    for root, _dirs, files in os.walk(clone.path):
        rel_root = os.path.relpath(root, clone.path)
        if rel_root.startswith(".urc") or rel_root.startswith(".lore"):
            continue
        for f in files:
            if f.startswith(".urc") or f.startswith(".lore") or f == ".deps_marker":
                continue
            rel = os.path.join(rel_root, f) if rel_root != "." else f
            result.add(rel.replace(os.sep, "/"))
    return result


@pytest.mark.smoke
def test_clone_deps_root_file_only(new_lore_repo):
    """
    Clone with a single root file and no dependencies clones only that file.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=["src/main.rs", "src/lib.rs", "assets/texture.png"],
        deps=None,
    )

    clone = repo.clone(root_files=["src/main.rs"])
    files = cloned_files(clone)
    assert "src/main.rs" in files, f"Root file not cloned: {files}"
    assert "src/lib.rs" not in files, f"Unrelated file should not be cloned: {files}"
    assert "assets/texture.png" not in files, (
        f"Unrelated file should not be cloned: {files}"
    )


@pytest.mark.smoke
def test_clone_deps_direct_dependencies(new_lore_repo):
    """
    Clone with root file and non-recursive mode fetches root + direct deps only.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=[
            "src/main.rs",
            "src/lib.rs",
            "src/util.rs",
            "src/deep.rs",
            "assets/texture.png",
        ],
        deps={
            "src/main.rs": ["src/lib.rs", "src/util.rs"],
            "src/lib.rs": ["src/deep.rs"],
        },
    )

    # Non-recursive: only root + direct deps (lib.rs, util.rs), NOT deep.rs
    clone = repo.clone(root_files=["src/main.rs"])
    files = cloned_files(clone)
    assert "src/main.rs" in files
    assert "src/lib.rs" in files, f"Direct dep not cloned: {files}"
    assert "src/util.rs" in files, f"Direct dep not cloned: {files}"
    assert "src/deep.rs" not in files, (
        f"Transitive dep should not be cloned without recursive: {files}"
    )
    assert "assets/texture.png" not in files, (
        f"Unrelated file should not be cloned: {files}"
    )


@pytest.mark.smoke
def test_clone_deps_recursive(new_lore_repo):
    """
    Clone with recursive flag fetches root + all transitive dependencies.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=[
            "src/main.rs",
            "src/lib.rs",
            "src/util.rs",
            "src/deep.rs",
            "assets/texture.png",
        ],
        deps={
            "src/main.rs": ["src/lib.rs", "src/util.rs"],
            "src/lib.rs": ["src/deep.rs"],
        },
    )

    clone = repo.clone(root_files=["src/main.rs"], dependency_recursive=True)
    files = cloned_files(clone)
    assert "src/main.rs" in files
    assert "src/lib.rs" in files
    assert "src/util.rs" in files
    assert "src/deep.rs" in files, (
        f"Transitive dep should be cloned with recursive: {files}"
    )
    assert "assets/texture.png" not in files, (
        f"Unrelated file should not be cloned: {files}"
    )


@pytest.mark.smoke
def test_clone_deps_tag_filter(new_lore_repo):
    """
    Clone with tag filter only follows dependency edges matching the specified tags.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=[
            "src/main.rs",
            "src/lib.rs",
            "assets/texture.png",
            "assets/sound.wav",
        ],
        deps=None,
        tag_deps={
            "src/main.rs": [
                (["src/lib.rs"], ["build"]),
                (["assets/texture.png", "assets/sound.wav"], ["art"]),
            ],
        },
    )

    # Clone with only "build" tag
    clone_build = repo.clone(root_files=["src/main.rs"], dependency_tags=["build"])
    files = cloned_files(clone_build)
    assert "src/main.rs" in files
    assert "src/lib.rs" in files, f"Build dep not cloned: {files}"
    assert "assets/texture.png" not in files, (
        f"Art dep should not be cloned with build tag: {files}"
    )
    assert "assets/sound.wav" not in files, (
        f"Art dep should not be cloned with build tag: {files}"
    )

    # Clone with only "art" tag
    clone_art = repo.clone(root_files=["src/main.rs"], dependency_tags=["art"])
    files = cloned_files(clone_art)
    assert "src/main.rs" in files
    assert "src/lib.rs" not in files, (
        f"Build dep should not be cloned with art tag: {files}"
    )
    assert "assets/texture.png" in files, f"Art dep not cloned: {files}"
    assert "assets/sound.wav" in files, f"Art dep not cloned: {files}"


@pytest.mark.smoke
def test_clone_deps_multiple_roots(new_lore_repo):
    """
    Clone with multiple root files fetches each root and their respective dependencies.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=[
            "src/main.rs",
            "src/lib.rs",
            "src/test.rs",
            "src/helper.rs",
            "assets/texture.png",
        ],
        deps={
            "src/main.rs": ["src/lib.rs"],
            "src/test.rs": ["src/helper.rs"],
        },
    )

    clone = repo.clone(root_files=["src/main.rs", "src/test.rs"])
    files = cloned_files(clone)
    assert "src/main.rs" in files
    assert "src/lib.rs" in files, f"Dep of first root not cloned: {files}"
    assert "src/test.rs" in files
    assert "src/helper.rs" in files, f"Dep of second root not cloned: {files}"
    assert "assets/texture.png" not in files, (
        f"Unrelated file should not be cloned: {files}"
    )


@pytest.mark.smoke
def test_clone_deps_recursive_with_tags(new_lore_repo):
    """
    Recursive clone with tag filter follows only tagged edges transitively.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=[
            "src/main.rs",
            "src/lib.rs",
            "src/util.rs",
            "assets/texture.png",
        ],
        deps=None,
        tag_deps={
            "src/main.rs": [
                (["src/lib.rs"], ["build"]),
                (["assets/texture.png"], ["art"]),
            ],
            "src/lib.rs": [
                (["src/util.rs"], ["build"]),
            ],
        },
    )

    # Recursive with build tag: main -> lib -> util, but NOT texture
    clone = repo.clone(
        root_files=["src/main.rs"],
        dependency_tags=["build"],
        dependency_recursive=True,
    )
    files = cloned_files(clone)
    assert "src/main.rs" in files
    assert "src/lib.rs" in files
    assert "src/util.rs" in files, f"Transitive build dep not cloned: {files}"
    assert "assets/texture.png" not in files, (
        f"Art dep should not be cloned with build tag: {files}"
    )


@pytest.mark.smoke
def test_clone_deps_depth_limit(new_lore_repo):
    """
    Recursive clone with depth limit stops following dependencies at the limit.
    Chain: main.rs -> lib.rs -> util.rs -> deep.rs (depth 3).
    depth_limit=1: root deps only (lib.rs), not util.rs or deep.rs.
    depth_limit=2: root + one level (lib.rs, util.rs), not deep.rs.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=[
            "src/main.rs",
            "src/lib.rs",
            "src/util.rs",
            "src/deep.rs",
        ],
        deps={
            "src/main.rs": ["src/lib.rs"],
            "src/lib.rs": ["src/util.rs"],
            "src/util.rs": ["src/deep.rs"],
        },
    )

    # depth_limit=1: follow deps of root (depth 0) but not deps at depth 1
    clone_d1 = repo.clone(
        root_files=["src/main.rs"],
        dependency_recursive=True,
        dependency_depth_limit=1,
    )
    files = cloned_files(clone_d1)
    assert "src/main.rs" in files
    assert "src/lib.rs" in files, f"Depth-1 dep not cloned: {files}"
    assert "src/util.rs" not in files, (
        f"Depth-2 dep should not be cloned with limit=1: {files}"
    )
    assert "src/deep.rs" not in files, (
        f"Depth-3 dep should not be cloned with limit=1: {files}"
    )

    # depth_limit=2: follow deps of root and depth 1
    clone_d2 = repo.clone(
        root_files=["src/main.rs"],
        dependency_recursive=True,
        dependency_depth_limit=2,
    )
    files = cloned_files(clone_d2)
    assert "src/main.rs" in files
    assert "src/lib.rs" in files
    assert "src/util.rs" in files, f"Depth-2 dep not cloned with limit=2: {files}"
    assert "src/deep.rs" not in files, (
        f"Depth-3 dep should not be cloned with limit=2: {files}"
    )


@pytest.mark.smoke
def test_clone_deps_no_root_files_clones_all(new_lore_repo):
    """
    Clone without root_files (default) clones all files as before.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=["src/main.rs", "src/lib.rs", "assets/texture.png"],
        deps=None,
    )

    clone = repo.clone()
    files = cloned_files(clone)
    assert "src/main.rs" in files
    assert "src/lib.rs" in files
    assert "assets/texture.png" in files


def clone_with_json(repo, scratch_dir, root_files, **extra_args):
    """Run clone with JSON output and return the raw output string.

    The clone lands beside the repository rather than inside it, so it needs
    `scratch_dir` to have it removed when the test ends.
    """
    clone_path = str(scratch_dir("clone", create=True))
    args = ["repository", "clone", repo.remote + repo.name, clone_path]
    for rf in root_files:
        args += ["--root-file", rf]
    for tag in extra_args.get("dependency_tags", []):
        args += ["--dependency-tag", tag]
    if extra_args.get("dependency_recursive"):
        args.append("--dependency-recursive")
    return repo.run(args, json=True)


@pytest.mark.smoke
def test_clone_deps_events(new_lore_repo, scratch_dir):
    """
    Clone with root files emits dependency resolve begin/end/item events.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=["src/main.rs", "src/lib.rs", "src/util.rs"],
        deps={"src/main.rs": ["src/lib.rs", "src/util.rs"]},
    )

    output = clone_with_json(repo, scratch_dir, ["src/main.rs"])

    begin_events = parse_jsonl(output, "dependencyResolveBegin")
    end_events = parse_jsonl(output, "dependencyResolveEnd")
    item_events = parse_jsonl(output, "dependencyResolveItem")

    assert len(begin_events) == 1, f"Expected 1 begin event, got: {begin_events}"
    assert begin_events[0]["rootCount"] == 1
    assert len(end_events) == 1, f"Expected 1 end event, got: {end_events}"
    assert end_events[0]["resolvedCount"] >= 1

    # Verify item events for each resolved dependency
    assert len(item_events) == 2, f"Expected 2 item events, got: {item_events}"
    targets = {e["target"] for e in item_events}
    assert "src/lib.rs" in targets, f"Missing lib.rs in item events: {item_events}"
    assert "src/util.rs" in targets, f"Missing util.rs in item events: {item_events}"
    for e in item_events:
        assert e["source"] == "src/main.rs", f"Expected source src/main.rs, got: {e}"


@pytest.mark.smoke
def test_clone_deps_events_with_tags(new_lore_repo, scratch_dir):
    """
    Item events include the tags from the dependency edges.
    Only edges matching the tag filter produce item events.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=["src/main.rs", "src/lib.rs", "assets/texture.png"],
        deps=None,
        tag_deps={
            "src/main.rs": [
                (["src/lib.rs"], ["build"]),
                (["assets/texture.png"], ["art"]),
            ],
        },
    )

    # Clone with build tag — only build dep item events
    output = clone_with_json(
        repo, scratch_dir, ["src/main.rs"], dependency_tags=["build"]
    )
    item_events = parse_jsonl(output, "dependencyResolveItem")
    assert len(item_events) == 1, (
        f"Expected 1 item event with build tag, got: {item_events}"
    )
    assert item_events[0]["target"] == "src/lib.rs"
    assert item_events[0]["source"] == "src/main.rs"
    assert "build" in item_events[0]["tags"]

    # Clone with art tag — only art dep item events
    output = clone_with_json(
        repo, scratch_dir, ["src/main.rs"], dependency_tags=["art"]
    )
    item_events = parse_jsonl(output, "dependencyResolveItem")
    assert len(item_events) == 1, (
        f"Expected 1 item event with art tag, got: {item_events}"
    )
    assert item_events[0]["target"] == "assets/texture.png"
    assert "art" in item_events[0]["tags"]

    # Clone with no tag filter — all dep item events
    output = clone_with_json(repo, scratch_dir, ["src/main.rs"])
    item_events = parse_jsonl(output, "dependencyResolveItem")
    assert len(item_events) == 2, (
        f"Expected 2 item events without tag filter, got: {item_events}"
    )
    targets = {e["target"] for e in item_events}
    assert targets == {"src/lib.rs", "assets/texture.png"}
