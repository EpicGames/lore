# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import enum
import logging
import os
import shutil

import pytest

from error_types import (
    BranchAdvanced,
    ExistingSharedStore,
    LocalMutableStoreWithSharedStore,
    MissingSharedStore,
    WrongSharedStoreRemote,
    BadSharedStoreRemoteUrl,
)
from test_utils import to_posix
from lore_parsers import SpecificSharedStoreInfo, parse_jsonl
from lore import Lore

logger = logging.getLogger(__name__)

LORE_GLOBAL_PATH_VAR = "LORE_GLOBAL_PATH"

CONFIG_TOML = "shared_store.toml"


class CreationType(enum.Enum):
    CREATE = 1
    CLONE = 2


@pytest.fixture(params=[CreationType.CREATE, CreationType.CLONE])
def create_repo(
    request,
    new_lore_repo,
):
    """
    Makes a test able to be generic over creating an empty repo and cloning an empty repo
    """

    def create_repo_impl(
        use_shared_store: bool | None = None,
        shared_store_path: str | None = None,
        **kwargs,
    ):
        if request.param == CreationType.CREATE:
            repo = new_lore_repo(create_repo=False)
            repo.repository_create(
                use_shared_store=use_shared_store,
                shared_store_path=shared_store_path,
                **kwargs,
            )
            return repo
        else:
            return new_lore_repo().clone(
                use_shared_store=use_shared_store,
                shared_store_path=shared_store_path,
                **kwargs,
            )

    return create_repo_impl


def same_path(path1, path2):
    return os.path.abspath(path1) == os.path.abspath(path2)


def _strip_protocol(url: str) -> str:
    """Strip protocol scheme (e.g. 'lore://') and trailing slash from a URL."""
    url = url.removesuffix("/")
    if "://" in url:
        url = url.split("://", 1)[1]
    return url


def _per_url_store_path(base: str, remote: str) -> str:
    """Path to the shared store for `remote` under base path `base`, mirroring the
    Rust layout <base>/<escaped-url>/shared_store. Each remote URL gets its own
    subdirectory so one base path can back multiple endpoints."""
    escaped = "".join(
        "_" if c in '/\\:*?"<>|' or ord(c) < 32 else c for c in _strip_protocol(remote)
    )
    return os.path.join(base, escaped, "shared_store")


def get_shared_store_info(repo: Lore) -> SpecificSharedStoreInfo:
    logger.error(f"Getting shared store info for {repo.shared_store_info()}")
    return repo.shared_store_info().stores[_strip_protocol(repo.remote)]


def verify_shared_store_repo(
    repo_path: str, shared_store_path: str, previous_data_size: int = 0
) -> int:
    dot_dir = ".lore" if os.path.isdir(os.path.join(repo_path, ".lore")) else ".urc"
    dot_path = os.path.join(repo_path, dot_dir)
    local_immutable_path = os.path.join(dot_path, "immutable")
    local_mutable_path = os.path.join(dot_path, "mutable")
    shared_index_path = os.path.join(shared_store_path, "immutable", "index")
    shared_mutable_path = os.path.join(shared_store_path, "mutable")

    assert os.path.isdir(dot_path), f"Lore repo was not initialized at {dot_path}"

    assert not os.path.exists(local_immutable_path), (
        f"A local immutable store should not have been created at {local_immutable_path}"
    )
    assert not os.path.exists(local_mutable_path), (
        f"A local mutable store should not have been created at {local_mutable_path}"
    )

    total_data_size = 0
    for root, dirs, files in os.walk(shared_index_path):
        total_data_size += sum(
            os.path.getsize(os.path.join(root, name)) for name in files
        )
    assert total_data_size > previous_data_size, (
        f"The shared store index at {shared_index_path} should have had contents with more than {previous_data_size} "
        f"bytes but instead it had {total_data_size} bytes"
    )

    assert os.path.isdir(shared_mutable_path), (
        f"A shared mutable store should have been created at {shared_mutable_path}"
    )

    return total_data_size


@pytest.mark.smoke
def test_create(new_lore_repo, tmp_path_factory):
    repo: Lore = new_lore_repo(create_repo=False)

    # Create a shared store which will implicitly get set as the default
    store1_containing_path = tmp_path_factory.getbasetemp() / "store1"
    repo.shared_store_create(repo.remote, str(store1_containing_path))
    store1_path = _per_url_store_path(str(store1_containing_path), repo.remote)

    assert os.path.exists(store1_path), f"{store1_path} was created but does not exist"

    assert get_shared_store_info(repo) == SpecificSharedStoreInfo(
        path=store1_path, exists=True
    )

    # Create a second shared store without making it the default
    store2_containing_path = tmp_path_factory.getbasetemp() / "store2"
    repo.shared_store_create(
        repo.remote, str(store2_containing_path), make_default=False
    )
    store2_path = _per_url_store_path(str(store2_containing_path), repo.remote)

    assert os.path.exists(store2_path), f"{store2_path} was created but does not exist"

    assert get_shared_store_info(repo) == SpecificSharedStoreInfo(
        path=store1_path, exists=True
    )

    # Create a shared store at the default location and let it be the default store.
    # The exact path varies based on the OS and the user, so just verify that it is different than the previous ones.
    repo.shared_store_create(repo.remote)

    default = get_shared_store_info(repo)
    assert not same_path(default.path, store1_path) and not same_path(
        default.path, store2_path
    ), f"Default Lore store path is {default.path} but should be a new shared store"

    assert os.path.exists(default.path), (
        f"{default.path} was created but does not exist"
    )

    # Create a second default location shared store with a different remote URL. Ensure that both defaults show up and are different.
    other_remote_name = "other_remote_name"

    repo.shared_store_create(other_remote_name, offline=True)

    shared_store_info = repo.shared_store_info()

    assert (
        shared_store_info.stores[other_remote_name].path
        != shared_store_info.stores[_strip_protocol(repo.remote)].path
    )


@pytest.mark.smoke
def test_create_bad_remote(new_lore_repo, tmp_path_factory):
    repo: Lore = new_lore_repo(create_repo=False)

    random_remote_name = repo.generate_random_name()

    with pytest.raises(BadSharedStoreRemoteUrl):
        repo.shared_store_create(random_remote_name)


