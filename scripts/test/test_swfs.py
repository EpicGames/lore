import os

import pytest
from error_types import RepositoryAlreadyExistsError, SwfsOutsideServiceError
from service_util import LORE_SERVICE_ENVIRONMENT
from test_repository_info import get_instance_id
from test_shared_store import per_url_store_path

from lore import Lore


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


@pytest.mark.skip(
    reason="swfs is generally not available; test fails randomly under parallel execution"
)
@pytest.mark.smoke
def test_swfs_creates_external_dot_lore(new_lore_repo, background_lore_service, global_dir_name):
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    repo.repository_create(vfs="swfs", use_shared_store=True)

    instance_id = get_instance_id(repo.repository_info())
    assert instance_id is not None
    external_dot_lore = os.path.join(
        global_dir_name, "data", "external", instance_id, ".lore"
    )

    # Expect the external .lore directory to have been created rather than a local one.
    assert not repo.path_exists(".lore")
    assert os.path.exists(external_dot_lore)

    # Expect the .lore directory to be the same even after performing another operation on the repository.
    repo.write_commit_push("Test", {"abc": os.urandom(1000)})

    assert not repo.path_exists(".lore")
    assert os.path.exists(external_dot_lore)


@pytest.mark.skip(
    reason="swfs is generally not available; test fails randomly under parallel execution"
)
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


@pytest.mark.skip(
    reason="swfs is generally not available; test fails randomly under parallel execution"
)
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


@pytest.mark.skip(
    reason="swfs is generally not available; test fails randomly under parallel execution"
)
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


def _get_stores_size(stores_path: str) -> int:
    local_immutable_path = os.path.join(stores_path, "immutable")
    local_index_path = os.path.join(local_immutable_path, "index")
    local_mutable_path = os.path.join(stores_path, "mutable")

    assert os.path.isdir(stores_path), f"Lore repo was not initialized at {stores_path}"

    total_data_size = 0
    for root, dirs, files in os.walk(local_index_path):
        total_data_size += sum(
            os.path.getsize(os.path.join(root, name)) for name in files
        )

    assert os.path.isdir(local_mutable_path), (
        f"A local mutable store should have been created at {local_mutable_path}"
    )

    return total_data_size


@pytest.mark.smoke
@pytest.mark.skip(reason="swfs not fully supported yet")
def test_caches_file_contents(new_lore_repo, background_lore_service, scratch_dir):
    # Create a repo to clone
    original_repo: Lore = new_lore_repo(
        environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )
    original_repo.write_commit_push(
        "test message",
        {f"f{i}.txt": os.urandom(1024) for i in range(30)},
    )

    # Create the shared store for the SWFS repo
    shared_store_path = str(scratch_dir("caches_file_contents_scratch"))
    original_repo.shared_store_create(original_repo.remote, shared_store_path)
    shared_store_path = per_url_store_path(shared_store_path, original_repo.remote)

    # Create both SWFS and non-SWFS cloned repos and ensure the shared store is larger than the non-SWFS repo's store.
    # This validates that it correctly cached file contents locally.
    swfs_repo = original_repo.clone(vfs="swfs", use_shared_store=True)
    regular_repo = original_repo.clone()

    swfs_size = _get_stores_size(shared_store_path)
    regular_size = _get_stores_size(os.path.join(regular_repo.path, ".lore"))

    assert swfs_size - regular_size > 1024 * 5


@pytest.mark.smoke
@pytest.mark.skip(reason="swfs not fully supported yet")
def test_swfs_actually_mounted(new_lore_repo, lore_service_runner):
    # Create a repo and clone it into a SWFS-backed instance.
    lore_service_runner.start()

    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    file_contents = {f"f{i}.txt": os.urandom(1024) for i in range(4)} | {
        f"nested/f{i}.txt": os.urandom(1024) for i in range(4)
    }
    original_repo.write_commit_push(
        "test message",
        file_contents,
    )

    original_repo.shared_store_create(original_repo.remote)

    swfs_repo = original_repo.clone(vfs="swfs", use_shared_store=True)

    # Assert that the SWFS instance exists both immediately and after a service restart, but not while the service is
    # not running.
    assert os.path.exists(swfs_repo.path)
    for file_name in file_contents:
        assert original_repo.compare_file(swfs_repo, file_name), (
            f"File mismatch: {file_name}"
        )

    lore_service_runner.terminate()

    assert not os.path.exists(swfs_repo.path)

    lore_service_runner.start()

    assert os.path.exists(swfs_repo.path)
    for file_name in file_contents:
        assert original_repo.compare_file(swfs_repo, file_name)
