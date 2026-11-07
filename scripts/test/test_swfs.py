import os
import re
import sys
import typing

import pytest
from error_types import (
    ImproperArgumentsError,
    NotSupportedError,
    RepositoryAlreadyExistsError,
    SwfsOutsideServiceError,
)
from service_util import LORE_SERVICE_ENVIRONMENT
from test_repository_info import get_instance_id
from test_shared_store import per_url_store_path

from lore import Lore

pytestmark = pytest.mark.own_vfs("makes its SWFS clones from ordinary repositories")

# The most SWFS instances a service mounts, `MountManager::MOUNT_LIMIT`.
SWFS_MOUNT_LIMIT = 8


@pytest.mark.smoke
@pytest.mark.runs_in_process("asserts what a command run outside a service refuses")
def test_no_swfs_create_outside_service(new_lore_repo):
    lore: Lore = new_lore_repo(create_repo=False)

    with pytest.raises(SwfsOutsideServiceError):
        lore.repository_create(vfs="swfs", use_shared_store=True)


@pytest.mark.smoke
@pytest.mark.runs_in_process("asserts what a command run outside a service refuses")
def test_no_swfs_clone_outside_service(new_lore_repo):
    lore: Lore = new_lore_repo()

    with pytest.raises(SwfsOutsideServiceError):
        lore.clone(vfs="swfs", use_shared_store=True)


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_creates_external_dot_lore(
    new_lore_repo, background_lore_service, global_dir_name
):
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    repo.shared_store_create(repo.remote)
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
    repo.status()

    assert not repo.path_exists(".lore")
    assert os.path.exists(external_dot_lore)


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_repo_prevents_creating_non_swfs_repo(
    new_lore_repo, background_lore_service
):
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )

    repo.shared_store_create(repo.remote)
    repo.repository_create(vfs="swfs", use_shared_store=True)

    # Creating a second repository on top of the first one should fail, even though there is no local .lore directory.
    # Also the .lore directory should still continue to not exist after the attempt.
    with pytest.raises(RepositoryAlreadyExistsError):
        repo.repository_create(repo.remote + "/test_name")

    assert not repo.path_exists(".lore")

    lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    with pytest.raises(RepositoryAlreadyExistsError):
        lore.clone(repo.path)

    # Repeat those checks with `--force`, expecting the same results.
    with pytest.raises(RepositoryAlreadyExistsError):
        repo.repository_create(repo.remote + "/test_name", force=True)

    assert not repo.path_exists(".lore")

    with pytest.raises(RepositoryAlreadyExistsError):
        lore.clone(repo.path, force=True)


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


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
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


def _repositories_match_files(
    expected_repo: Lore, actual_repo: Lore, files: typing.Sequence[str]
):
    for file_name in files:
        assert expected_repo.compare_file(
            actual_repo, file_name, only_file_contents=True
        ), f"File mismatch {file_name}"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
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
    _repositories_match_files(original_repo, swfs_repo, file_contents)

    lore_service_runner.terminate()

    assert not os.path.exists(swfs_repo.path)

    lore_service_runner.start()

    _repositories_match_files(original_repo, swfs_repo, file_contents)


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_mount_is_closed_when_its_service_stops(
    new_lore_repo, lore_service_runner, scratch_dir
):
    """A service closes its SWFS mounts when it stops. A mount left open stays with
    the driver, which then cannot be stopped until the machine restarts.

    Read from the debug lines around `swfsInit` and `swfsClose`, which name the
    mount's path and SWFS handle: the clone command reports the mount it made, and
    the stopped service the close of that same handle."""
    lore_service_runner.start(debug=True)
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("test message", {"f.txt": os.urandom(1024)})
    original_repo.shared_store_create(original_repo.remote)

    target = scratch_dir("swfs_closed")
    clone_output = original_repo.run(
        [
            "repository",
            "clone",
            original_repo.remote_path,
            str(target),
            "--vfs",
            "swfs",
            "--use-shared-store",
        ],
        debug=True,
    )
    mounted = _swfs_handles(clone_output, "swfsInit: mounted", target.name)
    assert len(mounted) == 1, f"the clone mounts once: {clone_output}"

    lore_service_runner.terminate()

    service_output = lore_service_runner.output_paths[None].read_text(errors="replace")
    closed = _swfs_handles(service_output, "swfsClose: closed", target.name)
    assert closed == mounted, (
        f"the stopped service closed the clone's mount, SWFS handle {mounted}: "
        f"{service_output}"
    )