@pytest.mark.smoke
def test_double_create(new_lore_repo, tmp_path_factory):
    repo: Lore = new_lore_repo(create_repo=False)

    # Create a shared store at the default path twice, ensuring the second one fails
    repo.shared_store_create(repo.remote)

    with pytest.raises(ExistingSharedStore):
        repo.shared_store_create(repo.remote)

    # Create a shared store at a custom path twice, ensuring the second one fails
    store_path = tmp_path_factory.getbasetemp() / "double_created_store"
    repo.shared_store_create(repo.remote, str(store_path))

    with pytest.raises(ExistingSharedStore):
        repo.shared_store_create(repo.remote, str(store_path))


@pytest.mark.smoke
def test_create_repo(new_lore_repo):
    # Create the default shared store
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    shared_store_path = get_shared_store_info(repo).path

    # Create a repo without the shared store and verify that a local immutable store is created
    repo: Lore = new_lore_repo()
    local_immutable_path = os.path.join(repo.dot_path(), "immutable")
    assert os.path.exists(local_immutable_path), (
        f"A local immutable store should have been created at {local_immutable_path}"
    )

    # Create a repo using the shared store and verify it used the correct immutable store
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(use_shared_store=True)
    immutable_store_bytes = verify_shared_store_repo(repo.path, shared_store_path)

    # Add and commit a file and expect the shared store to have grown
    file_path = "file.txt"
    with repo.open_file(file_path, "w+") as file:
        file.writelines("Testing contents")
    repo.stage(file_path)
    repo.commit("Commit")
    verify_shared_store_repo(
        repo.path, shared_store_path, previous_data_size=immutable_store_bytes
    )


@pytest.mark.smoke
def test_create_repo_custom_default(new_lore_repo, tmp_path_factory):
    # Create a different default shared store
    repo: Lore = new_lore_repo(create_repo=False)
    default_store_base = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("default_store")
    )
    repo.shared_store_create(repo.remote, default_store_base)
    default_store_path = _per_url_store_path(default_store_base, repo.remote)

    # Create a repo using the default shared store and verify it used the correct immutable store
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(use_shared_store=True)
    default_immutable_store_bytes = verify_shared_store_repo(
        repo.path, default_store_path
    )

    # Add and commit a file and expect the appropriate shared store to have grown
    file_path = "file.txt"
    with repo.open_file(file_path, "w+") as file:
        file.writelines("Testing contents")
    repo.stage(file_path)
    repo.commit("Commit")
    verify_shared_store_repo(
        repo.path, default_store_path, previous_data_size=default_immutable_store_bytes
    )


@pytest.mark.smoke
def test_create_repo_custom_non_default(new_lore_repo, tmp_path_factory, create_repo):
    # Create a non-default shared store
    repo: Lore = new_lore_repo(create_repo=False)
    non_default_store_base = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("non_default_store")
    )
    repo.shared_store_create(repo.remote, non_default_store_base, make_default=False)

    # Create a repo using the non-default shared store and verify it used the correct immutable store
    repo = create_repo(use_shared_store=True, shared_store_path=non_default_store_base)

    non_default_store_path = _per_url_store_path(non_default_store_base, repo.remote)
    non_default_immutable_store_bytes = verify_shared_store_repo(
        repo.path, non_default_store_path
    )

    # Add and commit a file and expect the appropriate shared store to have grown
    file_path = "file.txt"
    with repo.open_file(file_path, "w+") as file:
        file.writelines("Testing contents")
    repo.stage(file_path)
    repo.commit("Commit")
    verify_shared_store_repo(
        repo.path,
        non_default_store_path,
        previous_data_size=non_default_immutable_store_bytes,
    )


@pytest.mark.smoke
def test_create_repo_relative_path(
    new_lore_repo, tmp_path_factory, create_repo, monkeypatch
):
    # Create a non-default shared store
    repo: Lore = new_lore_repo(create_repo=False)
    non_default_store_path = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("non_default_store")
    )
    try:
        non_default_store_relative_path = os.path.relpath(
            non_default_store_path, os.getcwd()
        )
    except ValueError:
        pytest.skip("No relative path between test location and temp path location")
        return
    repo.shared_store_create(repo.remote, non_default_store_path, make_default=False)

    # Create a repo using the non-default shared store and verify it used the correct immutable store
    repo = create_repo(
        use_shared_store=True,
        shared_store_path=non_default_store_relative_path,
        cwd=os.getcwd(),
    )

    monkeypatch.chdir(repo.path)

    non_default_store_path = _per_url_store_path(non_default_store_path, repo.remote)
    non_default_immutable_store_bytes = verify_shared_store_repo(
        repo.path, non_default_store_path
    )

    # Add and commit a file and expect the appropriate shared store to have grown
    file_path = "file.txt"
    with repo.open_file(file_path, "w+") as file:
        file.writelines("Testing contents")
    repo.stage(file_path)
    repo.commit("Commit")
    verify_shared_store_repo(
        repo.path,
        non_default_store_path,
        previous_data_size=non_default_immutable_store_bytes,
    )


