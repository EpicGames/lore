# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
from pathlib import Path

from lore import Lore


def write_text(repo: Lore, path: str, text: str):
    absolute = Path(repo.path) / path
    absolute.parent.mkdir(parents=True, exist_ok=True)
    absolute.write_text(text, encoding="utf-8")


def read_text(repo: Lore, path: str) -> str:
    return (Path(repo.path) / path).read_text(encoding="utf-8")


def commit_all(repo: Lore, message: str):
    repo.file_stage(scan=True, offline=True)
    repo.commit(message, offline=True)


def assert_no_conflict_sidecars(repo: Lore, path: str):
    assert not repo.file_exists(path + "~mine")
    assert not repo.file_exists(path + "~theirs")
    assert not repo.file_exists(path + "~base")


def test_merge_keep_target_path_strategy_keeps_git_dir(new_lore_repo):
    repo: Lore = new_lore_repo()

    write_text(repo, ".git/config", "base\n")
    write_text(repo, "base.txt", "base\n")
    commit_all(repo, "Initial commit")

    repo.branch_create("feature", offline=True)
    write_text(repo, ".git/config", "feature\n")
    write_text(repo, ".git/objects/aa/file", "feature object\n")
    write_text(repo, "feature.txt", "feature\n")
    commit_all(repo, "Feature changes")

    repo.branch_switch("main", offline=True)
    write_text(repo, ".git/config", "target\n")
    commit_all(repo, "Target changes")

    repo.branch_merge_start("feature", keep_target=".git", offline=True)

    assert read_text(repo, ".git/config") == "target\n"
    assert not repo.file_exists(".git/objects/aa/file")
    assert repo.file_exists("feature.txt")
    assert_no_conflict_sidecars(repo, ".git/config")


def test_merge_strategy_comma_excludes_multiple_paths(new_lore_repo):
    repo: Lore = new_lore_repo()

    write_text(repo, ".git/config", "base\n")
    write_text(repo, "generated/base.txt", "base\n")
    write_text(repo, "base.txt", "base\n")
    commit_all(repo, "Initial commit")

    repo.branch_create("feature", offline=True)
    write_text(repo, ".git/config", "feature\n")
    write_text(repo, "generated/base.txt", "feature generated\n")
    write_text(repo, "generated/cache.txt", "feature cache\n")
    write_text(repo, "feature.txt", "feature\n")
    commit_all(repo, "Feature changes")

    repo.branch_switch("main", offline=True)
    write_text(repo, ".git/config", "target\n")
    write_text(repo, "generated/base.txt", "target generated\n")
    commit_all(repo, "Target changes")

    repo.branch_merge_start(
        "feature",
        merge_strategy="exclude:.git,exclude:generated",
        offline=True,
    )

    assert read_text(repo, ".git/config") == "target\n"
    assert read_text(repo, "generated/base.txt") == "target generated\n"
    assert not repo.file_exists("generated/cache.txt")
    assert repo.file_exists("feature.txt")
    assert_no_conflict_sidecars(repo, ".git/config")
    assert_no_conflict_sidecars(repo, "generated/base.txt")