def _swfs_handles(output: str, event: str, mount_name: str) -> list[str]:
    """The SWFS handles that `event` debug lines in `output` name for the mount
    whose path ends in `mount_name`."""
    handles = []
    for line in output.splitlines():
        if event in line and mount_name in line:
            match = re.search(r"SWFS handle (0x[0-9a-fA-F]+)", line)
            assert match, f"the debug line names its SWFS handle: {line}"
            handles.append(match.group(1))
    return handles


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_sync(new_lore_repo, background_lore_service):
    # Create the original repo with 2 revisions
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    deleted_file = "deleted.txt"
    nested_deleted_file = "nested/deleted.txt"
    new_file = "new.txt"
    nested_new_file = "nested/new.txt"

    def first_contents():
        return "first contents!"

    def second_contents():
        return "second contents!"

    file_contents = (
        {f"f{i}.txt": first_contents() for i in range(4)}
        | {f"nested/f{i}.txt": first_contents() for i in range(4)}
        | {deleted_file: first_contents(), nested_deleted_file: first_contents()}
    )
    original_repo.write_commit_push(
        "test message",
        file_contents,
    )
    updated_file_contents = {
        file_name: second_contents() for file_name in file_contents
    } | {new_file: second_contents(), nested_new_file: second_contents()}
    del updated_file_contents[deleted_file]
    del updated_file_contents[nested_deleted_file]
    original_repo.remove_file(deleted_file)
    original_repo.remove_file(nested_deleted_file)
    original_repo.stage(".", scan=True)
    original_repo.write_commit_push("update message", updated_file_contents)

    # Clone that repository into an SWFS repo
    original_repo.shared_store_create(original_repo.remote)

    swfs_repo = original_repo.clone(vfs="swfs", use_shared_store=True)

    _repositories_match_files(original_repo, swfs_repo, updated_file_contents)
    assert not swfs_repo.file_exists(deleted_file)
    assert not swfs_repo.file_exists(nested_deleted_file)

    # Sync backwards and forwards, expecting the content to update each way
    swfs_repo.sync("main@1", force=True)
    original_repo.sync("main@1")

    _repositories_match_files(original_repo, swfs_repo, file_contents)
    assert not swfs_repo.file_exists(new_file)
    assert not swfs_repo.file_exists(nested_new_file)

    swfs_repo.sync("main@latest", force=True)
    original_repo.sync("main@latest")

    _repositories_match_files(original_repo, swfs_repo, updated_file_contents)
    assert not swfs_repo.file_exists(deleted_file)
    assert not swfs_repo.file_exists(nested_deleted_file)


def _swfs_clone(original_repo: Lore) -> Lore:
    original_repo.shared_store_create(original_repo.remote)
    return original_repo.clone(vfs="swfs", use_shared_store=True)


