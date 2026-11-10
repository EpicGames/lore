# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests for repository instance management commands.

Tests the `lore repository instance delete`, `move` and `clone` commands.
"""

import json
import logging
import os
import subprocess
import sys
import time

import pytest
from lore import Lore
from error_types import LoreException
from service_util import LORE_SERVICE_ENVIRONMENT

INVALID_ARGUMENTS = 3
NOT_SUPPORTED = 9

logger = logging.getLogger(__name__)


def _clone_swfs_instance(
    new_lore_repo, lore_service_runner
) -> tuple[Lore, dict[str, bytes]]:
    """Clone a repository into a SWFS instance mounted by the service.

    Callers are marked `swfs`: they need the epicvfs driver, which the harness can't
    see or start.
    """
    lore_service_runner.start()

    origin: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    contents = {
        "root.txt": os.urandom(1024),
        "nested/leaf.txt": os.urandom(1024),
    }
    origin.write_commit_push("Seed", contents)
    origin.shared_store_create(origin.remote)

    swfs_repo: Lore = origin.clone(vfs="swfs", use_shared_store=True)

    # A clone does not inherit its origin's environment; instance commands have
    # to be carried out by the service, which owns the mount.
    swfs_repo.environment_vars = LORE_SERVICE_ENVIRONMENT.copy()
    return swfs_repo, contents


@pytest.mark.smoke
class TestInstanceList:
    """Tests for `lore repository instance list`."""

    def test_instance_list_shows_current_instance(self, new_lore_repo):
        """A freshly created repository should appear in the instance list."""
        repo: Lore = new_lore_repo()

        output = repo.repository_instance_list()

        # The output should contain the repository path
        assert repo.path in output or repo.path.replace("\\", "/") in output, (
            f"Instance list should include current repo path. Output: {output}"
        )


@pytest.mark.smoke
class TestInstanceDelete:
    """Tests for `lore repository instance delete`."""

    def test_instance_delete_dry_run_reports_actions(self, new_lore_repo, scratch_dir):
        """Dry-run mode should report what would be deleted without making changes."""
        repo: Lore = new_lore_repo()
        outside = scratch_dir("delete_dry_run_outside", create=True)

        # Seed with a file and commit so there's something to delete
        with repo.open_file("test.txt", "w") as f:
            f.write("test content\n")
        repo.stage(scan=True, offline=True)
        repo.commit("Test commit", offline=True)

        # Get the instance ID before delete
        instance_list_before = repo.repository_instance_list(cwd=str(outside))

        # Run delete in dry-run mode
        output = repo.repository_instance_delete(dry_run=True, cwd=str(outside))

        # Should report the delete operation
        assert "Deleting instance" in output or "delete" in output.lower(), (
            f"Dry-run should report delete action. Output: {output}"
        )
        assert "(dry run)" in output, f"Dry-run should be labelled. Output: {output}"

        # The repository directory should still exist
        assert os.path.isdir(repo.path), (
            "Repository directory should still exist after dry-run delete"
        )
        assert os.path.isdir(repo.dot_path()), (
            "Repository .lore directory should still exist after dry-run delete"
        )

        # Instance should still be listed
        instance_list_after = repo.repository_instance_list(cwd=str(outside))
        assert (
            repo.path in instance_list_after
            or repo.path.replace("\\", "/") in instance_list_after
        ), "Instance should still be listed after dry-run delete"

    def test_instance_delete_removes_the_instance_directory(
        self, new_lore_repo, scratch_dir
    ):
        """Deleting a regular instance should remove its directory.

        The command runs from outside the repository: the directory cannot be
        deleted while it is a process's working directory on Windows.
        """
        repo: Lore = new_lore_repo()
        with repo.open_file("test.txt", "w") as f:
            f.write("test content\n")
        repo.stage(scan=True, offline=True)
        repo.commit("Test commit", offline=True)

        outside = scratch_dir("delete_outside", create=True)
        repo.repository_instance_delete(cwd=str(outside))

        assert not os.path.exists(repo.path), (
            f"Instance directory should be deleted: {repo.path}"
        )

    def test_instance_delete_removes_the_shared_store_registration(
        self, new_lore_repo, scratch_dir
    ):
        """A deleted instance must leave the registry its shared store keeps."""
        origin: Lore = new_lore_repo()
        origin.write_commit_push("Seed", {"root.txt": os.urandom(1024)})
        origin.shared_store_create(origin.remote)
        doomed = origin.clone(use_shared_store=True)
        survivor = origin.clone(use_shared_store=True)

        def listed(output: str, path: str) -> bool:
            return path in output or path.replace("\\", "/") in output

        before = survivor.repository_instance_list()
        assert listed(before, doomed.path), (
            f"The shared store should list both instances. Output: {before}"
        )

        outside = scratch_dir("delete_shared_outside", create=True)
        doomed.repository_instance_delete(cwd=str(outside))

        assert not os.path.exists(doomed.path), (
            f"The deleted instance's directory should be gone: {doomed.path}"
        )
        after = survivor.repository_instance_list()
        assert not listed(after, doomed.path), (
            f"The deleted instance should no longer be registered. Output: {after}"
        )
        assert listed(after, survivor.path), (
            f"The surviving instance should still be registered. Output: {after}"
        )

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on an open file"
    )
    def test_instance_delete_failure_still_deregisters_a_shared_store_instance(
        self, new_lore_repo, scratch_dir
    ):
        """The registration goes before the files, so a failed delete leaves only files to remove."""
        origin: Lore = new_lore_repo()
        origin.write_commit_push("Seed", {"root.txt": os.urandom(1024)})
        origin.shared_store_create(origin.remote)
        doomed = origin.clone(use_shared_store=True)
        survivor = origin.clone(use_shared_store=True)
        outside = scratch_dir("delete_failure_shared_outside", create=True)

        # Another process holding a file open stops Windows deleting the directory.
        with open(os.path.join(doomed.path, "root.txt"), "rb"):
            with pytest.raises(LoreException) as exc_info:
                doomed.repository_instance_delete(cwd=str(outside))

        assert "manually" in str(exc_info.value)
        listed = survivor.repository_instance_list()
        assert (
            doomed.path not in listed and doomed.path.replace("\\", "/") not in listed
        ), f"The deregistered instance should not be listed. Output: {listed}"
        assert survivor.path in listed or survivor.path.replace("\\", "/") in listed

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on a working directory"
    )
    def test_instance_delete_dry_run_reports_working_directory_inside_instance(
        self, new_lore_repo
    ):
        """A dry run should find the problem a real delete would hit, and change nothing."""
        repo: Lore = new_lore_repo()

        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_delete(dry_run=True)

        assert "working directory" in str(exc_info.value).lower(), (
            f"Error should name the working directory. Got: {exc_info.value}"
        )
        assert os.path.isdir(repo.dot_path())

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on a working directory"
    )
    def test_instance_delete_refuses_working_directory_inside_instance_via_service(
        self, new_lore_repo, lore_service_runner
    ):
        """The caller's working directory must be checked, not the service's, when a service runs the delete."""
        lore_service_runner.start()
        repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())

        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_delete()

        assert "working directory" in str(exc_info.value).lower(), (
            f"Error should name the working directory. Got: {exc_info.value}"
        )
        assert os.path.isdir(repo.dot_path())

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on a working directory"
    )
    def test_instance_delete_refuses_working_directory_inside_instance(
        self, new_lore_repo
    ):
        """Windows cannot delete a working directory, so the command must say so and change nothing."""
        repo: Lore = new_lore_repo()

        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_delete()

        assert "working directory" in str(exc_info.value).lower(), (
            f"Error should name the working directory. Got: {exc_info.value}"
        )
        assert os.path.isdir(repo.dot_path())
        assert repo.path in repo.repository_instance_list() or (
            repo.path.replace("\\", "/") in repo.repository_instance_list()
        )

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on an open file"
    )
    def test_instance_delete_reports_a_directory_it_cannot_remove(
        self, new_lore_repo, scratch_dir
    ):
        """A delete that cannot remove the directory must fail rather than report success."""
        repo: Lore = new_lore_repo()
        with repo.open_file("test.txt", "w") as f:
            f.write("test content\n")
        repo.stage(scan=True, offline=True)
        repo.commit("Test commit", offline=True)
        outside = scratch_dir("delete_failure_outside", create=True)

        # Another process holding a file open stops Windows deleting the directory.
        with open(os.path.join(repo.path, "test.txt"), "rb"):
            with pytest.raises(LoreException) as exc_info:
                repo.repository_instance_delete(cwd=str(outside))

        # The error names the directory left behind: the instance's own, or the sibling it was
        # renamed to, whose name starts with the instance's path. The command writes the path
        # with forward slashes, so both sides are normalized before they are compared.
        message = os.path.normcase(str(exc_info.value)).lower()
        directory = os.path.normcase(repo.path).lower()
        assert f"failed to delete {directory}" in message, (
            f"Error should name the directory it could not delete. Got: {exc_info.value}"
        )
        assert "delete that directory manually" in message, (
            f"Error should say to delete the directory by hand. Got: {exc_info.value}"
        )
        assert os.path.exists(os.path.join(repo.path, "test.txt"))

    @pytest.mark.skipif(
        sys.platform != "win32", reason="swfs is only available on Windows"
    )
    @pytest.mark.swfs
    def test_instance_delete_unmounts_and_removes_swfs_instance(
        self, new_lore_repo, lore_service_runner, scratch_dir
    ):
        """Deleting a SWFS instance should unmount it and remove its external directory."""
        swfs_repo, _ = _clone_swfs_instance(new_lore_repo, lore_service_runner)
        assert os.path.exists(swfs_repo.path), "SWFS should be mounted"

        # Run from outside the instance, which is a virtual mount.
        outside = scratch_dir("swfs_delete_outside", create=True)
        swfs_repo.repository_instance_delete(dry_run=True, cwd=str(outside))
        assert os.path.exists(swfs_repo.path), "A dry run should leave SWFS mounted"
        swfs_repo.repository_instance_delete(cwd=str(outside))

        assert not os.path.exists(swfs_repo.path), (
            f"SWFS should no longer be mounted at {swfs_repo.path}"
        )


@pytest.mark.smoke
class TestInstanceMove:
    """Tests for `lore repository instance move`."""

    def test_instance_move_fails_if_target_exists(self, new_lore_repo, scratch_dir):
        """Moving to an existing path should fail."""
        repo: Lore = new_lore_repo()

        # Create a target directory that already exists
        target = scratch_dir("move_target", create=True)

        # Move should fail because target exists
        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_move(str(target))

        assert "already exists" in str(exc_info.value).lower(), (
            f"Error should mention target exists. Got: {exc_info.value}"
        )
        assert exc_info.value.returncode == INVALID_ARGUMENTS, exc_info.value

        # Nothing was changed: the instance is still at the source, and registered there.
        assert os.path.isdir(repo.dot_path())
        assert os.path.isdir(target) and not os.listdir(target)
        listed = repo.repository_instance_list()
        assert repo.path in listed or repo.path.replace("\\", "/") in listed, (
            f"Instance should still be registered at the source. Output: {listed}"
        )
        assert (
            str(target) not in listed and str(target).replace("\\", "/") not in listed
        )

    def test_instance_move_creates_a_missing_parent(self, new_lore_repo, scratch_dir):
        """A move creates the directories above its target that don't exist, and a
        dry run creates none of them.

        Runs from outside the repository, as Windows cannot rename a directory that
        is a running process's working directory."""
        repo: Lore = new_lore_repo()
        with repo.open_file("test.txt", "w") as f:
            f.write("test content\n")
        repo.stage(scan=True, offline=True)
        repo.commit("Test commit", offline=True)
        original_path = repo.path
        missing = scratch_dir("missing_parent")
        target = missing / "subdir" / "repo"
        outside = str(missing.parent)

        repo.repository_instance_move(str(target), dry_run=True, cwd=outside)
        assert not os.path.exists(missing), "a dry run creates nothing"

        repo.repository_instance_move(str(target), cwd=outside)

        assert not os.path.exists(original_path), "the instance left the source"
        with open(target / "test.txt") as f:
            assert f.read() == "test content\n", "the instance is at the target"
        listed = repo.repository_instance_list(path=str(target), cwd=outside)
        assert str(target) in listed or str(target).replace("\\", "/") in listed, (
            f"Instance should be registered at the target. Output: {listed}"
        )

    @pytest.mark.own_vfs("checks the .lore an ordinary instance carries into the moved directory")
    def test_instance_move_succeeds_for_regular_instance(
        self, new_lore_repo, scratch_dir
    ):
        """Moving a regular (non-SWFS) instance should relocate the directory.

        The command runs from outside the repository and names it with
        `--repository`: Windows cannot rename a directory that is a running
        process's working directory. The command must also close the
        repository's files before it renames, since Windows refuses to rename
        a directory with any file open inside it.
        """
        repo: Lore = new_lore_repo()

        # Seed with a file
        with repo.open_file("test.txt", "w") as f:
            f.write("test content\n")
        repo.stage(scan=True, offline=True)
        repo.commit("Test commit", offline=True)

        original_path = repo.path

        # Create parent for target but not target itself
        target_parent = scratch_dir("move_target_parent", create=True)
        target = target_parent / "moved_repo"

        # Move should succeed
        output = repo.repository_instance_move(str(target), cwd=str(target_parent))

        assert "moved" in output.lower() or "Moving instance" in output, (
            f"Output should indicate successful move. Output: {output}"
        )

        # Original path should no longer exist (it was moved)
        assert not os.path.exists(original_path), (
            f"Original path should not exist after move: {original_path}"
        )

        # Target path should now exist with the repository
        assert os.path.isdir(target), f"Target path should exist after move: {target}"
        assert os.path.isdir(target / ".lore") or os.path.isdir(target / ".urc"), (
            "Target should contain repository metadata after move"
        )

        # The test file should be present at the new location
        assert os.path.isfile(target / "test.txt"), (
            "Test file should exist at new location"
        )

        # The registration should name the new path and no longer the old one
        listed = repo.repository_instance_list(path=str(target), cwd=str(target_parent))
        assert str(target) in listed or str(target).replace("\\", "/") in listed, (
            f"Instance should be registered at the target. Output: {listed}"
        )
        assert (
            original_path not in listed
            and original_path.replace("\\", "/") not in listed
        ), f"Instance should no longer be registered at the source. Output: {listed}"

    @pytest.mark.own_vfs("checks the .lore an ordinary instance carries into the moved directory")
    def test_instance_move_resolves_a_relative_target_against_the_working_directory(
        self, new_lore_repo, scratch_dir
    ):
        """A relative target names a path beside where the command was run, not beside the process's own."""
        repo: Lore = new_lore_repo()
        original_path = repo.path
        working_directory = scratch_dir("move_relative_cwd", create=True)

        repo.repository_instance_move("moved_repo", cwd=str(working_directory))

        target = working_directory / "moved_repo"
        assert os.path.isdir(target / ".lore") or os.path.isdir(target / ".urc")
        assert not os.path.exists(original_path)
        listed = repo.repository_instance_list(
            path=str(target), cwd=str(working_directory)
        )
        assert str(target) in listed or str(target).replace("\\", "/") in listed, (
            f"Instance should be registered at the resolved target. Output: {listed}"
        )

    def test_instance_move_resolves_a_relative_target_for_a_service(
        self, new_lore_repo, lore_service_runner, scratch_dir
    ):
        """A service must resolve a relative target against the caller's directory, not its own."""
        lore_service_runner.start()
        repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
        original_path = repo.path
        working_directory = scratch_dir("move_relative_service_cwd", create=True)

        repo.repository_instance_move("moved_repo", cwd=str(working_directory))

        target = working_directory / "moved_repo"
        assert os.path.isdir(target / ".lore") or os.path.isdir(target / ".urc"), (
            f"The instance should be at {target}"
        )
        assert not os.path.exists(original_path)
        listed = repo.repository_instance_list(
            path=str(target), cwd=str(working_directory)
        )
        assert str(target) in listed or str(target).replace("\\", "/") in listed, (
            f"Instance should be registered at the resolved target. Output: {listed}"
        )

    def test_instance_move_succeeds_for_shared_store_instance(
        self, new_lore_repo, scratch_dir
    ):
        """Moving an instance that uses a shared store should relocate it and its registration."""
        origin: Lore = new_lore_repo()
        origin.write_commit_push("Seed", {"root.txt": os.urandom(1024)})
        origin.shared_store_create(origin.remote)
        instance = origin.clone(use_shared_store=True)
        original_path = instance.path

        target_parent = scratch_dir("shared_move_target_parent", create=True)
        target = target_parent / "moved_repo"
        instance.repository_instance_move(str(target), cwd=str(target_parent))

        assert not os.path.exists(original_path)
        assert os.path.isfile(target / "root.txt")
        listed = instance.repository_instance_list(
            path=str(target), cwd=str(target_parent)
        )
        assert str(target) in listed or str(target).replace("\\", "/") in listed, (
            f"Instance should be registered at the target. Output: {listed}"
        )
        assert (
            original_path not in listed
            and original_path.replace("\\", "/") not in listed
        ), f"Instance should no longer be registered at the source. Output: {listed}"

    def test_instance_move_dry_run_changes_nothing(self, new_lore_repo, scratch_dir):
        """A dry run should report the move as possible and leave everything where it is."""
        repo: Lore = new_lore_repo()
        with repo.open_file("test.txt", "w") as f:
            f.write("test content\n")
        repo.stage(scan=True, offline=True)
        repo.commit("Test commit", offline=True)
        target_parent = scratch_dir("move_dry_run_target_parent", create=True)
        target = target_parent / "moved_repo"

        output = repo.repository_instance_move(
            str(target), dry_run=True, cwd=str(target_parent)
        )

        assert "dry run" in output.lower(), (
            f"Output should say it was a dry run: {output}"
        )
        assert os.path.isfile(os.path.join(repo.path, "test.txt"))
        assert not os.path.exists(target)
        listed = repo.repository_instance_list(cwd=str(target_parent))
        assert repo.path in listed or repo.path.replace("\\", "/") in listed, (
            f"Instance should still be registered at the source. Output: {listed}"
        )
        assert (
            str(target) not in listed and str(target).replace("\\", "/") not in listed
        )

    def test_instance_move_global_dry_run_changes_nothing(
        self, new_lore_repo, scratch_dir
    ):
        """The global --dry-run flag must not let the move through."""
        repo: Lore = new_lore_repo()
        target_parent = scratch_dir("move_global_dry_run_target_parent", create=True)
        target = target_parent / "moved_repo"

        repo.run(
            ["repository", "instance", "move", str(target)],
            dry_run=True,
            cwd=str(target_parent),
        )

        assert os.path.isdir(repo.dot_path())
        assert not os.path.exists(target)

    def test_instance_move_dry_run_reports_existing_target(
        self, new_lore_repo, scratch_dir
    ):
        """A dry run should fail on the same bad target a real move would."""
        repo: Lore = new_lore_repo()
        target = scratch_dir("move_dry_run_existing", create=True)

        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_move(str(target), dry_run=True, cwd=str(target))

        assert "already exists" in str(exc_info.value).lower(), (
            f"Error should mention target exists. Got: {exc_info.value}"
        )
        assert exc_info.value.returncode == INVALID_ARGUMENTS, exc_info.value

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on a working directory"
    )
    def test_instance_move_dry_run_reports_working_directory_inside_instance(
        self, new_lore_repo, scratch_dir
    ):
        """A dry run should find the problem a real move would hit, and change nothing."""
        repo: Lore = new_lore_repo()
        target = (
            scratch_dir("move_dry_run_cwd_target_parent", create=True) / "moved_repo"
        )

        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_move(str(target), dry_run=True)

        assert "working directory" in str(exc_info.value).lower(), (
            f"Error should name the working directory. Got: {exc_info.value}"
        )
        assert not os.path.exists(target)

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on a working directory"
    )
    def test_instance_move_refuses_working_directory_inside_instance_via_service(
        self, new_lore_repo, lore_service_runner, scratch_dir
    ):
        """The caller's working directory must be checked, not the service's, when a service runs the move."""
        lore_service_runner.start()
        repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
        target = (
            scratch_dir("move_cwd_service_target_parent", create=True) / "moved_repo"
        )

        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_move(str(target))

        assert "working directory" in str(exc_info.value).lower(), (
            f"Error should name the working directory. Got: {exc_info.value}"
        )
        assert os.path.isdir(repo.dot_path())
        assert not os.path.exists(target)

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on a working directory"
    )
    def test_instance_move_refuses_working_directory_inside_instance(
        self, new_lore_repo, scratch_dir
    ):
        """Windows cannot rename a working directory, so the command must say so and change nothing."""
        repo: Lore = new_lore_repo()
        target = scratch_dir("move_cwd_target_parent", create=True) / "moved_repo"

        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_move(str(target))

        assert "working directory" in str(exc_info.value).lower(), (
            f"Error should name the working directory. Got: {exc_info.value}"
        )
        assert os.path.isdir(repo.dot_path())
        assert not os.path.exists(target)

    @pytest.mark.skipif(
        sys.platform != "win32", reason="Only Windows blocks on an open file"
    )
    def test_instance_move_failure_keeps_the_registration(
        self, new_lore_repo, scratch_dir
    ):
        """A move whose rename fails must leave the instance registered at the source."""
        repo: Lore = new_lore_repo()
        with repo.open_file("test.txt", "w") as f:
            f.write("test content\n")
        repo.stage(scan=True, offline=True)
        repo.commit("Test commit", offline=True)
        target_parent = scratch_dir("move_failure_target_parent", create=True)
        target = target_parent / "moved_repo"

        # Another process holding a file open stops Windows renaming the directory.
        with open(os.path.join(repo.path, "test.txt"), "rb"):
            with pytest.raises(LoreException) as exc_info:
                repo.repository_instance_move(str(target), cwd=str(target_parent))

            assert "update-path" in str(exc_info.value), (
                f"Error should say how to move the directory manually. Got: {exc_info.value}"
            )

            listed = repo.repository_instance_list(cwd=str(target_parent))
            assert repo.path in listed or repo.path.replace("\\", "/") in listed, (
                f"Instance should still be registered at the source. Output: {listed}"
            )
            assert (
                str(target) not in listed
                and str(target).replace("\\", "/") not in listed
            ), f"Instance must not be registered at the target. Output: {listed}"
        assert os.path.isdir(repo.dot_path())
        assert not os.path.exists(target)

    @pytest.mark.skipif(
        sys.platform != "win32", reason="swfs is only available on Windows"
    )
    @pytest.mark.swfs
    def test_instance_move_remounts_swfs_instance(
        self, new_lore_repo, lore_service_runner, scratch_dir
    ):
        """Moving a SWFS instance should remount the virtual filesystem at the target.

        The service keeps the instance mounted by SWFS at its path. A move that
        only re-keys the service's bookkeeping leaves SWFS mounted at the
        source while the service believes the instance is at the target.
        """
        swfs_repo, contents = _clone_swfs_instance(new_lore_repo, lore_service_runner)

        source = swfs_repo.path
        assert os.path.exists(source), "SWFS should be mounted at the source"

        target_parent = scratch_dir("swfs_move_target_parent", create=True)
        target = target_parent / "moved_swfs"

        # Run from outside the instance, which is a virtual mount.
        swfs_repo.repository_instance_move(
            str(target), dry_run=True, cwd=str(target_parent)
        )
        assert os.path.exists(source), (
            "A dry run should leave SWFS mounted at the source"
        )
        assert not os.path.exists(target), "A dry run should not mount the target"
        swfs_repo.repository_instance_move(str(target), cwd=str(target_parent))

        assert not os.path.exists(source), (
            f"SWFS should no longer be mounted at the source: {source}"
        )
        assert os.path.isdir(target), f"SWFS should be mounted at the target: {target}"
        for name, expected in contents.items():
            assert (target / name).read_bytes() == expected, (
                f"{name} should be readable through the mount at the target"
            )


