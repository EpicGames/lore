# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests for repository instance management commands.

Tests the `lore repository instance delete` and `lore repository instance move`
commands added for SWFS instance management.
"""

import logging
import os
import sys

import pytest
from lore import Lore
from error_types import LoreException
from service_util import LORE_SERVICE_ENVIRONMENT

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

        assert "Failed to delete repository directory" in str(exc_info.value)
        assert "manually" in str(exc_info.value)
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

    def test_instance_move_fails_if_parent_missing(self, new_lore_repo, scratch_dir):
        """Moving to a path with non-existent parent should fail."""
        repo: Lore = new_lore_repo()

        # Target path with non-existent parent
        target = scratch_dir("nonexistent_parent", unique=False) / "subdir" / "repo"

        # Move should fail because parent doesn't exist
        with pytest.raises(LoreException) as exc_info:
            repo.repository_instance_move(str(target))

        assert (
            "parent" in str(exc_info.value).lower()
            or "does not exist" in str(exc_info.value).lower()
        ), f"Error should mention parent issue. Got: {exc_info.value}"

        # Nothing was changed: the instance is still at the source, and registered there.
        assert os.path.isdir(repo.dot_path())
        assert not os.path.exists(target.parent)
        listed = repo.repository_instance_list()
        assert repo.path in listed or repo.path.replace("\\", "/") in listed, (
            f"Instance should still be registered at the source. Output: {listed}"
        )
        assert (
            str(target) not in listed and str(target).replace("\\", "/") not in listed
        )

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