@pytest.mark.smoke
def test_create_two_repos(new_lore_repo, tmp_path_factory, create_repo):
    # Set up a shared store to be shared between two repos
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    shared_store_path = get_shared_store_info(repo).path

    # Create two repos which will both use the default shared store
    repo1: Lore = create_repo(use_shared_store=True)
    size_after_repo1_create = verify_shared_store_repo(
        repo1.path, shared_store_path, previous_data_size=0
    )

    repo2: Lore = create_repo(use_shared_store=True)
    size_after_repo2_create = verify_shared_store_repo(
        repo2.path, shared_store_path, previous_data_size=size_after_repo1_create
    )

    # Create the contents of two files whose size in the immutable store will be much larger than the metadata associated with storing them
    file_name = "test_file.txt"
    large_file_contents = "".join((str(x) for x in range(1000)))
    other_large_file_contents = "".join((str(x) for x in range(1000, 2000)))

    # Add the same commit to both repo1 and repo2. The second identical commit should add less data to the immutable store because the fragments containing file contents are reused.
    repo1.write_commit_push("test commit", {file_name: large_file_contents})
    size_after_repo1_commit = verify_shared_store_repo(
        repo1.path, shared_store_path, previous_data_size=size_after_repo2_create
    )

    repo2.write_commit_push("test commit", {file_name: large_file_contents})
    size_after_repo2_identical_commit = verify_shared_store_repo(
        repo1.path, shared_store_path, previous_data_size=size_after_repo1_commit
    )

    assert (
        size_after_repo2_identical_commit - size_after_repo1_commit
        < size_after_repo1_commit - size_after_repo2_create
    )

    # Add a different commit to repo2. It should add more to the shared store size than the identical commit because its file contents require mostly new fragments to be added.
    repo2.write_commit_push("test commit 2", {file_name: other_large_file_contents})
    size_after_repo2_different_commit = verify_shared_store_repo(
        repo1.path,
        shared_store_path,
        previous_data_size=size_after_repo2_identical_commit,
    )

    assert (
        size_after_repo2_different_commit - size_after_repo2_identical_commit
        > size_after_repo2_identical_commit - size_after_repo1_commit
    )

    # Verify that both repos can recover their contents from the shared store.
    repo1.remove_file(file_name)
    repo1.reset(".")
    with repo1.open_file(file_name, "r") as f:
        assert f.read() == large_file_contents

    repo2.remove_file(file_name)
    repo2.reset(".")
    with repo2.open_file(file_name, "r") as f:
        assert f.read() == other_large_file_contents


@pytest.mark.smoke
def test_shared_store_remote_mismatch_rejected(new_lore_repo, tmp_path_factory):
    """If a store's recorded remote URL does not match the repo's (e.g. a
    hand-edited or corrupted shared_store.toml), loading it is rejected rather than
    serving another endpoint's data."""
    base = str(tmp_path_factory.getbasetemp() / Lore.generate_random_name("mismatch"))
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(use_shared_store=True, shared_store_path=base)

    config_path = os.path.join(_per_url_store_path(base, repo.remote), CONFIG_TOML)
    with open(config_path, "r+") as f:
        contents = f.read().replace(_strip_protocol(repo.remote), "some.other.host")
        f.seek(0)
        f.write(contents)
        f.truncate()

    with pytest.raises(WrongSharedStoreRemote):
        repo.status()


@pytest.mark.smoke
def test_create_repo_remote_different_but_correct(new_lore_repo, create_repo):
    # Create the shared store with a protocol-less url
    repo: Lore = new_lore_repo(create_repo=False)
    no_protocol_remote = _strip_protocol(repo.remote)
    assert no_protocol_remote != repo.remote
    repo.shared_store_create(no_protocol_remote, offline=True)
    shared_store_path = get_shared_store_info(repo).path

    # Create a repo using the shared store and verify it succeeded despite having a slightly different URL
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(use_shared_store=True)
    immutable_store_bytes = verify_shared_store_repo(repo.path, shared_store_path)


@pytest.mark.smoke
def test_deleted_shared_store(new_lore_repo, tmp_path_factory):
    """Deleting a repo's shared store out from under it surfaces as a missing
    store on the next command that loads the repo — the load path does not
    recreate it, only clone/create do."""
    store_base = str(
        tmp_path_factory.getbasetemp()
        / Lore.generate_random_name("deleted_shared_store")
    )
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(use_shared_store=True, shared_store_path=store_base)

    store_path = _per_url_store_path(store_base, repo.remote)
    assert os.path.isdir(store_path)
    shutil.rmtree(store_path)

    with pytest.raises(MissingSharedStore):
        repo.status()


@pytest.mark.smoke
def test_set_and_get_automatic_shared_store(new_lore_repo):
    repo: Lore = new_lore_repo(create_repo=False)

    # Verify default is_automatic is false
    info = repo.shared_store_info()
    assert info.is_automatic is False, "is_automatic should default to false"

    # Enable automatic shared store usage
    repo.shared_store_set_use_automatically(True)

    # Verify is_automatic is now true
    info = repo.shared_store_info()
    assert info.is_automatic is True, "is_automatic should be true after enabling"

    # Disable automatic shared store usage
    repo.shared_store_set_use_automatically(False)

    # Verify is_automatic is now false
    info = repo.shared_store_info()
    assert info.is_automatic is False, "is_automatic should be false after disabling"


@pytest.mark.smoke
def test_automatic_shared_store(new_lore_repo, tmp_path_factory, create_repo):
    """With automatic usage enabled and no shared store yet, creating or cloning
    a repo creates the default-location shared store on demand instead of
    failing."""
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_set_use_automatically(True)

    repo = create_repo()

    info = get_shared_store_info(repo)
    assert info.exists, "Automatic shared store should have been created on demand"
    verify_shared_store_repo(repo.path, info.path)


@pytest.mark.smoke
def test_create_repo_creates_missing_default_shared_store(new_lore_repo, create_repo):
    """Creating or cloning a repo with --use-shared-store and no explicit path
    creates the default-location shared store when none exists yet — the first
    repo for a given endpoint."""
    repo = create_repo(use_shared_store=True)

    info = get_shared_store_info(repo)
    assert info.exists, "Default shared store should have been auto-created"
    verify_shared_store_repo(repo.path, info.path)

    file_path = "file.txt"
    with repo.open_file(file_path, "w+") as file:
        file.writelines("Testing contents")
    repo.stage(file_path)
    repo.commit("Commit")

    status = parse_jsonl(repo.status(json=True), "repositoryStatusRevision")
    assert len(status) == 1 and status[0]["revisionNumber"] > 0
    verify_shared_store_repo(repo.path, info.path)


@pytest.mark.smoke
def test_create_repo_creates_store_for_new_endpoint(new_lore_repo, create_repo):
    """A shared store already registered for one endpoint must not stop a repo
    for a different endpoint from creating its own store. Reproduces the original
    multi-endpoint failure where the second endpoint reported a missing store."""
    other: Lore = new_lore_repo(create_repo=False)
    other_endpoint = "other.endpoint.example"
    other.shared_store_create(other_endpoint, offline=True)

    repo = create_repo(use_shared_store=True)

    info = repo.shared_store_info()
    real_endpoint = _strip_protocol(repo.remote)
    assert info.stores[real_endpoint].exists, (
        "The new endpoint's shared store should have been auto-created"
    )
    assert info.stores[real_endpoint].path != info.stores[other_endpoint].path, (
        "Each endpoint must get its own shared store"
    )
    verify_shared_store_repo(repo.path, info.stores[real_endpoint].path)


