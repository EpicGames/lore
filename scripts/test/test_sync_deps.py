# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

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
        with repo.open_file(".deps_marker", "w+") as f:
            f.write("deps")
        repo.stage(scan=True, offline=True)
        repo.commit("Add dependencies", offline=True)

    repo.push()


def read_file(repo: Lore, path: str) -> str:
    """Read file content from a repository."""
    with repo.open_file(path, "r") as f:
        return f.read()


@pytest.mark.smoke
def test_sync_deps_filters_changes(new_lore_repo):
    """
    Sync with root_files only applies changes for root + direct deps.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=["src/main.rs", "src/lib.rs", "src/util.rs", "assets/texture.png"],
        deps={"src/main.rs": ["src/lib.rs"]},
    )

    # Clone full repo, then modify files in source and push new revision
    clone = repo.clone()
    with repo.open_file("src/main.rs", "w") as f:
        f.write("main v2\n")
    with repo.open_file("src/lib.rs", "w") as f:
        f.write("lib v2\n")
    with repo.open_file("src/util.rs", "w") as f:
        f.write("util v2\n")
    with repo.open_file("assets/texture.png", "w") as f:
        f.write("texture v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Update all files", offline=True)
    repo.push()

    # Sync clone with only main.rs as root — should update main.rs + lib.rs, NOT util.rs or texture
    clone.sync(root_files=["src/main.rs"])
    assert read_file(clone, "src/main.rs") == "main v2\n"
    assert read_file(clone, "src/lib.rs") == "lib v2\n"
    assert read_file(clone, "src/util.rs") == "content of src/util.rs\n", (
        "util.rs should not be updated by filtered sync"
    )
    assert (
        read_file(clone, "assets/texture.png") == "content of assets/texture.png\n"
    ), "texture.png should not be updated by filtered sync"


@pytest.mark.smoke
def test_sync_deps_recursive(new_lore_repo):
    """
    Sync with recursive follows transitive dependencies.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=["src/main.rs", "src/lib.rs", "src/deep.rs", "src/unrelated.rs"],
        deps={
            "src/main.rs": ["src/lib.rs"],
            "src/lib.rs": ["src/deep.rs"],
        },
    )

    clone = repo.clone()
    with repo.open_file("src/main.rs", "w") as f:
        f.write("main v2\n")
    with repo.open_file("src/lib.rs", "w") as f:
        f.write("lib v2\n")
    with repo.open_file("src/deep.rs", "w") as f:
        f.write("deep v2\n")
    with repo.open_file("src/unrelated.rs", "w") as f:
        f.write("unrelated v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Update all", offline=True)
    repo.push()

    clone.sync(root_files=["src/main.rs"], dependency_recursive=True)
    assert read_file(clone, "src/main.rs") == "main v2\n"
    assert read_file(clone, "src/lib.rs") == "lib v2\n"
    assert read_file(clone, "src/deep.rs") == "deep v2\n", (
        "Transitive dep should be synced with recursive"
    )
    assert read_file(clone, "src/unrelated.rs") == "content of src/unrelated.rs\n", (
        "Unrelated file should not be synced"
    )


@pytest.mark.smoke
def test_sync_deps_tag_filter(new_lore_repo):
    """
    Sync with tag filter only applies changes for deps matching the tag.
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

    clone = repo.clone()
    with repo.open_file("src/main.rs", "w") as f:
        f.write("main v2\n")
    with repo.open_file("src/lib.rs", "w") as f:
        f.write("lib v2\n")
    with repo.open_file("assets/texture.png", "w") as f:
        f.write("texture v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Update all", offline=True)
    repo.push()

    # Sync with build tag — only main + lib updated
    clone.sync(root_files=["src/main.rs"], dependency_tags=["build"])
    assert read_file(clone, "src/main.rs") == "main v2\n"
    assert read_file(clone, "src/lib.rs") == "lib v2\n"
    assert (
        read_file(clone, "assets/texture.png") == "content of assets/texture.png\n"
    ), "Art dep should not be synced with build tag"


@pytest.mark.smoke
def test_sync_deps_no_root_files_syncs_all(new_lore_repo):
    """
    Sync without root_files (default) syncs all changes.
    """
    repo: Lore = new_lore_repo()
    setup_repo_with_deps(
        repo,
        files=["src/main.rs", "src/lib.rs", "assets/texture.png"],
        deps=None,
    )

    clone = repo.clone()
    with repo.open_file("src/main.rs", "w") as f:
        f.write("main v2\n")
    with repo.open_file("src/lib.rs", "w") as f:
        f.write("lib v2\n")
    with repo.open_file("assets/texture.png", "w") as f:
        f.write("texture v2\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Update all", offline=True)
    repo.push()

    clone.sync()
    assert read_file(clone, "src/main.rs") == "main v2\n"
    assert read_file(clone, "src/lib.rs") == "lib v2\n"
    assert read_file(clone, "assets/texture.png") == "texture v2\n"