def _shared_store_instance(new_lore_repo, contents: dict[str, bytes]) -> Lore:
    """An instance on a shared store, synced to a revision holding `contents`."""
    origin: Lore = new_lore_repo()
    origin.write_commit_push("Seed", contents)
    origin.shared_store_create(origin.remote)
    return origin.clone(use_shared_store=True)


def _read(root, relative: str) -> bytes:
    with open(os.path.join(root, relative), "rb") as file:
        return file.read()


def _listed(path, listing: str) -> bool:
    return str(path) in listing or str(path).replace("\\", "/") in listing


def _clone_events(source: Lore, destination, *flags: str, check: bool = True) -> list[dict]:
    """The events `lore repository instance clone` sends, as JSON objects."""
    output = source.run(
        ["repository", "instance", "clone", str(destination), *flags], json=True, check=check
    )
    return [json.loads(line) for line in output.splitlines() if line.startswith("{")]


def _events_named(events: list[dict], name: str) -> list[dict]:
    return [event["data"] for event in events if event.get("tagName") == name]


def _event(events: list[dict], name: str) -> dict:
    found = _events_named(events, name)
    assert found, f"no {name} event in {events}"
    return found[0]


def _uuid7_hex() -> str:
    """A UUIDv7 as 32 hex digits, the form a clone names its in-progress directory with."""
    milliseconds = time.time_ns() // 1_000_000
    rest = int.from_bytes(os.urandom(10), "big")
    value = (milliseconds << 80) | (0x7 << 76) | ((rest >> 64) & 0xFFF) << 64
    value |= (0b10 << 62) | (rest & ((1 << 62) - 1))
    return f"{value:032x}"