@pytest.mark.smoke
def test_explicit_base_hosts_multiple_endpoints(
    new_lore_repo, tmp_path_factory, create_repo
):
    """An explicit --shared-store-path is a base directory holding a per-URL
    store, so a second endpoint pointed at the same base gets its own store
    rather than colliding with the first."""
    base = str(tmp_path_factory.getbasetemp() / Lore.generate_random_name("multi_base"))

    other_endpoint = "other.endpoint.example"
    other: Lore = new_lore_repo(create_repo=False)
    other.shared_store_create(other_endpoint, path=base, offline=True)

    repo = create_repo(use_shared_store=True, shared_store_path=base)

    real_store = _per_url_store_path(base, repo.remote)
    other_store = _per_url_store_path(base, other_endpoint)
    assert real_store != other_store
    assert os.path.isdir(real_store), (
        f"This endpoint's store should have been auto-created at {real_store}"
    )
    assert os.path.isdir(other_store), "The other endpoint's store should still exist"
    verify_shared_store_repo(repo.path, real_store)


@pytest.mark.smoke
@pytest.mark.parametrize("legacy_config_file", [True, False])
def test_legacy_store_migrated_to_per_url_dir(
    new_lore_repo, tmp_path_factory, create_repo, legacy_config_file
):
    """A pre-per-URL store at <base>/shared_store whose recorded remote matches is
    moved into <base>/<remote>/shared_store on the next clone/create and loaded
    from there, rather than a new empty store being created alongside it."""
    base = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("legacy_base")
    )

    seed: Lore = new_lore_repo(create_repo=False)
    seed.shared_store_create(seed.remote, path=base, make_default=False)

    per_url_store = _per_url_store_path(base, seed.remote)
    legacy_store = os.path.join(base, "shared_store")
    shutil.move(per_url_store, legacy_store)
    assert not os.path.exists(per_url_store)
    assert os.path.isdir(legacy_store)
    if legacy_config_file:
        shutil.move(
            os.path.join(legacy_store, "shared_store.toml"),
            os.path.join(legacy_store, CONFIG_TOML),
        )

    repo = create_repo(use_shared_store=True, shared_store_path=base)

    assert os.path.isdir(per_url_store), "Legacy store should have been migrated"
    assert not os.path.exists(legacy_store), "Legacy store should have been moved"
    verify_shared_store_repo(repo.path, per_url_store)


@pytest.mark.smoke
def test_legacy_store_not_migrated_for_different_remote(
    new_lore_repo, tmp_path_factory, create_repo
):
    """A legacy <base>/shared_store recording a different remote is left in place;
    a fresh per-URL store is created for the repo's own remote alongside it."""
    base = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("legacy_other_base")
    )

    other_remote = "other.remote.example"
    seed: Lore = new_lore_repo(create_repo=False)
    seed.shared_store_create(other_remote, path=base, make_default=False, offline=True)

    other_per_url_store = _per_url_store_path(base, other_remote)
    legacy_store = os.path.join(base, "shared_store")
    shutil.move(other_per_url_store, legacy_store)
    assert os.path.isdir(legacy_store)

    repo = create_repo(use_shared_store=True, shared_store_path=base)

    real_store = _per_url_store_path(base, repo.remote)
    assert real_store != legacy_store

    assert os.path.isdir(legacy_store), (
        "Mismatched legacy store should be left in place"
    )
    with open(os.path.join(legacy_store, CONFIG_TOML)) as f:
        assert other_remote in f.read(), (
            "Legacy store should be untouched and keep its own remote"
        )

    assert os.path.isdir(real_store), (
        "A new store should be created for the repo's own remote"
    )
    verify_shared_store_repo(repo.path, real_store)


@pytest.mark.smoke
def test_shared_store_clone_no_local_mutable(new_lore_repo):
    """Clone with --use-shared-store should not create a local .lore/mutable/ directory.
    The mutable store should live in the shared store's mutable/ directory."""
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    shared_store_path = get_shared_store_info(repo).path

    # Clone using the shared store
    cloned: Lore = new_lore_repo(create_repo=False)
    cloned.repository_create(use_shared_store=True)

    local_mutable_path = os.path.join(cloned.dot_path(), "mutable")
    local_immutable_path = os.path.join(cloned.dot_path(), "immutable")
    global_mutable_path = os.path.join(shared_store_path, "mutable")

    assert not os.path.exists(local_mutable_path), (
        f"Local mutable/ should not exist at {local_mutable_path}"
    )
    assert not os.path.exists(local_immutable_path), (
        f"Local immutable/ should not exist at {local_immutable_path}"
    )
    assert os.path.isdir(global_mutable_path), (
        f"Global mutable/ should exist at {global_mutable_path}"
    )

    # Commit a file and verify mutable store data lands in shared store
    file_path = "test.txt"
    with cloned.open_file(file_path, "w+") as f:
        f.write("hello")
    cloned.stage(file_path)
    cloned.commit("test commit")

    # Use JSON status to verify the repo still works after commit
    status_output = cloned.status(json=True)
    status_events = parse_jsonl(status_output, "repositoryStatusRevision")
    assert len(status_events) == 1
    assert status_events[0]["revisionNumber"] > 0

    # Local mutable directory must still not exist after commit
    assert not os.path.exists(local_mutable_path), (
        "Local mutable/ should still not exist after commit"
    )