def _remove_commit_push(repo: Lore, message: str, removed: typing.Sequence[str]):
    for file_name in removed:
        repo.remove_file(file_name)
    repo.stage(".", scan=True)
    repo.commit(message=message)
    repo.push()


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_sync_removes_file_with_no_other_changes(
    new_lore_repo, background_lore_service
):
    # The second revision only removes files, so nothing else is written during the sync that
    # could cause the thaw to take effect incidentally.
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    removed_file = "removed.txt"
    nested_removed_file = "nested/removed.txt"
    kept_contents = {
        "kept.txt": "kept contents!",
        "nested/kept.txt": "nested kept contents!",
    }
    original_repo.write_commit_push(
        "first",
        kept_contents
        | {removed_file: "to be removed", nested_removed_file: "nested to be removed"},
    )
    _remove_commit_push(
        original_repo, "remove only", [removed_file, nested_removed_file]
    )

    swfs_repo = _swfs_clone(original_repo)
    assert not swfs_repo.file_exists(removed_file)
    assert not swfs_repo.file_exists(nested_removed_file)

    swfs_repo.sync("main@1", force=True)
    assert swfs_repo.file_exists(removed_file)
    assert swfs_repo.file_exists(nested_removed_file)

    swfs_repo.sync("main@latest", force=True)
    assert not swfs_repo.file_exists(removed_file)
    assert not swfs_repo.file_exists(nested_removed_file)
    _repositories_match_files(original_repo, swfs_repo, kept_contents)


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_sync_force_clears_local_writes(new_lore_repo, background_lore_service):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    unchanged_contents = {
        "unchanged.txt": "unchanged contents",
        "nested/unchanged.txt": "nested unchanged contents",
    }
    original_repo.write_commit_push(
        "first",
        unchanged_contents
        | {
            "changed.txt": "first changed",
            "nested/changed.txt": "first nested changed",
        },
    )
    changed_contents = {
        "changed.txt": "second changed",
        "nested/changed.txt": "second nested changed",
    }
    original_repo.write_commit_push("second", changed_contents)

    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.sync("main@1", force=True)
    original_repo.sync("main@1")

    # Overwrite tracked files through the mount, with contents of a different length: some the
    # next revision changes, some it leaves alone.
    local_contents = {
        "changed.txt": "locally written changed, longer than the original",
        "nested/changed.txt": "local",
        "unchanged.txt": "locally written unchanged, longer than the original",
        "nested/unchanged.txt": "local",
    }
    swfs_repo.write_files(local_contents, flush_writes=True)
    for file_name in local_contents:
        assert not original_repo.compare_file(
            swfs_repo, file_name, only_file_contents=True
        )

    swfs_repo.sync("main@latest", force=True)
    original_repo.sync("main@latest")

    # The files the sync changed hold the synced contents, the local writes gone.
    _repositories_match_files(original_repo, swfs_repo, list(changed_contents))
    # As on a plain filesystem, a forced sync leaves alone files it has no change for.
    for file_name in unchanged_contents:
        with swfs_repo.open_file(file_name) as unchanged_file:
            assert unchanged_file.read() == local_contents[file_name]


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_sync_force_removes_locally_written_file(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    removed_file = "removed.txt"
    nested_removed_file = "nested/removed.txt"
    kept_contents = {"kept.txt": "kept contents!"}
    original_repo.write_commit_push(
        "first",
        kept_contents
        | {removed_file: "to be removed", nested_removed_file: "nested to be removed"},
    )
    _remove_commit_push(
        original_repo, "remove only", [removed_file, nested_removed_file]
    )

    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.sync("main@1", force=True)
    assert swfs_repo.file_exists(removed_file)
    assert swfs_repo.file_exists(nested_removed_file)

    # Write locally to the files the next revision removes, so SWFS holds changes to them.
    swfs_repo.write_files(
        {
            removed_file: "locally written, longer than the original contents",
            nested_removed_file: "local",
        }
    )

    swfs_repo.sync("main@latest", force=True)

    assert not swfs_repo.file_exists(removed_file)
    assert not swfs_repo.file_exists(nested_removed_file)
    _repositories_match_files(original_repo, swfs_repo, kept_contents)


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_refuses_a_repository_nested_in_its_mount(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"a.txt": "a"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.make_dirs("nested")

    # A repository with its own state inside the mount, unlike a link or a layer, which is the
    # parent's own content.
    with pytest.raises(ImproperArgumentsError):
        swfs_repo.run(
            ["repository", "create", "nested"],
            path=os.path.join(swfs_repo.path, "nested"),
            offline=True,
        )
    assert not swfs_repo.path_exists(os.path.join("nested", ".lore"))


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_instance_is_refused_inside_another_repository(
    new_lore_repo, background_lore_service
):
    # A plain repository, named so even under --all-swfs: the parent's own SWFS mount would be
    # refused for the other reason.
    parent = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )
    parent.repository_create(vfs="none")
    parent.write_commit_push("first", {"a.txt": "a"})
    parent.shared_store_create(parent.remote)

    with pytest.raises(ImproperArgumentsError):
        parent.clone(
            os.path.join(parent.path, "inner"), vfs="swfs", use_shared_store=True
        )


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_refuses_an_instance_past_the_mount_limit(
    new_lore_repo, background_lore_service, global_dir_name
):
    """SWFS attaches about eight mounts and parks the process that asks for one more, so the
    service refuses another SWFS instance, cloned or created, once it mounts that many, before
    any of the instance is made."""
    # A plain repository, named so even under --all-swfs, so that only the clones count.
    source = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )
    source.repository_create(vfs="none")
    source.write_commit_push("first", {"a.txt": "a"})
    source.shared_store_create(source.remote)

    instances = [
        source.clone(vfs="swfs", use_shared_store=True) for _ in range(SWFS_MOUNT_LIMIT)
    ]
    external = os.path.join(global_dir_name, "data", "external")
    assert len(os.listdir(external)) == SWFS_MOUNT_LIMIT

    with pytest.raises(NotSupportedError):
        source.clone(vfs="swfs", use_shared_store=True)
    creator = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )
    with pytest.raises(NotSupportedError):
        creator.repository_create(vfs="swfs", use_shared_store=True)
    assert len(os.listdir(external)) == SWFS_MOUNT_LIMIT, (
        "a refused instance must leave nothing behind"
    )

    # The mounted instances still work, and a plain clone is not limited.
    for repo in (instances[-1], source.clone(vfs="none")):
        with repo.open_file("a.txt") as f:
            assert f.read() == "a"
