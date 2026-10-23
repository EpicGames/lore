# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from lore import Lore

logger = logging.getLogger(__name__)

# RFC 5737 TEST-NET-1 address with a closed port: guaranteed to be
# unreachable, so any attempt to connect would fail rather than succeed.
UNREACHABLE_REMOTE_URL = "lore://192.0.2.1:1/"


@pytest.mark.smoke
class TestCreateOfflineLocal:
    """`repository create` with --offline/--local should work without a
    reachable remote and without requiring a real URL."""

    def test_create_offline_without_remote_url(self, new_lore_repo, monkeypatch):
        """--offline with a bare name succeeds even when no remote URL is set."""
        monkeypatch.delenv("LORE_REMOTE_URL", raising=False)
        repo: Lore = new_lore_repo(create_repo=False)
        assert not os.path.isdir(repo.dot_path()), "Lore repo is already initialized"

        repo.repository_create(offline=True)

        assert os.path.isdir(repo.dot_path()), (
            "Lore repo was not initialized with --offline"
        )

    def test_create_local_without_remote_url(self, new_lore_repo, monkeypatch):
        """--local mirrors --offline: a bare name succeeds with no remote URL."""
        monkeypatch.delenv("LORE_REMOTE_URL", raising=False)
        repo: Lore = new_lore_repo(create_repo=False)
        assert not os.path.isdir(repo.dot_path()), "Lore repo is already initialized"

        repo.repository_create(local=True)

        assert os.path.isdir(repo.dot_path()), (
            "Lore repo was not initialized with --local"
        )

    def test_create_local_remote_unreachable(self, new_lore_repo):
        """--local must not attempt to connect even when the remote is unreachable."""
        repo: Lore = new_lore_repo(create_repo=False, remote_url=UNREACHABLE_REMOTE_URL)
        assert not os.path.isdir(repo.dot_path()), "Lore repo is already initialized"

        repo.repository_create(local=True)

        assert os.path.isdir(repo.dot_path()), (
            "Lore repo was not initialized with --local against an unreachable remote"
        )

    def test_create_offline_remote_unreachable(self, new_lore_repo):
        """--offline must not attempt to connect even when the remote is unreachable."""
        repo: Lore = new_lore_repo(create_repo=False, remote_url=UNREACHABLE_REMOTE_URL)
        assert not os.path.isdir(repo.dot_path()), "Lore repo is already initialized"

        repo.repository_create(offline=True)

        assert os.path.isdir(repo.dot_path()), (
            "Lore repo was not initialized with --offline against an unreachable remote"
        )

    def test_create_dry_run_creates_nothing(self, new_lore_repo):
        """--dry-run creates no repository, and a create after it makes a usable one."""
        repo: Lore = new_lore_repo(create_repo=False)

        output = repo.repository_create(offline=True, dry_run=True)

        assert "Would create repository" in output, output
        assert not os.path.exists(repo.dot_path()), "--dry-run created the repository"

        repo.repository_create(offline=True)
        repo.write_commit_push("First", {"a.txt": b"first\n"}, offline=True)

    def test_create_force_dry_run_keeps_the_existing_repository(self, new_lore_repo):
        """--force --dry-run leaves an existing repository in place."""
        repo: Lore = new_lore_repo(create_repo=False)
        repo.repository_create(offline=True)
        repo.write_commit_push("First", {"a.txt": b"first\n"}, offline=True)
        with open(os.path.join(repo.dot_path(), "id"), "rb") as id_file:
            repository_id = id_file.read()

        repo.repository_create(offline=True, force=True, dry_run=True)

        with open(os.path.join(repo.dot_path(), "id"), "rb") as id_file:
            assert id_file.read() == repository_id, "--dry-run replaced the repository"
        assert "First" in repo.run(["history"], offline=True, no_pager=True)