@pytest.mark.smoke
def test_shared_store_shared_mutable_cross_repo(new_lore_repo):
    """Two different repositories sharing a shared store should both use the
    shared mutable/ directory for their branch data. Verify via JSON status
    that both repos function correctly with the shared mutable store."""
    # Create shared store
    repo_a: Lore = new_lore_repo(create_repo=False)
    repo_a.shared_store_create(repo_a.remote)
    shared_store_path = get_shared_store_info(repo_a).path
    global_mutable_path = os.path.join(shared_store_path, "mutable")

    # Create two different repos, both using the shared store
    repo_a.repository_create(use_shared_store=True)
    repo_b: Lore = new_lore_repo(create_repo=False)
    repo_b.repository_create(use_shared_store=True)

    # Commit a file in each repo
    with repo_a.open_file("a.txt", "w+") as f:
        f.write("from A")
    repo_a.stage("a.txt")
    repo_a.commit("commit from A")

    with repo_b.open_file("b.txt", "w+") as f:
        f.write("from B")
    repo_b.stage("b.txt")
    repo_b.commit("commit from B")

    # Both repos should report valid status via JSON
    status_a = parse_jsonl(repo_a.status(json=True), "repositoryStatusRevision")
    assert len(status_a) == 1 and status_a[0]["revisionNumber"] > 0, (
        "Repo A should have a valid revision after commit"
    )

    status_b = parse_jsonl(repo_b.status(json=True), "repositoryStatusRevision")
    assert len(status_b) == 1 and status_b[0]["revisionNumber"] > 0, (
        "Repo B should have a valid revision after commit"
    )

    # Neither repo should have a local mutable store
    assert not os.path.exists(os.path.join(repo_a.dot_path(), "mutable"))
    assert not os.path.exists(os.path.join(repo_b.dot_path(), "mutable"))

    # The shared global mutable store should have data from both repos
    mutable_index_path = os.path.join(global_mutable_path, "index")
    assert os.path.isdir(mutable_index_path), (
        f"Global mutable store should have index at {mutable_index_path}"
    )


@pytest.mark.smoke
def test_shared_store_local_mutable_store_rejected(new_lore_repo, tmp_path_factory):
    """If a repository has a local mutable/ directory but is configured to use
    a shared store, loading it should fail with an error instructing the user to
    reclone."""
    store_containing_path = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("gs_reject")
    )
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote, store_containing_path)

    # Create a repo using the shared store
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(
        use_shared_store=True, shared_store_path=store_containing_path
    )

    # Artificially create a local mutable/ directory to simulate
    # a pre-upgrade state
    local_mutable = os.path.join(repo.dot_path(), "mutable")
    os.makedirs(local_mutable)

    # Any command that loads the repository should fail
    with pytest.raises(LocalMutableStoreWithSharedStore):
        repo.status()


@pytest.mark.smoke
def test_shared_store_auto_upgrade_mutable_dir(new_lore_repo, tmp_path_factory):
    """If an existing shared store has no mutable/ directory (pre-upgrade),
    creating a new repository that uses it should auto-create the mutable/
    directory."""
    store_containing_path = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("gs_upgrade")
    )
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote, store_containing_path)
    shared_store_path = _per_url_store_path(store_containing_path, repo.remote)

    # Remove the mutable/ directory to simulate a pre-upgrade shared store
    mutable_path = os.path.join(shared_store_path, "mutable")
    shutil.rmtree(mutable_path)
    assert not os.path.exists(mutable_path)

    # Creating a repo that uses the shared store should auto-create mutable/
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(
        use_shared_store=True, shared_store_path=store_containing_path
    )

    assert os.path.isdir(mutable_path), (
        f"mutable/ directory should be auto-created at {mutable_path}"
    )

    # Verify the repo works correctly with the auto-created mutable store
    status_output = repo.status(json=True)
    status_events = parse_jsonl(status_output, "repositoryStatusRevision")
    assert len(status_events) == 1, "Repository should load successfully"


# --- Phase 5: Commit safety and sync strengthening ---


def create_shared_instances(new_lore_repo):
    """Helper: create two instances of the same repo sharing a shared store.

    Returns (repo_a, repo_b) where both share the same global mutable store.
    repo_a has one commit pushed so repo_b can clone from it.
    """
    repo_a: Lore = new_lore_repo(create_repo=False)
    repo_a.shared_store_create(repo_a.remote)

    repo_a.repository_create(use_shared_store=True)

    # Instance A needs at least one commit pushed so instance B can clone
    with repo_a.open_file("init.txt", "w+") as f:
        f.write("initial")
    repo_a.stage("init.txt")
    repo_a.commit("initial commit")
    repo_a.push()

    # Clone instance B from the same remote using the shared shared store
    repo_b: Lore = repo_a.clone(use_shared_store=True)

    return repo_a, repo_b


@pytest.mark.smoke
def test_commit_precondition_branch_advanced(new_lore_repo):
    """When instance A commits, instance B's subsequent commit must fail with
    BranchAdvanced because the branch latest pointer has moved."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Both instances stage changes
    with repo_a.open_file("a.txt", "w+") as f:
        f.write("from A")
    repo_a.stage("a.txt")

    with repo_b.open_file("b.txt", "w+") as f:
        f.write("from B")
    repo_b.stage("b.txt")

    # Instance A commits first — advances branch latest in shared store
    repo_a.commit("commit from A")

    # Instance B's commit must fail — branch latest moved
    with pytest.raises(BranchAdvanced):
        repo_b.commit("commit from B")


@pytest.mark.smoke
def test_commit_precondition_single_instance(new_lore_repo):
    """A single instance using a shared store should commit normally — the
    precondition check passes trivially when no other instance advances the
    branch."""
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    repo.repository_create(use_shared_store=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("content")
    repo.stage("file.txt")
    repo.commit("first commit")

    # Verify commit succeeded via JSON status
    status = parse_jsonl(repo.status(json=True), "repositoryStatusRevision")
    assert len(status) == 1
    assert status[0]["revisionNumber"] > 0

    # Second commit should also work
    with repo.open_file("file2.txt", "w+") as f:
        f.write("more content")
    repo.stage("file2.txt")
    repo.commit("second commit")

    status = parse_jsonl(repo.status(json=True), "repositoryStatusRevision")
    assert status[0]["revisionNumber"] > 1


@pytest.mark.smoke
def test_sync_locally_advanced(new_lore_repo):
    """When instance A commits via the shared mutable store, instance B can
    sync to pick up the new revision without push/sync through the server.
    This should be a fast-forward, not a merge."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Instance A commits — advances branch latest in shared store
    with repo_a.open_file("a.txt", "w+") as f:
        f.write("from A")
    repo_a.stage("a.txt")
    repo_a.commit("commit from A")

    # Get instance A's revision number for comparison
    status_a = parse_jsonl(repo_a.status(json=True), "repositoryStatusRevision")
    rev_a = status_a[0]["revisionNumber"]

    # Instance B syncs — should fast-forward to instance A's commit
    repo_b.sync()

    # Instance B should now be at the same revision as A
    status_b = parse_jsonl(repo_b.status(json=True), "repositoryStatusRevision")
    assert status_b[0]["revision"] == status_a[0]["revision"], (
        "Instance B should have fast-forwarded to instance A's revision"
    )


