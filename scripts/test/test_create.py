# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import subprocess
from pathlib import Path

import pytest
from lore import Lore
from error_types import (
    RepositoryAlreadyExistsError,
    ImproperArgumentsError,
    UninitializedRepositoryError,
)
from service_util import LORE_SERVICE_ENVIRONMENT, SERVICE_UNAVAILABLE

logger = logging.getLogger(__name__)


@pytest.mark.smoke
class TestCreate:
    def test_urc_fails_uninitialized(self, new_lore_repo):
        """
        Calling repo in an uninitialized directory should fail.
        """
        repo: Lore = new_lore_repo(create_repo=False)
        # verify the repo is not yet initialized
        assert not os.path.isdir(repo.dot_path()), "Lore repo is already initialized"

        with pytest.raises(UninitializedRepositoryError):
            repo.status()

    def test_create(self, new_lore_repo):
        """
        Initializing a new repo repository should succeed.
        """
        repo: Lore = new_lore_repo(create_repo=False)
        # verify the repo is not yet initialized
        assert not os.path.isdir(repo.dot_path()), "Lore repo is already initialized"

        # initialize Lore repo and verify success
        repo.repository_create()

        # verify repo was initialized
        assert os.path.isdir(repo.dot_path()), "Lore repo was not initialized"

    def test_initialize_twice_fails(self, new_lore_repo):
        """
        Attempting to initialize a repo repository a second time should fail.
        """
        new_repo = new_lore_repo()

        # repo is expected to error when initializing a repo twice
        with pytest.raises(RepositoryAlreadyExistsError):
            new_repo.repository_create()

        # # verify repository still exists
        assert os.path.isdir(new_repo.dot_path()), (
            "Lore repo does not exist after second initialization attempt"
        )

    @pytest.mark.own_vfs("re-creates the repository in place with --force, which a virtual clone refuses")
    def test_initialize_twice_forced(self, new_lore_repo):
        """
        Attempting to initialize a repo repository a second time with a new name using the force flag should succeed.
        """
        new_repo = new_lore_repo()
        new_repo_id = new_repo.get_id()
        new_repo.run(
            [
                "repository",
                "create",
                new_repo.remote + Lore.generate_random_name(),
                "--force",
            ]
        )
        second_repo_id = new_repo.get_id()

        # # verify repository still exists
        assert os.path.isdir(new_repo.dot_path()), (
            "Lore repo does not exist after second initialization"
        )
        assert new_repo_id != second_repo_id, (
            "ID of recreated repo should not be the same as the original repo."
        )

    def test_create_with_repo_id(self, new_lore_repo):
        """
        A repo repository can be initialized with a given repository ID.
        """
        generated_id = Lore.generate_id()
        repo = new_lore_repo(repo_id=generated_id, create_repo=False)
        # verify the repo is not yet initialized
        assert not os.path.isdir(repo.dot_path()), "Lore repo is already initialized"
        repo.repository_create(repo_id=generated_id)
        created_repo_id = repo.get_id()

        # verify repo id matches given id value
        assert generated_id == created_repo_id, (
            "Repo did not initialize with correct ID. Expected "
            + generated_id
            + " but got "
            + created_repo_id
        )

    def test_create_then_delete_by_name(self, new_lore_repo):
        """
        Delete a repo repository by name.
        """
        new_repo = new_lore_repo()
        new_repo.repository_delete()

    def test_recreate_by_name(self, new_lore_repo):
        """
        Recreate a deleted repo repository by name.
        """
        new_repo: Lore = new_lore_repo()
        new_repo.repository_delete()

        new_repo.clear_local_files()

        # recreate repo after deletion using name
        new_lore_repo(remote_path=new_repo.remote_path)

    def test_create_then_delete_by_id(self, new_lore_repo):
        """
        Delete a repo repository by id.
        """
        new_repo = new_lore_repo()

        # delete repo by ID
        new_repo.repository_delete(new_repo.get_id())

    def test_delete_relays_to_the_service(
        self, new_lore_repo, stops_background_services, global_dir_name
    ):
        """A delete in service mode is carried out by the service, not where it
        was called. Shown without a real service by naming an executable that
        cannot be spawned, and controlled against the same delete run locally.
        """
        repo: Lore = new_lore_repo()
        listed = f"{repo.name} ({repo.get_id()})"

        routed = subprocess.run(
            [
                repo.lore_executable_path,
                "--repository",
                repo.path,
                "repository",
                "delete",
                repo.remote_path,
            ],
            capture_output=True,
            text=True,
            env=repo.sandboxed_env(
                **LORE_SERVICE_ENVIRONMENT,
                LORE_SERVICE_EXECUTABLE=str(
                    Path(global_dir_name) / "no-such-lore-binary"
                ),
            ),
            cwd=repo.path,
            check=False,
        )
        assert routed.returncode == SERVICE_UNAVAILABLE, (
            f"expected the service-unavailable code {SERVICE_UNAVAILABLE}, got "
            f"{routed.returncode}: {routed.stdout}{routed.stderr}"
        )
        assert listed in repo.repository_list().splitlines(), (
            "a delete that reached no service must leave the repository"
        )

        repo.repository_delete()
        assert listed not in repo.repository_list().splitlines(), (
            "the same delete run locally must delete the repository"
        )

    def test_delete_through_the_c_api_on_the_service(
        self, new_lore_repo, background_lore_service, lore_library_path
    ):
        """`lore_repository_delete` relays to a running service, which deletes
        the repository."""
        repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
        listed = f"{repo.name} ({repo.get_id()})"

        assert repo.repository_delete_capi(lore_library_path) == 0
        assert listed not in repo.repository_list().splitlines(), (
            "the service must have deleted the repository"
        )

    def test_delete_by_name_resolves_a_relative_repository_against_the_caller(
        self, new_lore_repo, lore_service_runner, tmp_path
    ):
        """A bare name is resolved against the remote of the repository at
        `--repository`. The service runs in a directory unrelated to the
        caller's, so it must resolve a relative path where the caller ran."""
        service_directory = tmp_path / "service_elsewhere"
        service_directory.mkdir()
        lore_service_runner.start(str(service_directory))

        repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
        listed = f"{repo.name} ({repo.get_id()})"

        repo.run(
            ["repository", "delete", repo.name],
            path=os.path.basename(repo.path),
            cwd=os.path.dirname(repo.path),
        )
        assert listed not in repo.repository_list().splitlines(), (
            "the service must have deleted the repository"
        )

    def test_recreate_by_id(self, new_lore_repo):
        """
        Recreate a deleted repo repository by id.
        """
        new_repo = new_lore_repo()

        # get repo ID
        new_repo_id = new_repo.get_id()

        # delete repo by ID
        new_repo.repository_delete(new_repo.get_id())

        # remove local files
        new_repo.clear_local_files()

        # recreate repo after deletion using the same ID
        recreated_repo = new_lore_repo(repo_id=new_repo_id)

        # verify ID of repo is the same as the original ID
        recreated_repo_id = recreated_repo.get_id()
        assert recreated_repo_id == new_repo_id, (
            "ID of recreated repo is not the same as the original repo."
        )

    def test_clone_empty_repo(self, new_lore_repo):
        new_repo: Lore = new_lore_repo()

        cloned_repo: Lore = new_repo.clone()

        cloned_repo.write_commit_push("Something", {"file.txt": "Testing testing"})

        cloned_repo.branch_create("test-branch")

        cloned_repo.write_commit_push("Something 2", {"file.txt": "Testing testing 2"})

        cloned_repo = new_repo.clone(branch="test-branch")
        assert "On branch test-branch revision 2" in cloned_repo.status()

        cloned_repo.clear_local_files()

        cloned_repo = new_repo.clone(revision="test-branch@LATEST")
        assert "On branch test-branch revision 2" in cloned_repo.status()

        cloned_repo.clear_local_files()

        with pytest.raises(ImproperArgumentsError):
            new_repo.clone(branch="test-branch", revision="test-branch@LATEST")

        repo_id = new_repo.get_id()
        new_repo.repository_delete(new_repo.get_id())
        new_repo.clear_local_files()

        new_repo.repository_create(repo_id=repo_id)

        output = new_repo.status()
        assert (
            "On branch main revision 0 -> 0000000000000000000000000000000000000000000000000000000000000000"
            in output
        )
        if "Remote revision " in output:
            assert (
                "Remote revision 0 -> 0000000000000000000000000000000000000000000000000000000000000000"
                in output
            )

        branch_list = new_repo.branch_list()
        assert len(branch_list.remote_branches) == 1

    def test_instance_id_created_on_clone(self, new_lore_repo):
        """
        Cloning a repository creates a .lore/instance file (16 bytes).
        Deleting it and running any command lazily regenerates a new ID.
        """
        repo: Lore = new_lore_repo()
        instance_path = os.path.join(repo.dot_path(), "instance")

        # Instance file should exist after create/clone
        assert os.path.isfile(instance_path), ".lore/instance was not created"
        assert os.path.getsize(instance_path) == 16, (
            f".lore/instance should be 16 bytes, got {os.path.getsize(instance_path)}"
        )

        # Read the original instance ID
        with open(instance_path, "rb") as f:
            original_id = f.read()
        assert len(original_id) == 16
        assert original_id != b"\x00" * 16, "Instance ID should not be all zeros"

        # Delete the instance file
        os.remove(instance_path)
        assert not os.path.exists(instance_path)

        # Running any command should lazily regenerate the instance ID
        repo.status()

        assert os.path.isfile(instance_path), (
            ".lore/instance was not regenerated after deletion"
        )
        assert os.path.getsize(instance_path) == 16

        # The recovered ID should match the original — recovery enumerates
        # instances in the mutable store and matches by path.
        with open(instance_path, "rb") as f:
            regenerated_id = f.read()
        assert regenerated_id == original_id, (
            "Recovered instance ID should match the original"
        )
        assert regenerated_id != b"\x00" * 16

    def test_create_and_list(self, new_lore_repo):
        """
        Create and list repo repositories
        """
        first_repo: Lore = new_lore_repo()
        first_name = first_repo.name
        first_id = first_repo.get_id()

        second_repo: Lore = new_lore_repo(first_repo.name + "_second")
        second_name = second_repo.name
        second_id = second_repo.get_id()

        list = first_repo.repository_list().splitlines()

        assert f"{first_name} ({first_id})" in list
        assert f"{second_name} ({second_id})" in list