@pytest.fixture
def other_volume(scratch_dir):
    """A small HFS+ volume mounted for the test, which shares no blocks with the volume the
    repositories are on and cannot clone files itself, but keeps permissions and links."""
    if sys.platform != "darwin":
        pytest.skip("the volume is made with hdiutil, which only macOS has")
    image = scratch_dir("other_volume_image", create=True) / "volume.dmg"
    mount = scratch_dir("other_volume", create=True)
    for command in (
        ["hdiutil", "create", "-size", "50m", "-fs", "HFS+", "-volname", "LoreTest", str(image)],
        ["hdiutil", "attach", "-nobrowse", "-mountpoint", str(mount), str(image)],
    ):
        made = subprocess.run(command, capture_output=True, text=True)
        if made.returncode != 0:
            pytest.skip(f"hdiutil cannot make a volume here: {made.stderr}")
    try:
        yield mount
    finally:
        subprocess.run(["hdiutil", "detach", "-force", str(mount)], capture_output=True)


@pytest.mark.smoke
@pytest.mark.own_vfs(
    "an instance clone copies an on-disk instance; a virtual instance's provider refuses it"
)
class TestInstanceClone:
    """Tests for `lore repository instance clone`."""

    def test_instance_clone_is_a_registered_clean_copy(self, new_lore_repo, scratch_dir):
        """A clone holds the source's files, has an ID of its own, and is registered."""
        contents = {"root.txt": os.urandom(1024), "nested/leaf.txt": os.urandom(4096)}
        source = _shared_store_instance(new_lore_repo, contents)
        destination = scratch_dir("clone_destination")

        clone = source.repository_instance_clone(str(destination), allow_copy=True)

        for relative, data in contents.items():
            assert _read(destination, relative) == data, f"{relative} differs in the clone"
        source_id = _read(source.dot_path(), "instance")
        clone_id = _read(clone.dot_path(), "instance")
        assert clone_id != source_id, "the clone needs an instance ID of its own"
        listed = source.repository_instance_list()
        assert _listed(destination, listed) and _listed(source.path, listed), (
            f"Both instances should be registered. Output: {listed}"
        )
        assert clone_id.hex() in listed
        status = clone.status(scan=True)
        assert "root.txt" not in status and "leaf.txt" not in status, (
            f"A fresh clone should have a clean status. Output: {status}"
        )
        branch_line = [line for line in status.splitlines() if line.startswith("On branch")]
        assert branch_line and branch_line[0] in source.status(), (
            "the clone is on the source's branch and revision"
        )

    def test_instance_clone_first_scan_trusts_the_inherited_records(
        self, new_lore_repo, scratch_dir
    ):
        """The clone's first scan hashes no file its records cover.

        A file changed in the clone behind the scan's back, keeping its size and modification
        time, is only seen by hashing it, so a scan that reports nothing hashed nothing.
        """
        contents = {"root.txt": b"a" * 4096}
        source = _shared_store_instance(new_lore_repo, contents)
        destination = scratch_dir("clone_records")

        clone = source.repository_instance_clone(str(destination), allow_copy=True)

        path = os.path.join(destination, "root.txt")
        stat = os.stat(path)
        with open(path, "r+b") as file:
            file.write(b"b" * 4096)
        os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        status = clone.status(scan=True)
        assert "root.txt" not in status, (
            f"The first scan should trust the records the clone inherited. Output: {status}"
        )

    def test_instance_clone_refuses_an_existing_destination(
        self, new_lore_repo, scratch_dir
    ):
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        destination = scratch_dir("clone_existing", create=True)

        with pytest.raises(LoreException) as exc_info:
            source.repository_instance_clone(str(destination))
        assert exc_info.value.returncode == INVALID_ARGUMENTS, exc_info.value

        assert not os.listdir(destination)

    def test_instance_clone_refuses_a_destination_inside_an_instance(
        self, new_lore_repo
    ):
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        destination = os.path.join(source.path, "inside")

        with pytest.raises(LoreException) as exc_info:
            source.repository_instance_clone(destination)
        assert exc_info.value.returncode == INVALID_ARGUMENTS, exc_info.value

        assert not os.path.exists(destination)

    def test_instance_clone_refuses_an_instance_with_a_local_store(
        self, new_lore_repo, scratch_dir
    ):
        repo: Lore = new_lore_repo()
        destination = scratch_dir("clone_local_store")

        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_clone(str(destination))
        assert exc_info.value.returncode == NOT_SUPPORTED, exc_info.value

        assert not os.path.exists(destination)

    def test_instance_clone_dry_run_changes_nothing(self, new_lore_repo, scratch_dir):
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        destination = scratch_dir("clone_dry_run")

        source.repository_instance_clone(str(destination), dry_run=True, allow_copy=True)

        assert not os.path.exists(destination)
        assert not _listed(destination, source.repository_instance_list())

    def test_instance_clone_dry_run_states_that_it_shares_blocks(
        self, new_lore_repo, scratch_dir
    ):
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        destination = scratch_dir("clone_dry_run_answer")

        events = _clone_events(source, destination, "--allow-copy", "--dry-run")

        begin = _event(events, "repositoryInstanceCloneBegin")
        if begin["method"] == "copiesBytes":
            pytest.skip(f"this volume does not share blocks: {begin['reason']}")
        assert begin["method"] == "sharesBlocks"
        assert not os.path.exists(destination)

    def test_instance_clone_refuses_another_volume(self, other_volume, new_lore_repo):
        """A clone to a volume that cannot share the source's blocks is refused before anything
        is written, naming what a copy would cost."""
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        destination = other_volume / "clone"

        with pytest.raises(LoreException) as exc_info:
            source.repository_instance_clone(str(destination))
        assert exc_info.value.returncode == NOT_SUPPORTED, exc_info.value

        assert not [name for name in os.listdir(other_volume) if name.startswith("clone")]
        assert not _listed(destination, source.repository_instance_list())

    def test_instance_clone_dry_run_states_the_copy(self, other_volume, new_lore_repo):
        contents = {"root.txt": b"a" * 1000, "nested/leaf.txt": b"b" * 24}
        source = _shared_store_instance(new_lore_repo, contents)
        destination = other_volume / "clone"

        events = _clone_events(source, destination, "--allow-copy", "--dry-run")

        begin = _event(events, "repositoryInstanceCloneBegin")
        assert begin["method"] == "copiesBytes"
        assert begin["fileCount"] >= len(contents)
        assert begin["size"] >= 1024
        assert not os.path.exists(destination)

    def test_instance_clone_allow_copy_copies_to_another_volume(
        self, other_volume, new_lore_repo
    ):
        contents = {"root.txt": os.urandom(1024), "nested/leaf.txt": os.urandom(4096)}
        source = _shared_store_instance(new_lore_repo, contents)
        destination = other_volume / "clone"

        clone = source.repository_instance_clone(str(destination), allow_copy=True)

        for relative, data in contents.items():
            assert _read(destination, relative) == data, f"{relative} differs in the copy"
        assert _read(clone.dot_path(), "instance") != _read(source.dot_path(), "instance")
        assert _listed(destination, source.repository_instance_list())
        status = clone.status(scan=True)
        assert "root.txt" not in status and "leaf.txt" not in status, (
            f"A fresh copy should have a clean status. Output: {status}"
        )
        assert not [
            name for name in os.listdir(other_volume) if name.startswith("clone.cloning-")
        ], "the copy leaves no in-progress directory behind"

    def test_instance_clone_removes_a_directory_an_interrupted_clone_left(
        self, new_lore_repo, scratch_dir
    ):
        """A directory an interrupted clone left beside the destination, named for a clone to it
        and marked as a clone in progress, is removed by the next clone there."""
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        parent = scratch_dir("clone_leftover_parent", create=True)
        destination = os.path.join(parent, "clone")
        leftover = f"{destination}.cloning-{_uuid7_hex()}"
        os.makedirs(os.path.join(leftover, ".lore"))
        os.makedirs(os.path.join(leftover, "nested"))
        with open(os.path.join(leftover, ".lore", "clone-in-progress"), "wb"):
            pass
        with open(os.path.join(leftover, "nested", "partial.txt"), "wb") as handle:
            handle.write(b"partial")

        events = _clone_events(source, destination, "--allow-copy")

        removed = [
            progress["path"]
            for progress in _events_named(events, "repositoryInstanceCloneProgress")
            if progress["phase"] == "removingLeftover"
        ]
        assert len(removed) == 1 and removed[0].endswith(os.path.basename(leftover))
        assert sorted(os.listdir(parent)) == ["clone"]
        assert _read(destination, "root.txt") == b"data"

    def test_instance_clone_leaves_siblings_alone(self, new_lore_repo, scratch_dir):
        """Directories beside the destination that a clone did not leave are not taken over:
        one named otherwise, and one named like a clone's but not marked as one."""
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        parent = scratch_dir("clone_sibling_parent", create=True)
        destination = os.path.join(parent, "clone")
        siblings = [destination + ".cloning-notes", f"{destination}.cloning-{_uuid7_hex()}"]
        for sibling in siblings:
            os.makedirs(sibling)
            with open(os.path.join(sibling, "kept.txt"), "wb") as handle:
                handle.write(b"kept")

        source.repository_instance_clone(destination, allow_copy=True)

        for sibling in siblings:
            assert _read(sibling, "kept.txt") == b"kept", sibling
        assert _read(destination, "root.txt") == b"data"
        assert sorted(os.listdir(parent)) == sorted(
            ["clone", *(os.path.basename(sibling) for sibling in siblings)]
        ), "the clone leaves no in-progress directory behind"

    def test_instance_clone_resolves_a_relative_target_against_the_working_directory(
        self, new_lore_repo, scratch_dir
    ):
        """A relative target names a path beside where the command was run."""
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        working_directory = scratch_dir("clone_relative_cwd", create=True)

        source.repository_instance_clone(
            "relative_clone", allow_copy=True, cwd=str(working_directory)
        )

        assert _read(working_directory / "relative_clone", "root.txt") == b"data"
        assert _listed(working_directory / "relative_clone", source.repository_instance_list())

    def test_instance_clone_resolves_a_relative_target_for_a_service(
        self, new_lore_repo, lore_service_runner, scratch_dir
    ):
        """A service resolves a relative target against the caller's directory, not its own."""
        lore_service_runner.start()
        origin: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
        origin.write_commit_push("Seed", {"root.txt": b"data"})
        origin.shared_store_create(origin.remote)
        source = origin.clone(use_shared_store=True)
        working_directory = scratch_dir("clone_relative_service_cwd", create=True)

        source.repository_instance_clone(
            "relative_clone", allow_copy=True, cwd=str(working_directory)
        )

        assert _read(working_directory / "relative_clone", "root.txt") == b"data"
        assert _listed(working_directory / "relative_clone", source.repository_instance_list())

    def test_instance_clone_carries_the_staged_state(self, new_lore_repo, scratch_dir):
        """A file staged in the source is staged in the clone."""
        source = _shared_store_instance(new_lore_repo, {"root.txt": b"data"})
        source.write_files({"staged.txt": b"staged"})
        source.stage("staged.txt")
        destination = scratch_dir("clone_staged")

        clone = source.repository_instance_clone(str(destination), allow_copy=True)

        def staged_lines(status: str) -> list[str]:
            return [line.strip() for line in status.splitlines() if "staged.txt" in line]

        source_lines = staged_lines(source.status())
        assert source_lines, "the source lists the staged file"
        assert staged_lines(clone.status()) == source_lines


@pytest.mark.smoke
class TestInstancePrune:
    """Tests for `lore repository instance prune`."""

    def test_instance_prune_with_no_stale_instances(self, new_lore_repo):
        """Pruning when there are no stale instances should succeed."""
        repo: Lore = new_lore_repo()

        output = repo.repository_instance_prune()

        # Should report no stale instances or successful completion
        assert (
            "No stale instances" in output or "Pruned" in output or output.strip() == ""
        ), f"Prune should report result. Output: {output}"