@pytest.mark.smoke
def test_full_recovery_flow(new_lore_repo):
    """Full recovery flow: instance A commits → instance B's commit fails →
    instance B unstages → syncs → re-stages → commits successfully."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Both instances stage changes
    with repo_a.open_file("a.txt", "w+") as f:
        f.write("from A")
    repo_a.stage("a.txt")

    with repo_b.open_file("b.txt", "w+") as f:
        f.write("from B")
    repo_b.stage("b.txt")

    # Instance A commits first
    repo_a.commit("commit from A")

    # Instance B's commit fails
    with pytest.raises(BranchAdvanced):
        repo_b.commit("commit from B")

    # Recovery: unstage → sync → re-stage → commit
    repo_b.unstage("b.txt")
    repo_b.sync()

    # After sync, instance B should be at instance A's revision
    status_b = parse_jsonl(repo_b.status(json=True), "repositoryStatusRevision")
    status_a = parse_jsonl(repo_a.status(json=True), "repositoryStatusRevision")
    assert status_b[0]["revision"] == status_a[0]["revision"]

    # Re-stage and commit should succeed now
    repo_b.stage("b.txt")
    repo_b.commit("commit from B after recovery")

    # Instance B should be ahead of instance A
    status_b = parse_jsonl(repo_b.status(json=True), "repositoryStatusRevision")
    assert status_b[0]["revisionNumber"] > status_a[0]["revisionNumber"]


@pytest.mark.smoke
def test_shared_instance_clone(new_lore_repo):
    """Cloning a second instance of the same repo with --use-shared-store
    should work even though the branch already exists in the shared mutable
    store."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Both instances should show valid status
    status_a = parse_jsonl(repo_a.status(json=True), "repositoryStatusRevision")
    status_b = parse_jsonl(repo_b.status(json=True), "repositoryStatusRevision")
    assert len(status_a) == 1
    assert len(status_b) == 1

    # Both should be on the same branch
    assert status_a[0]["branch"] == status_b[0]["branch"]

    # Instance B should see instance A's branches via shared mutable store
    branch_output = repo_b.run(["branch", "list"], json=True)
    branch_entries = parse_jsonl(branch_output, "branchListEntry")
    local_branch_names = [
        e["name"] for e in branch_entries if e.get("location") == "local"
    ]
    assert "main" in local_branch_names


