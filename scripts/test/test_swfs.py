import os

import pytest

from lore import Lore
from error_types import SwfsOutsideServiceError, RepositoryAlreadyExistsError
from test_repository_info import get_instance_id
from service_util import LORE_SERVICE_ENVIRONMENT


@pytest.mark.smoke
def test_no_swfs_create_outside_service(new_lore_repo):
    lore: Lore = new_lore_repo(create_repo=False)

    with pytest.raises(SwfsOutsideServiceError):
        lore.repository_create(vfs="swfs", use_shared_store=True)


@pytest.mark.smoke
def test_no_swfs_clone_outside_service(new_lore_repo):
    lore: Lore = new_lore_repo()

    with pytest.raises(SwfsOutsideServiceError):
        lore.clone(vfs="swfs", use_shared_store=True)


@pytest.mark.skip(reason="swfs is generally not available; test fails randomly under parallel execution")
@pytest.mark.smoke
def test_swfs_creates_external_dot_lore(new_lore_repo, background_lore_service):
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    repo.repository_create(vfs="swfs", use_shared_store=True)

    instance_id = get_instance_id(repo.repository_info())
    assert instance_id is not None
    external_dot_lore = os.path.join(
        repo.global_dir, "data", "external", instance_id, ".lore"
    )

    # Expect the external .lore directory to have been created rather than a local one.
    assert not repo.path_exists(".lore")
    assert os.path.exists(external_dot_lore)

    # Expect the .lore directory to be the same even after performing another operation on the repository.
    repo.write_commit_push("Test", {"abc": os.urandom(1000)})

    assert not repo.path_exists(".lore")
    assert os.path.exists(external_dot_lore)


@pytest.mark.skip(reason="swfs is generally not available; test fails randomly under parallel execution")
@pytest.mark.smoke
def test_swfs_repo_prevents_creating_non_swfs_repo(
    new_lore_repo, background_lore_service
):
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    repo.repository_create(vfs="swfs", use_shared_store=True)

    # Creating a second repository on top of the first one should fail, even though there is no local .lore directory.
    # Also the .lore directory should still continue to not exist after the attempt.
    with pytest.raises(RepositoryAlreadyExistsError):
        repo.repository_create("test_name")

    assert not repo.path_exists(".lore")

    lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    with pytest.raises(RepositoryAlreadyExistsError):
        lore.clone(repo.path)


@pytest.mark.skip(reason="swfs is generally not available; test fails randomly under parallel execution")
@pytest.mark.smoke
def test_swfs_repo_prevents_creating_non_swfs_repo_after_restart(
    new_lore_repo, lore_service_runner
):
    lore_service_runner.start()
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    repo.repository_create(vfs="swfs", use_shared_store=True)
    lore_service_runner.terminate_all()
    lore_service_runner.start()

    # Creating a second repository on top of the first one should fail, even though there is no local .lore directory.
    # Also the .lore directory should still continue to not exist after the attempt.
    with pytest.raises(RepositoryAlreadyExistsError):
        repo.repository_create("test_name")

    assert not repo.path_exists(".lore")

    lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    with pytest.raises(RepositoryAlreadyExistsError):
        lore.clone(repo.path)


@pytest.mark.skip(reason="swfs is generally not available; test fails randomly under parallel execution")
@pytest.mark.smoke
def test_swfs_repo_can_be_force_created_over(new_lore_repo, background_lore_service):
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    original_repo_file = "file1.txt"
    new_repo_file = "file2.txt"

    # Create a repo with a file and clone it
    repo.repository_create(vfs="swfs", use_shared_store=True)
    clone_of_original_repo = repo.clone()

    repo.write_commit_push(None, {original_repo_file: os.urandom(1000)})

    # Create a new repository over the original using --force and add a file to it
    repo.repository_create("test_name2", force=True)
    assert repo.path_exists(".lore")

    repo.write_commit_push(None, {new_repo_file: os.urandom(1000)})

    clone_of_original_repo.sync()
    assert clone_of_original_repo.file_exists(original_repo_file)
    assert not clone_of_original_repo.file_exists(new_repo_file)


@pytest.mark.skip(reason="swfs is generally not available; test fails constantly in CI")
@pytest.mark.smoke
def test_swfs_repo_can_be_force_cloned_over(new_lore_repo, background_lore_service):
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    original_repo_file = "file1.txt"
    new_repo_file = "file2.txt"

    repo.repository_create(vfs="swfs", use_shared_store=True)
    repo.write_commit_push(None, {original_repo_file: os.urandom(1000)})

    clone_of_original_repo = repo.clone()

    source_for_force_clone: Lore = new_lore_repo(
        environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    force_cloned_repo = source_for_force_clone.clone(path=repo.path, force=True)
    assert repo.path_exists(".lore")
    assert force_cloned_repo.path_exists(".lore")

    force_cloned_repo.write_commit_push(None, {new_repo_file: os.urandom(1000)})

    clone_of_original_repo.sync()
    assert clone_of_original_repo.file_exists(original_repo_file)
    assert not clone_of_original_repo.file_exists(new_repo_file)