@pytest.mark.smoke
def test_sync_locally_advanced_offline(new_lore_repo):
    """When instance A commits via the shared mutable store, instance B can
    sync to pick up the new revision even in offline mode — the revision data
    is already in the shared immutable store, so no server is needed."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Instance A commits offline — fragments go to the shared immutable store
    # without any remote involvement, so instance B can sync offline too.
    with repo_a.open_file("a.txt", "w+") as f:
        f.write("from A")
    repo_a.stage("a.txt", offline=True)
    repo_a.commit("commit from A", offline=True)

    status_a = parse_jsonl(repo_a.status(json=True), "repositoryStatusRevision")

    # Instance B syncs in offline mode — should fast-forward using local_latest.
    # All data is in the shared shared store, no server needed.
    repo_b.sync(offline=True)

    # Instance B should now be at the same revision as A
    status_b = parse_jsonl(repo_b.status(json=True), "repositoryStatusRevision")
    assert status_b[0]["revision"] == status_a[0]["revision"], (
        "Instance B should have fast-forwarded to instance A's revision in offline mode"
    )


@pytest.mark.smoke
def test_sync_locally_advanced_remains_divergent(new_lore_repo):
    """After a local fast-forward sync (picking up another instance's commit),
    the branch status should remain divergent from the remote — isLocalAhead=1.
    After push, it should reset to convergent — isLocalAhead=0."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Instance A commits (not pushed) — advances branch latest in shared store
    with repo_a.open_file("a.txt", "w+") as f:
        f.write("from A")
    repo_a.stage("a.txt")
    repo_a.commit("commit from A")

    # Instance B syncs — fast-forwards to A's unpushed commit
    repo_b.sync()

    # Instance B should see isLocalAhead=1 — local branch has commits remote doesn't
    status_b = parse_jsonl(repo_b.status(json=True), "repositoryStatusRevision")
    assert status_b[0]["isLocalAhead"] == 1, (
        "After local sync, branch should be ahead of remote (isLocalAhead=1)"
    )

    # Push from instance B to sync with remote
    repo_b.push()

    # Now isLocalAhead should be 0
    status_b = parse_jsonl(repo_b.status(json=True), "repositoryStatusRevision")
    assert status_b[0]["isLocalAhead"] == 0, (
        "After push, branch should be in sync with remote (isLocalAhead=0)"
    )


# --- Phase 6: Branch checkout awareness ---


@pytest.mark.smoke
def test_branch_switch_warns_multiple_instance(new_lore_repo):
    """When instance B switches to a branch that instance A already has checked
    out, a branchMultipleInstance event should be emitted containing instance A's
    ID and path."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Both start on main. Create a feature branch in instance A.
    repo_a.run(["branch", "create", "feature-branch"])

    # Instance B switches to the same feature branch — should get a warning
    output = repo_b.run(["branch", "switch", "feature-branch"], json=True)
    events = parse_jsonl(output, "branchMultipleInstance")

    assert len(events) == 1, (
        f"Expected exactly one branchMultipleInstance event, got {len(events)}"
    )
    assert len(events[0]["instanceIds"]) == 1, (
        "Expected one other instance in the warning"
    )
    assert len(events[0]["instancePaths"]) == 1, (
        "Expected one instance path in the warning"
    )
    # The path should point to instance A's working directory
    assert to_posix(repo_a.path) in events[0]["instancePaths"][0], (
        f"Expected instance A's path ({repo_a.path}) in warning, "
        f"got {events[0]['instancePaths'][0]}"
    )


@pytest.mark.smoke
def test_branch_switch_no_self_warning(new_lore_repo):
    """Switching branches on a single instance should not produce a
    branchMultipleInstance event — the check excludes self."""
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    repo.repository_create(use_shared_store=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("content")
    repo.stage("file.txt")
    repo.commit("initial commit")
    repo.push()

    repo.run(["branch", "create", "solo-branch"])

    # Switch back to main — only this instance exists, no warning expected
    output = repo.run(["branch", "switch", "main"], json=True)
    events = parse_jsonl(output, "branchMultipleInstance")

    assert len(events) == 0, (
        f"Expected no branchMultipleInstance event for single instance, got {len(events)}"
    )


@pytest.mark.smoke
def test_branch_switch_stale_instance_no_warning(new_lore_repo):
    """If the other instance's directory has been deleted (stale), the
    branchMultipleInstance event should not include it."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Instance A creates and switches to a feature branch
    repo_a.run(["branch", "create", "stale-test-branch"])

    # Delete instance A's directory to make it stale
    import shutil

    shutil.rmtree(repo_a.path)

    # Instance B switches to the same branch — stale instance should be skipped
    output = repo_b.run(["branch", "switch", "stale-test-branch"], json=True)
    events = parse_jsonl(output, "branchMultipleInstance")

    assert len(events) == 0, (
        f"Expected no branchMultipleInstance event for stale instance, got {len(events)}"
    )


# --- Phase 7: CLI commands ---


@pytest.mark.smoke
def test_instance_list_shows_current(new_lore_repo):
    """instance list on a single shared-store repo should show exactly one
    instance with the current path and branch, not marked as stale."""

    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    repo.repository_create(use_shared_store=True)

    with repo.open_file("file.txt", "w+") as f:
        f.write("content")
    repo.stage("file.txt")
    repo.commit("initial commit")

    output = repo.run(["repository", "instance", "list"], json=True)
    instances = parse_jsonl(output, "repositoryInstance")

    assert len(instances) == 1, f"Expected 1 instance, got {len(instances)}"
    assert to_posix(instances[0]["path"]) == to_posix(repo.path)
    assert instances[0]["stale"] == 0
    assert instances[0]["branchName"] == "main"
    assert instances[0]["revision"] != "0" * 64


@pytest.mark.smoke
def test_instance_list_and_prune(new_lore_repo):
    """Create two instances, verify list shows both. Delete one, verify it
    shows as stale. Prune, verify prune event emitted. List again, verify
    only one remains."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # List should show both instances
    output = repo_a.run(["repository", "instance", "list"], json=True)
    instances = parse_jsonl(output, "repositoryInstance")
    assert len(instances) == 2, f"Expected 2 instances, got {len(instances)}"
    paths = [to_posix(i["path"]) for i in instances]
    assert to_posix(repo_a.path) in paths
    assert to_posix(repo_b.path) in paths

    # Delete instance B's directory
    shutil.rmtree(repo_b.path)

    # List should show B as stale
    output = repo_a.run(["repository", "instance", "list"], json=True)
    instances = parse_jsonl(output, "repositoryInstance")
    assert len(instances) == 2
    stale_instances = [i for i in instances if i["stale"] != 0]
    assert len(stale_instances) == 1
    assert to_posix(stale_instances[0]["path"]) == to_posix(repo_b.path)

    # Prune should emit one instance event for the pruned entry
    output = repo_a.run(["repository", "instance", "prune"], json=True)
    pruned = parse_jsonl(output, "repositoryInstance")
    assert len(pruned) == 1, f"Expected 1 pruned instance, got {len(pruned)}"
    assert to_posix(pruned[0]["path"]) == to_posix(repo_b.path)

    # List should now show only instance A
    output = repo_a.run(["repository", "instance", "list"], json=True)
    instances = parse_jsonl(output, "repositoryInstance")
    assert len(instances) == 1
    assert to_posix(instances[0]["path"]) == to_posix(repo_a.path)
    assert instances[0]["stale"] == 0


@pytest.mark.smoke
def test_instance_prune_nothing_to_prune(new_lore_repo):
    """Prune with no stale instances should produce no instance events."""

    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    repo.repository_create(use_shared_store=True)

    output = repo.run(["repository", "instance", "prune"], json=True)
    pruned = parse_jsonl(output, "repositoryInstance")
    assert len(pruned) == 0, f"Expected no pruned instances, got {len(pruned)}"


@pytest.mark.smoke
def test_config_get_remote_url(new_lore_repo):
    """config get remote_url should return the repository's remote URL."""

    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    repo.repository_create(use_shared_store=True)

    output = repo.run(["repository", "config", "get", "remote_url"], json=True)
    events = parse_jsonl(output, "repositoryConfigGet")
    assert len(events) == 1
    assert events[0]["key"] == "remote_url"
    assert events[0]["value"] != ""
    # The value should match the remote URL used during creation
    assert repo.remote.rstrip("/") in events[0]["value"]


@pytest.mark.smoke
def test_config_get_invalid_key(new_lore_repo):
    """config get with an invalid key should fail."""

    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    repo.repository_create(use_shared_store=True)

    output = repo.run(
        ["repository", "config", "get", "nonexistent_key"], json=True, check=False
    )
    complete = parse_jsonl(output, "complete")
    assert len(complete) == 1
    assert complete[0]["status"] != 0, "config get with invalid key should fail"


@pytest.mark.smoke
def test_update_path_after_move(new_lore_repo, tmp_path_factory):
    """After moving an instance directory, update-path should update the stored
    path so instance list reflects the new location."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Move instance B to a new location
    new_path = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("moved_instance")
    )
    shutil.move(repo_b.path, new_path)

    # Create a new Lore wrapper pointing to the moved location
    moved_repo = Lore(
        lore_executable_path=repo_b.lore_executable_path,
        path=new_path,
        name=repo_b.name,
        global_dir=repo_b.global_dir,
        create_repo=False,
    )

    # Before update-path, instance list from A shows old path (now stale)
    output = repo_a.run(["repository", "instance", "list"], json=True)
    instances = parse_jsonl(output, "repositoryInstance")
    b_instances = [i for i in instances if to_posix(i["path"]) == to_posix(repo_b.path)]
    assert len(b_instances) == 1
    assert b_instances[0]["stale"] != 0

    # Run update-path from the moved instance
    moved_repo.run(["repository", "update-path"])

    # Now instance list from A should show the new path, not stale
    output = repo_a.run(["repository", "instance", "list"], json=True)
    instances = parse_jsonl(output, "repositoryInstance")
    new_path_instances = [
        i for i in instances if to_posix(i["path"]) == to_posix(new_path)
    ]
    assert len(new_path_instances) == 1
    assert new_path_instances[0]["stale"] == 0
    # Old path should no longer appear
    old_path_instances = [
        i for i in instances if to_posix(i["path"]) == to_posix(repo_b.path)
    ]
    assert len(old_path_instances) == 0


@pytest.mark.smoke
def test_background_prune_during_clone(new_lore_repo):
    """Cloning with --use-shared-store should automatically prune stale
    instances in the background."""
    repo_a, repo_b = create_shared_instances(new_lore_repo)

    # Delete instance B to make it stale
    shutil.rmtree(repo_b.path)

    # Verify stale instance exists
    output = repo_a.run(["repository", "instance", "list"], json=True)
    instances = parse_jsonl(output, "repositoryInstance")
    stale = [i for i in instances if i["stale"] != 0]
    assert len(stale) == 1

    # Clone a new instance — background prune should clean up stale entry
    repo_c = repo_a.clone(use_shared_store=True)

    # After clone (which awaits the prune task), stale instance should be gone
    output = repo_c.run(["repository", "instance", "list"], json=True)
    instances = parse_jsonl(output, "repositoryInstance")
    stale = [i for i in instances if i["stale"] != 0]
    assert len(stale) == 0, (
        f"Expected no stale instances after clone with background prune, got {len(stale)}"
    )
    # Should have repo_a and repo_c (B was pruned)
    paths = [to_posix(i["path"]) for i in instances]
    assert to_posix(repo_a.path) in paths
    assert to_posix(repo_c.path) in paths


@pytest.mark.smoke
def test_backwards_compatible_repo(new_lore_repo, tmp_path_factory):
    # Create a shared store at a non-default location
    repo: Lore = new_lore_repo(create_repo=False)
    non_default_store_path = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("legacy_store")
    )
    repo.shared_store_create(repo.remote, non_default_store_path)
    shared_store_path = get_shared_store_info(repo).path

    # Move the shared store to the old global store location
    legacy_global_store_path = (
        shared_store_path.removesuffix("shared_store") + "global_store"
    )
    shutil.move(shared_store_path, legacy_global_store_path)

    # Create a repo using the shared store and verify it correctly found the legacy global_store directory
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(
        use_shared_store=True, shared_store_path=non_default_store_path
    )
    immutable_store_bytes = verify_shared_store_repo(
        repo.path, legacy_global_store_path
    )

    # Modify the repo's config file to use the old global_store* field names
    with repo.open_file(os.path.join(".lore", "config.toml"), mode="r+") as f:
        modified_contents = (
            f.read()
            .replace("shared_store_to_use", "global_store_to_use")
            .replace("use_shared_store", "use_global_store")
            .replace("shared_store_path", "global_store_path")
        )
        f.seek(0)
        f.write(modified_contents)
        f.truncate()

    # Add and commit a file and expect the shared store to have grown
    file_path = "file.txt"
    with repo.open_file(file_path, "w+") as file:
        file.writelines("Testing contents")
    repo.stage(file_path)
    repo.commit("Commit")
    verify_shared_store_repo(
        repo.path, legacy_global_store_path, previous_data_size=immutable_store_bytes
    )


@pytest.mark.smoke
def test_backwards_compatible_config_file(new_lore_repo, tmp_path_factory):
    """A shared store with a config with the old file name will have it moved to the new file name when used"""
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    shared_store_path = get_shared_store_info(repo).path

    # Move the shared store config file to the old location
    legacy_config_path = os.path.join(shared_store_path, "global.toml")
    config_path = os.path.join(shared_store_path, CONFIG_TOML)
    with open(config_path, "r") as config:
        config_contents = config.read()
    os.remove(config_path)
    with open(legacy_config_path, "w") as legacy_config:
        legacy_config.write(config_contents)

    # Create a repo using the shared store and verify it correctly behaved
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(use_shared_store=True)
    verify_shared_store_repo(repo.path, shared_store_path)

    # Assert the config contents are in the correct file only
    assert os.path.exists(config_path)
    assert not os.path.exists(legacy_config_path)
    with open(config_path, "r") as config:
        assert config.read() == config_contents


@pytest.mark.smoke
def test_backwards_compatible_config_file_broken(new_lore_repo, tmp_path_factory):
    """A shared store with a config with the old file name that fails to migrate due to not parsing will still have the
    old file left alone"""
    repo: Lore = new_lore_repo(create_repo=False)
    repo.shared_store_create(repo.remote)
    shared_store_path = get_shared_store_info(repo).path

    # Move the shared store config file to the old location but with bad contents so it won't parse correctly
    legacy_config_path = os.path.join(shared_store_path, "global.toml")
    config_path = os.path.join(shared_store_path, CONFIG_TOML)

    with open(config_path, "r") as config:
        config_contents = config.read()
    os.remove(config_path)

    config_contents += "\nNon toml gibberish"
    with open(legacy_config_path, "w") as legacy_config:
        legacy_config.write(config_contents)

    # Create a repo using the shared store which should fail due to failing to parse the bad config file
    repo: Lore = new_lore_repo(create_repo=False)
    with pytest.raises(MissingSharedStore):
        repo.repository_create(use_shared_store=True)

    # Assert the config contents are in the legacy file as they were before they failed to read
    assert not os.path.exists(config_path)
    assert os.path.exists(legacy_config_path)
    with open(legacy_config_path, "r") as legacy_config:
        assert legacy_config.read() == config_contents
