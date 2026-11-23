import os
import re
import sys
import typing

import pytest
from error_types import (
    ImproperArgumentsError,
    LoreException,
    NotSupportedError,
    RepositoryAlreadyExistsError,
    SwfsOutsideServiceError,
)
from lore_parsers import parse_jsonl, parse_status_json
from service_util import LORE_SERVICE_ENVIRONMENT, stop_lore_service
from test_repository_info import get_instance_id
from test_utils import unstaged_entries

from lore import Lore, external_root, per_url_store_path

pytestmark = pytest.mark.own_vfs("makes its SWFS clones from ordinary repositories")


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
    repo = _swfs_create(new_lore_repo)

    instance_id = get_instance_id(repo.repository_info())
    assert instance_id is not None
    external_dot_lore = os.path.join(
        external_root(global_dir_name), instance_id, ".lore"
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
    repo = _swfs_create(new_lore_repo)

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


# The most SWFS instances a service mounts, `MountManager::MOUNT_LIMIT`.
SWFS_MOUNT_LIMIT = 8


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
    external = external_root(global_dir_name)
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
    # Named, so a run that mounts every repository through SWFS still compares against one it
    # does not.
    regular_repo = original_repo.clone(vfs="none")

    swfs_size = _get_stores_size(shared_store_path)
    regular_size = _get_stores_size(os.path.join(regular_repo.path, ".lore"))

    assert swfs_size - regular_size > 1024 * 5


def _repositories_match_files(
    expected_repo: Lore, actual_repo: Lore, files: typing.Iterable[str]
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
    file_contents = _random_files()
    original_repo.write_commit_push(
        "test message",
        file_contents,
    )

    swfs_repo = _swfs_clone(original_repo)

    _assert_mounted_across_restart(
        lore_service_runner, original_repo, swfs_repo, file_contents
    )


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_created_instance_actually_mounted(new_lore_repo, lore_service_runner):
    """An SWFS instance made by `lore repository create` is mounted while its service runs,
    syncs what another instance pushed, and is mounted again when the service restarts."""
    lore_service_runner.start()

    swfs_repo = _swfs_create(new_lore_repo)

    plain_repo = swfs_repo.clone(vfs="none")
    file_contents = _random_files()
    plain_repo.write_commit_push("test message", file_contents)

    swfs_repo.sync("main@latest")

    _assert_mounted_across_restart(
        lore_service_runner, plain_repo, swfs_repo, file_contents
    )


def _random_files() -> dict[str, bytes]:
    """Random contents for four files at the root and four in a nested directory."""
    return {f"f{i}.txt": os.urandom(1024) for i in range(4)} | {
        f"nested/f{i}.txt": os.urandom(1024) for i in range(4)
    }


def _assert_mounted_across_restart(
    lore_service_runner,
    expected_repo: Lore,
    swfs_repo: Lore,
    files: typing.Collection[str],
):
    """Asserts that `swfs_repo` holds `expected_repo`'s `files` while the service runs, is
    absent while it is stopped, and holds them again once it restarts."""
    _repositories_match_files(expected_repo, swfs_repo, files)

    lore_service_runner.terminate()

    assert not os.path.exists(swfs_repo.path)

    lore_service_runner.start()

    _repositories_match_files(expected_repo, swfs_repo, files)


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


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_sync_shrinks_a_file_never_listed(new_lore_repo, background_lore_service):
    """A sync that shrinks a file the SWFS instance has never listed or read gives it the new
    content alone. The clone's thaw made SWFS an entry for the file at its long size, and SWFS
    kept that size through the sync's thaw unless its directory was listed between them, so the
    file read as the new content padded with NUL bytes."""
    # A plain repository, named so even under --all-swfs.
    source = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )
    source.repository_create(vfs="none")
    long_contents = b"a source file that is longer than what replaces it\n"
    source.write_commit_push(
        "first", {"src/main.rs": long_contents, "top.txt": long_contents}
    )
    clone = _swfs_clone(source)

    shrunk = {"src/main.rs": b"main v2\n", "top.txt": b"top v2\n"}
    source.write_commit_push("shrink", shrunk)
    clone.sync()

    for name, contents in shrunk.items():
        with clone.open_file(name, "rb") as f:
            assert f.read() == contents, f"{name} read with the old size"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_forced_switch_after_a_commit_keeps_the_working_tree(
    new_lore_repo, background_lore_service
):
    """A forced switch with reset, after a commit, leaves the working tree where it is. The
    commit left the modified-file record to be rebuilt from SWFS's list, and the frozen files SWFS
    lists include the root: kept as a child of itself, it read as a file at the root's path, which
    the reset then removed, leaving the mount with no root."""
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )
    repo.shared_store_create(repo.remote)
    repo.repository_create(vfs="swfs", use_shared_store=True)
    repo.write_commit_push("main file", {"f.txt": b"main\n"})
    repo.branch_create("other")
    repo.write_commit_push("other file", {"f.txt": b"other\n"})

    repo.branch_switch("main", force=True, reset=True)

    with repo.open_file("f.txt", "rb") as f:
        assert f.read() == b"main\n"
    repo.write_files({"after.txt": b"written after the switch\n"})
    assert sorted(os.listdir(repo.path)) == ["after.txt", "f.txt"]


def _swfs_create(new_lore_repo) -> Lore:
    """An SWFS instance made by `lore repository create` over a shared store, in the service."""
    repo: Lore = new_lore_repo(
        create_repo=False, environment_vars=LORE_SERVICE_ENVIRONMENT.copy()
    )
    repo.shared_store_create(repo.remote)
    repo.repository_create(vfs="swfs", use_shared_store=True)
    return repo


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
    swfs_repo.write_files(local_contents)
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
def test_swfs_scan_reports_changes_found_through_the_modified_record(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first",
        {f"f{i}.txt": f"contents {i}" for i in range(4)}
        | {f"nested/n{i}.txt": f"nested {i}" for i in range(4)},
    )
    swfs_repo = _swfs_clone(original_repo)

    # The first scan walks the whole tree to learn what the record holds, and finds it clean.
    assert "A " not in swfs_repo.status(scan=True)

    swfs_repo.write_files(
        {
            "f0.txt": "edited",
            "added.txt": "new",
            "newdir/inside.txt": "new in a new directory",
        }
    )
    swfs_repo.remove_file("nested/n1.txt")
    changes = swfs_repo.status(scan=True)

    assert "M f0.txt" in changes
    assert "D nested/n1.txt" in changes
    assert "A added.txt" in changes
    assert "A newdir/inside.txt" in changes
    assert "f1.txt" not in changes
    assert "nested/n0.txt" not in changes


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_later_scan_keeps_the_changes_an_earlier_scan_found(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {f"f{i}.txt": f"contents {i}" for i in range(4)}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)
    swfs_repo.write_files({"f0.txt": "edited"})
    assert "M f0.txt" in swfs_repo.status(scan=True)

    # A later change joins what the record already holds.
    swfs_repo.write_files({"f2.txt": "edited later"})
    changes = swfs_repo.status(scan=True)

    assert "M f2.txt" in changes
    assert "M f0.txt" in changes


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_scan_after_a_service_restart_still_reports_earlier_changes(
    new_lore_repo, lore_service_runner
):
    lore_service_runner.start()
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {f"f{i}.txt": f"contents {i}" for i in range(4)}
    )
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.status(scan=True)
    swfs_repo.write_files({"f0.txt": "edited", "added.txt": "new"})
    swfs_repo.remove_file("f1.txt")
    before = swfs_repo.status(scan=True)
    assert "M f0.txt" in before

    # SWFS keeps the changes across a restart, and its list of them, which the first operation
    # rebuilds the record from.
    lore_service_runner.terminate()
    lore_service_runner.start()
    after = swfs_repo.status(scan=True)

    assert "M f0.txt" in after
    assert "D f1.txt" in after
    assert "A added.txt" in after
    assert "f2.txt" not in after


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_scan_skips_paths_the_ignore_rules_exclude(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {".loreignore": "build/\n*.tmp\n", "f0.txt": "contents"}
    )
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.status(scan=True)
    swfs_repo.write_files(
        {
            "build/out.bin": "ignored",
            "scratch.tmp": "ignored",
            "kept.txt": "reported",
        },
    )
    changes = swfs_repo.status(scan=True)

    assert "A kept.txt" in changes
    assert "build" not in changes
    assert "scratch.tmp" not in changes


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_scan_after_a_sync_reports_the_changes_the_sync_did_not_replace(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    files = {f"f{i}.txt": f"contents {i}" for i in range(4)}
    original_repo.write_commit_push("first", files)
    original_repo.write_commit_push("second", {"f1.txt": "second contents"})
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.status(scan=True)
    # No scan between this edit and the sync: a sync of an SWFS instance whose staged state holds
    # dirty paths fails to freeze a second time, which is not what this test is about.
    swfs_repo.write_files({"f0.txt": "edited before the sync"})

    swfs_repo.sync("main@1", force=True)
    after_sync = swfs_repo.status(scan=True)
    # The sync rewrites only the file the two revisions differ in, so the local edit to another
    # stays a change, and the rewritten file is a change from nothing.
    assert "M f0.txt" in after_sync
    assert "f1.txt" not in after_sync

    swfs_repo.write_files({"f2.txt": "edited after the sync"})
    after_edit = swfs_repo.status(scan=True)
    assert "M f2.txt" in after_edit
    assert "M f0.txt" in after_edit
    assert "f1.txt" not in after_edit


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_stage_after_a_scan_stages_what_the_scan_found(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first",
        {
            "f0.txt": "contents 0",
            "f1.txt": "contents 1",
            "gone/inside.txt": "inside",
            "kept.txt": "kept",
        },
    )
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.write_files(
        {
            "f0.txt": "edited",
            "added.txt": "new",
            "newdir/inside.txt": "new in a new directory",
        }
    )
    swfs_repo.remove_file("f1.txt")
    swfs_repo.remove_file("gone/inside.txt")
    swfs_repo.remove_dir("gone")
    swfs_repo.status(scan=True)

    # Without --scan, stage takes the paths the scan marked, and asks the provider about each.
    swfs_repo.stage(".")
    staged = swfs_repo.status()

    assert "M f0.txt" in staged
    assert "D f1.txt" in staged
    assert "A added.txt" in staged
    assert "A newdir/inside.txt" in staged
    assert "D gone/" in staged
    assert "kept.txt" not in staged


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_stage_with_a_scan_stages_what_it_finds(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first",
        {"f0.txt": "contents 0", "f1.txt": "contents 1", "kept.txt": "kept"},
    )
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.write_files({"f0.txt": "edited", "added.txt": "new"})
    swfs_repo.remove_file("f1.txt")
    swfs_repo.stage(".", scan=True)
    staged = swfs_repo.status()

    assert "M f0.txt" in staged
    assert "D f1.txt" in staged
    assert "A added.txt" in staged
    assert "kept.txt" not in staged


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_staging_a_file_it_cannot_describe_never_stages_its_deletion(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {"f0.txt": "contents 0", "f1.txt": "contents 1"}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)
    swfs_repo.write_files({"f0.txt": "edited"})
    swfs_repo.status(scan=True)

    # SWFS froze f0.txt for the scan above and not for this operation, so it cannot be described.
    # That may fail the stage; it must not stage the file as deleted.
    try:
        swfs_repo.stage(".")
    except Exception:  # noqa: BLE001 - the failure is the expected outcome
        pass
    staged = swfs_repo.status()

    assert "D f0.txt" not in staged, staged


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_commands_never_report_a_path_they_cannot_describe_as_absent(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"f0.txt": "contents 0"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)
    swfs_repo.write_files({"new.txt": "added"})
    swfs_repo.status(scan=True)

    # new.txt was added before the last thaw, so SWFS froze nothing to describe it from. A command
    # may fail for that; one that succeeds must not have dropped the path or called it deleted.
    for command in (
        lambda: swfs_repo.status(path="new.txt", scan=True),
        lambda: swfs_repo.file_info("new.txt"),
    ):
        try:
            output = str(command())
        except Exception:  # noqa: BLE001 - the failure is an acceptable outcome
            continue
        assert "new.txt" in output, output


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_sync_after_a_scan_carries_the_dirty_paths_forward(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    files = {f"f{i}.txt": f"contents {i}" for i in range(4)}
    original_repo.write_commit_push("first", files)
    original_repo.write_commit_push("second", {"f1.txt": "second contents"})
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.write_files({"f0.txt": "edited before the sync", "added.txt": "new"})
    # The scan leaves dirty paths in the staged state, which the sync then rebases onto the
    # revision it moves to, from inside its own filesystem operation.
    assert "M f0.txt" in swfs_repo.status(scan=True)

    swfs_repo.sync("main@1", force=True)
    after_sync = swfs_repo.status(scan=True)

    assert "M f0.txt" in after_sync
    assert "A added.txt" in after_sync
    assert "f1.txt" not in after_sync


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_sync_keeping_a_local_deletion_leaves_the_file_deleted(
    new_lore_repo, background_lore_service
):
    """A sync that keeps a file the user deleted leaves it deleted. Moving to the revision empties
    the modified-file record, which then rebuilds from SWFS's list before the operation keeps what
    it left unwritten: SWFS freezes no deleted file, so an empty record took the deletion for no
    change and wrote the old content back."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"a.txt": "a1", "b.txt": "b1"})
    original_repo.write_commit_push("second", {"a.txt": "a2", "b.txt": "b2"})
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.remove_file("b.txt")
    assert "D b.txt" in swfs_repo.status(scan=True)

    swfs_repo.sync("main@1", forward_changes=True)

    assert not swfs_repo.file_exists("b.txt")
    after_sync = swfs_repo.status(scan=True)
    assert "D b.txt" in after_sync
    assert "M b.txt" not in after_sync
    with swfs_repo.open_file("a.txt", "r") as file:
        assert file.read() == "a1"


# An operation logs how many times SWFS called back into Lore, at debug level when it finishes.
_CALLBACKS = re.compile(r"read_file callbacks \+(\d+), directory evaluations \+(\d+)")


def _callbacks(output) -> tuple[int, int]:
    """The `read_file` and directory-evaluation callbacks SWFS made during a command, summed over
    the operations it ran, as the command's debug output reports them."""
    reads = evaluations = 0
    for match in _CALLBACKS.finditer(str(output)):
        reads += int(match.group(1))
        evaluations += int(match.group(2))
    return reads, evaluations


# And how many files SWFS held changed when it began, as it froze them.
_FROZEN = re.compile(r"(\d+) files frozen")


def _frozen_files(output) -> list[int]:
    """How many files SWFS held changed as each operation of a command began, in the order the
    operations ran, as the command's debug output reports them."""
    return [int(match.group(1)) for match in _FROZEN.finditer(str(output))]


# And how many paths its thaw described to SWFS.
_DESCRIBED = re.compile(r"(\d+) paths described")


def _described(output) -> list[int]:
    """How many paths each operation of a command described to SWFS when it thawed, in the order
    the operations ran, as the command's debug output reports them."""
    return [int(match.group(1)) for match in _DESCRIBED.finditer(str(output))]


# How many files a scan read to compare, as its summary reports.
_HASH_CHECKS = re.compile(r"(\d+) hash checks")


def _hash_checks(output) -> int:
    """How many files the scans of a command read to compare, as its debug output reports."""
    return sum(int(match.group(1)) for match in _HASH_CHECKS.finditer(str(output)))


# What an operation logs where it rebuilds the modified-file record from SWFS's list.
_REBUILT = "SWFS rebuilds the modified-file record"

# A build without test-util has no fallback, and reports none.
_ANSWERED = re.compile(
    r"(\d+) reads answered without the mount(?:, (\d+) fell back to the mount)?"
)


def _answered(output) -> tuple[int, int]:
    """The reads a command's operations answered without the mount, and the reads that fell back
    to it, summed over the operations it ran."""
    answered = fell_back = 0
    for match in _ANSWERED.finditer(str(output)):
        answered += int(match.group(1))
        fell_back += int(match.group(2) or 0)
    return answered, fell_back


def _hash_of(output) -> str:
    return re.search(r"[0-9a-f]{16,}", str(output)).group(0)


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_hashing_an_unmodified_file_reads_it_from_the_store_not_the_mount(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {f"f{i}.txt": f"contents of file {i}" for i in range(4)}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    hashed = swfs_repo.file_hash("f1.txt", level="debug")

    reads, _ = _callbacks(hashed)
    assert reads == 0, f"SWFS fetched content {reads} times: {hashed}"
    assert _hash_of(hashed) == _hash_of(original_repo.file_hash("f1.txt"))


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_scan_stage_and_commit_make_swfs_fetch_nothing_for_unchanged_files(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {f"f{i}.txt": f"contents of file {i}" for i in range(6)}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    swfs_repo.write_files({"f0.txt": "edited"})
    scanned = swfs_repo.status(scan=True, level="debug")
    swfs_repo.stage(".")
    committed = swfs_repo.commit(message="edit one file", level="debug")

    for command, output in (("status --scan", scanned), ("commit", committed)):
        reads, _ = _callbacks(output)
        assert reads == 0, f"{command} made SWFS fetch {reads} files: {output}"
    assert "M f0.txt" in scanned


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_commit_brings_the_record_up_to_date_rather_than_rebuilding_it(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {f"f{i}.txt": f"contents of file {i}" for i in range(4)}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    swfs_repo.write_files({"f0.txt": "committed edit", "f1.txt": "edit left alone"})
    swfs_repo.stage("f0.txt")
    swfs_repo.commit(message="commit one edit")
    scanned = swfs_repo.status(scan=True, level="debug")

    # The commit's thaw took f0.txt off SWFS's list, and the record with it, so the scan after it
    # answers from the record as it stands.
    assert _REBUILT not in str(scanned), scanned
    assert "M f1.txt" in scanned, scanned
    assert "M f0.txt" not in scanned, scanned


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_the_users_own_ignore_rules_decide_what_a_scan_reports(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {".loreignore": "*.tmp\n", "f0.txt": "contents"}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    # As a plain instance's scan reads the rules from disk, the user's edit decides: `*.tmp` is
    # no longer ignored, and `*.log` is.
    swfs_repo.write_files(
        {".loreignore": "*.log\n", "notes.tmp": "kept", "trace.log": "ignored"}
    )
    scanned = swfs_repo.status(scan=True)

    assert "notes.tmp" in scanned, scanned
    assert "trace.log" not in scanned, scanned


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_sync_keeps_the_users_file_where_the_revision_adds_a_directory(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"other.txt": "other\n"})
    swfs_repo = _swfs_clone(original_repo)
    original_repo.write_commit_push("x becomes a directory", {"x/inner.txt": "inner\n"})

    # As on a plain instance, where no directory is made over a file, the sync fails rather than
    # replace the user's file.
    swfs_repo.write_files({"x": "user file\n"})
    with pytest.raises(LoreException):
        swfs_repo.sync()

    with open(os.path.join(swfs_repo.path, "x")) as held:
        assert held.read() == "user file\n"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_file_renamed_again_after_its_rename_is_committed_keeps_its_contents(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"a.txt": "alpha\n", "other.txt": "other\n"})
    swfs_repo = _swfs_clone(original_repo)

    os.rename(os.path.join(swfs_repo.path, "a.txt"), os.path.join(swfs_repo.path, "b.txt"))
    swfs_repo.stage(".", scan=True)
    swfs_repo.commit(message="rename a to b")

    # SWFS reads the bytes of a file renamed without changes from where it lists the file as
    # renamed from: the commit's thaw names that origin, so a later rename comes from `b.txt`,
    # which the revision holds, not from `a.txt`, which it no longer does.
    os.rename(os.path.join(swfs_repo.path, "b.txt"), os.path.join(swfs_repo.path, "c.txt"))
    with open(os.path.join(swfs_repo.path, "c.txt")) as held:
        assert held.read() == "alpha\n"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_commit_of_a_deletion_keeps_a_file_made_at_the_path_since_it_was_staged(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {"a.txt": "a\n", "d/x.txt": "x\n", "other.txt": "other\n"}
    )
    swfs_repo = _swfs_clone(original_repo)

    os.remove(os.path.join(swfs_repo.path, "a.txt"))
    os.remove(os.path.join(swfs_repo.path, "d", "x.txt"))
    os.rmdir(os.path.join(swfs_repo.path, "d"))
    swfs_repo.stage(".", scan=True)
    # As on a plain instance, a file made where a staged deletion stands is the user's, which
    # the commit of the deletion leaves as an addition.
    swfs_repo.write_files({"a.txt": "made again\n"})
    swfs_repo.commit(message="delete a and d")

    with open(os.path.join(swfs_repo.path, "a.txt")) as held:
        assert held.read() == "made again\n"
    # The deleted directory leaves nothing behind, not even an empty file.
    assert not os.path.lexists(os.path.join(swfs_repo.path, "d"))
    scanned = swfs_repo.status(scan=True)
    assert "a.txt" in scanned, scanned


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_stage_with_a_scan_and_commit_make_swfs_fetch_nothing_for_unchanged_files(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {f"f{i}.txt": f"contents of file {i}" for i in range(6)}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    swfs_repo.write_files({"f0.txt": "edited"})
    staged = swfs_repo.stage(".", scan=True, level="debug")
    committed = swfs_repo.commit(message="edit one file", level="debug")

    for command, output in (("stage --scan", staged), ("commit", committed)):
        reads, _ = _callbacks(output)
        assert reads == 0, f"{command} made SWFS fetch {reads} files: {output}"
    # The commit holds the edit, not the removal of the file.
    changes = swfs_repo.revision_diff("main@1", target="main@2")
    assert "M f0.txt" in changes, changes
    assert "D f0.txt" not in changes, changes


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_hashing_a_file_changed_since_the_last_thaw_reads_the_frozen_snapshot(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"f0.txt": "original", "f1.txt": "other"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    # Written after the last thaw, so SWFS freezes it with the next operation.
    swfs_repo.write_files({"f0.txt": "changed since the last thaw"})
    hashed = swfs_repo.file_hash("f0.txt", level="debug")

    answered, fell_back = _answered(hashed)
    assert fell_back == 0 and answered > 0, hashed
    expected = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    expected.write_commit_push(
        "same content", {"f0.txt": "changed since the last thaw"}
    )
    assert _hash_of(hashed) == _hash_of(expected.file_hash("f0.txt"))


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_hashing_a_large_changed_file_reads_it_a_window_at_a_time_from_the_frozen_snapshot(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"big.bin": b"original"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    # SWFS is read for a frozen file a range at a time, so however large the file is, Lore holds
    # no more of it at once than the window it hashes.
    large = bytes(i % 251 for i in range(48 * 1024 * 1024))
    swfs_repo.write_files({"big.bin": large})
    hashed = swfs_repo.file_hash("big.bin", level="debug")

    answered, fell_back = _answered(hashed)
    assert fell_back == 0 and answered > 0, hashed
    expected = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    expected.write_commit_push("same content", {"big.bin": large})
    assert _hash_of(hashed) == _hash_of(expected.file_hash("big.bin"))


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_hashing_an_ignored_file_reads_it_from_the_frozen_snapshot(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {".loreignore": "*.tmp\n", "f0.txt": "contents"}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    # The record drops ignored paths and the revision does not hold the file, but SWFS froze it.
    swfs_repo.write_files({"scratch.tmp": "ignored contents"})
    hashed = swfs_repo.file_hash("scratch.tmp", level="debug")

    _, fell_back = _answered(hashed)
    assert fell_back == 0, hashed
    expected = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    expected.write_commit_push("same content", {"scratch.tmp": "ignored contents"})
    assert _hash_of(hashed) == _hash_of(expected.file_hash("scratch.tmp"))


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_conflicting_merge_leaves_its_copies_and_reads_nothing_through_the_mount(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    base = ["One line\n", "Another line\n", "Third line\n"]
    original_repo.write_commit_push("base", {"text.txt": base})
    original_repo.branch_create("theirs")
    original_repo.write_commit_push("theirs", {"text.txt": base + ["Fourth line\n"]})
    original_repo.branch_switch("main")
    original_repo.write_commit_push(
        "mine", {"text.txt": base + ["Fourth conflicting line\n"]}
    )
    swfs_repo = _swfs_clone(original_repo)

    merged = swfs_repo.branch_merge("theirs", level="debug")

    for suffix in ("~mine", "~theirs", "~base"):
        assert swfs_repo.file_exists("text.txt" + suffix), suffix
    # The base and theirs copies are written from the store and read straight back, which the
    # operation answers itself rather than from the mount, where they do not exist yet.
    assert "Failed to read a side of the merge" not in merged, merged
    _, fell_back = _answered(merged)
    assert fell_back == 0, merged


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_conflicting_merge_writes_conflict_markers_to_the_file(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    base = ["One line\n", "Another line\n", "Third line\n"]
    original_repo.write_commit_push("base", {"text.txt": base})
    original_repo.branch_create("theirs")
    original_repo.write_commit_push("theirs", {"text.txt": base + ["Fourth line\n"]})
    original_repo.branch_switch("main")
    original_repo.write_commit_push(
        "mine", {"text.txt": base + ["Fourth conflicting line\n"]}
    )
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.branch_merge("theirs")

    with swfs_repo.open_file("text.txt") as merged_file:
        assert "<<<<<<<" in merged_file.read()


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_revert_result_is_read_by_the_later_operations_that_commit_it(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"text.txt": "first version\n"})
    original_repo.write_commit_push("second", {"text.txt": "second version\n"})
    reverted = original_repo.revision_history(1)[0].signature
    swfs_repo = _swfs_clone(original_repo)

    # The revert writes its result to the mount as its operation finalizes; the scan and the
    # commit each read it in a later operation, after the thaws between.
    swfs_repo.revision_revert(reverted, no_commit=True)
    assert swfs_repo.read_files()["text.txt"] == "first version\n"
    swfs_repo.status(scan=True)
    swfs_repo.commit("Revert the second version")
    swfs_repo.push()

    original_repo.sync()
    assert original_repo.read_files()["text.txt"] == "first version\n"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_clean_text_merge_keeps_the_changes_of_both_sides(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    base = ["One line\n", "Another line\n", "Third line\n"]
    original_repo.write_commit_push("base", {"text.txt": base})
    original_repo.branch_create("theirs")
    original_repo.write_commit_push(
        "theirs", {"text.txt": ["First line\n", "Another line\n", "Third line\n"]}
    )
    original_repo.branch_switch("main")
    original_repo.write_commit_push("mine", {"text.txt": base + ["Fourth line\n"]})
    swfs_repo = _swfs_clone(original_repo)

    merged = swfs_repo.branch_merge("theirs")

    assert "Merge as text failed" not in merged, merged
    with swfs_repo.open_file("text.txt") as merged_file:
        assert merged_file.read() == (
            "First line\nAnother line\nThird line\nFourth line\n"
        )
    for suffix in ("~mine", "~theirs", "~base"):
        assert not swfs_repo.file_exists("text.txt" + suffix), suffix


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_files_a_merge_writes_and_removes_in_one_operation_leave_nothing(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    base = ["One line\n", "Another line\n", "Third line\n"]
    original_repo.write_commit_push("base", {"text.txt": base})
    original_repo.branch_create("theirs")
    original_repo.write_commit_push(
        "theirs", {"text.txt": ["First line\n", "Another line\n", "Third line\n"]}
    )
    original_repo.branch_switch("main")
    original_repo.write_commit_push("mine", {"text.txt": base + ["Fourth line\n"]})
    swfs_repo = _swfs_clone(original_repo)

    # A clean text merge writes the base, theirs and mine copies, reads them to merge, and removes
    # them again, all in the one operation. Thaw is told only of what the revision holds, so it
    # describes none of them: SWFS makes an empty file of a path it is told holds nothing.
    swfs_repo.branch_merge("theirs")
    suffixes = ("~mine", "~theirs", "~base")
    assert not [s for s in suffixes if swfs_repo.file_exists("text.txt" + s)]

    # A later operation does not bring them back, or report them.
    after = swfs_repo.status(scan=True)
    assert not [s for s in suffixes if swfs_repo.file_exists("text.txt" + s)]
    assert not [s for s in suffixes if s in after], after


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_merge_with_no_base_leaves_an_empty_base_copy(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("base", {"keep.txt": "kept\n"})
    original_repo.branch_create("theirs")
    original_repo.write_commit_push("theirs", {"new.txt": "from theirs\n"})
    original_repo.branch_switch("main")
    original_repo.write_commit_push("mine", {"new.txt": "from mine\n"})
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.branch_merge("theirs")

    # Both sides added the file, so the revision holds no base for it: the base copy is empty.
    assert swfs_repo.file_exists("new.txt~base")
    with swfs_repo.open_file("new.txt~base") as base_file:
        assert base_file.read() == ""


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_stage_lists_directories_from_the_revision_and_the_changes(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first",
        {
            "d0/a.txt": "a",
            "d0/c.txt": "c",
            "d1/b.txt": "b",
            "d2/keep.txt": "keep",
            "d2/deep/inner.txt": "inner",
        },
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    swfs_repo.write_files(
        {"d0/a.txt": "a, edited", "d2/new.txt": "new", "d3/x.txt": "in a new directory"}
    )
    swfs_repo.remove_file("d1/b.txt")
    swfs_repo.move("d0/c.txt", "d0/renamed.txt")

    staged = swfs_repo.stage(".", scan=True, level="debug")
    answered, _ = _answered(staged)
    status = swfs_repo.status()

    assert answered > 0, staged
    assert "M d0/a.txt" in status
    assert "D d1/b.txt" in status
    assert "A d2/new.txt" in status
    assert "A d3/x.txt" in status
    assert "d0/renamed.txt" in status
    assert "d0/c.txt" in status
    assert "keep.txt" not in status
    assert "inner.txt" not in status


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_new_instance_is_scanned_through_the_view_without_a_walk(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first",
        {f"d{d}/f{i}.txt": f"contents {d} {i}" for d in range(3) for i in range(3)},
    )
    swfs_repo = _swfs_clone(original_repo)

    # Nothing has written to an instance just created, so its record holds every change there is
    # and the first scan has no walk to make.
    first = swfs_repo.status(scan=True, level="debug")
    _, fell_back = _answered(first)
    assert fell_back == 0, first
    assert _callbacks(first) == (0, 0), first

    swfs_repo.write_files({"d1/f1.txt": "edited"})
    scanned = swfs_repo.status(scan=True, level="debug")

    answered, fell_back = _answered(scanned)
    assert "M d1/f1.txt" in scanned, scanned
    assert fell_back == 0 and answered > 0, scanned
    assert _callbacks(scanned) == (0, 0), scanned


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_the_first_scan_after_a_restart_answers_from_swfs_own_list(
    new_lore_repo, lore_service_runner
):
    lore_service_runner.start()
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {f"d{d}/f0.txt": f"contents {d}" for d in range(3)}
    )
    swfs_repo = _swfs_clone(original_repo)
    # A deleted file is the one change a scan reads no content for, so SWFS's lack of a frozen
    # file for an earlier change does not reach it.
    swfs_repo.remove_file("d1/f0.txt")
    swfs_repo.status(scan=True)

    lore_service_runner.terminate()
    lore_service_runner.start()
    first = swfs_repo.status(scan=True, level="debug")

    # SWFS keeps its list of what it holds modified across a restart, so the first operation
    # rebuilds the record from it rather than walking the tree to learn what differs.
    _, fell_back = _answered(first)
    assert fell_back == 0, first
    assert _callbacks(first) == (0, 0), first
    assert "D d1/f0.txt" in first, first


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_rename_from_before_a_restart_is_still_found(
    new_lore_repo, lore_service_runner
):
    """SWFS lists a renamed file by its new name and its origin as a deletion with the same
    `file_index`, so after a restart the record pairs them and finds both sides of the rename
    without walking the tree."""
    lore_service_runner.start()
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"f0.txt": "contents", "f1.txt": "other"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.move("f0.txt", "renamed.txt")
    swfs_repo.status(scan=True)

    lore_service_runner.terminate()
    lore_service_runner.start()
    after = swfs_repo.status(scan=True)

    assert (
        "D f0.txt" in after or "R f0.txt" in after or "f0.txt -> renamed.txt" in after
    ), after
    assert "renamed.txt" in after, after
    assert "f1.txt" not in after, after


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_file_changed_in_an_earlier_operation_is_still_read_from_the_snapshot(
    new_lore_repo, background_lore_service
):
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"f0.txt": "original", "f1.txt": "other"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.write_files({"f0.txt": "changed before an earlier operation"})
    swfs_repo.status(scan=True)

    hashed = swfs_repo.file_hash("f0.txt", level="debug")

    answered, fell_back = _answered(hashed)
    assert fell_back == 0 and answered > 0, hashed


def _swfs_instance_with_a_committed_link(new_lore_repo):
    """An SWFS instance with a link at `lib`, committed, to a repository holding `linked.txt` and
    `other.txt`; returns the instance and the linked repository."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    linked_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("parent", {"README.txt": "parent\n"})
    linked_repo.write_commit_push(
        "one", {"linked.txt": "linked one\n", "other.txt": "other one\n"}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.link_add("lib", linked_repo.get_id(), "/")
    swfs_repo.commit(message="add the link")
    return swfs_repo, linked_repo


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_sync_moves_a_layer(new_lore_repo, background_lore_service):
    """A sync that moves a layer syncs it within the sync's own operation: SWFS refuses a
    freeze while frozen, so a layer sync opening an operation of its own failed to freeze."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    layer_repo = new_lore_repo(
        original_repo.name + "_layer",
        environment_vars=LORE_SERVICE_ENVIRONMENT.copy(),
    )
    original_repo.write_commit_push("main", {"main.txt": "main contents"})
    layer_repo.write_commit_push("layer one", {"lay/layer.txt": "layer one"})

    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.layer_add("lay", layer_repo, "lay/")
    with swfs_repo.open_file("lay/layer.txt") as layer_file:
        assert layer_file.read() == "layer one"

    layer_repo.write_commit_push("layer two", {"lay/layer.txt": "layer two"})
    swfs_repo.sync(force=True)

    with swfs_repo.open_file("lay/layer.txt") as layer_file:
        assert layer_file.read() == "layer two"
    with swfs_repo.open_file("main.txt") as main_file:
        assert main_file.read() == "main contents"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_link_add_under_a_new_directory(new_lore_repo, background_lore_service):
    """A link added under a directory the instance does not hold is served through the mount
    from the linked repository: listed and read there, with nothing below it a local change,
    either to Lore or in SWFS's own write cache (REQ-F-3)."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    linked_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("parent", {"README.txt": "parent\n"})
    linked_repo.write_commit_push(
        "linked", {"linked.txt": "linked content\n", "sub/deep.txt": "deep\n"}
    )

    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.link_add("vendor/lib", linked_repo.get_id(), "/")

    mount = os.path.join(swfs_repo.path, "vendor", "lib")
    assert sorted(os.listdir(mount)) == ["linked.txt", "sub"]
    with swfs_repo.open_file("vendor/lib/linked.txt") as linked_file:
        assert linked_file.read() == "linked content\n"
    with swfs_repo.open_file("vendor/lib/sub/deep.txt") as deep_file:
        assert deep_file.read() == "deep\n"
    assert unstaged_entries(swfs_repo) == []
    staged_below_the_link = [
        entry["path"]
        for entry in parse_status_json(swfs_repo.status(json=True))
        if entry["path"].startswith("vendor/lib/")
    ]
    assert staged_below_the_link == []
    # Written onto the mount, the linked files would be SWFS's own changes, which it freezes.
    assert _frozen_files(swfs_repo.status(scan=True, level="debug")) == [0]


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_sync_turns_a_file_into_a_link(new_lore_repo, background_lore_service):
    """A file that becomes a link in the revision a sync moves to is served as the linked
    directory, which takes one thaw to describe: SWFS replaces the node and its type."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    linked_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "parent", {"vendor": "a file\n", "keep.txt": "keep\n"}
    )
    linked_repo.write_commit_push("linked", {"linked.txt": "linked content\n"})
    swfs_repo = _swfs_clone(original_repo)
    with swfs_repo.open_file("vendor") as vendor_file:
        assert vendor_file.read() == "a file\n"

    original_repo.remove_file("vendor")
    original_repo.stage("vendor")
    original_repo.link_add("vendor", linked_repo.get_id(), "/")
    original_repo.commit(message="replace the file with a link")
    original_repo.push()

    swfs_repo.sync()

    mount = os.path.join(swfs_repo.path, "vendor")
    assert os.path.isdir(mount)
    assert os.listdir(mount) == ["linked.txt"]
    with swfs_repo.open_file("vendor/linked.txt") as linked_file:
        assert linked_file.read() == "linked content\n"
    assert unstaged_entries(swfs_repo) == []


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_link_update_and_its_unstage_serve_each_pin(
    new_lore_repo, background_lore_service
):
    """A repinned link reads the new pin's content through the mount, and unstaging the repin
    reads the old pin's again, neither leaving a linked file a local change in SWFS."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    linked_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("parent", {"README.txt": "parent\n"})
    linked_repo.write_commit_push("one", {"linked.txt": "pin one\n"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.link_add("vendor/lib", linked_repo.get_id(), "/")
    swfs_repo.commit(message="add the link")
    linked_repo.write_commit_push("two", {"linked.txt": "pin two\n"})

    def linked_content():
        with swfs_repo.open_file("vendor/lib/linked.txt") as linked_file:
            return linked_file.read()

    swfs_repo.link_update("vendor/lib")
    assert linked_content() == "pin two\n"
    assert _frozen_files(swfs_repo.status(scan=True, level="debug")) == [0]

    swfs_repo.unstage("vendor/lib")
    assert linked_content() == "pin one\n"
    assert _frozen_files(swfs_repo.status(scan=True, level="debug")) == [0]


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_link_remove_takes_the_link_off_the_mount(
    new_lore_repo, background_lore_service
):
    """After `link remove` a top-level link's path is gone from the mount, as from a plain working
    tree, rather than still serving the linked files."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    linked_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("parent", {"README.txt": "parent\n"})
    linked_repo.write_commit_push("linked", {"linked.txt": "linked content\n"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.link_add("lib", linked_repo.get_id(), "/")
    swfs_repo.commit(message="add the link")

    swfs_repo.link_remove("lib")

    assert not os.path.exists(os.path.join(swfs_repo.path, "lib", "linked.txt"))
    assert sorted(os.listdir(swfs_repo.path)) == ["README.txt"]
    with swfs_repo.open_file("README.txt") as readme:
        assert readme.read() == "parent\n"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_layer_add_serves_the_layer_and_layer_remove_takes_it_off(
    new_lore_repo, background_lore_service
):
    """A layer added is listed and read through the mount from the layer's repository with none
    of its files a local change in SWFS, and after `layer remove` its files are gone."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    layer_repo = new_lore_repo(
        original_repo.name + "_layer",
        environment_vars=LORE_SERVICE_ENVIRONMENT.copy(),
    )
    original_repo.write_commit_push("main", {"main.txt": "main contents"})
    layer_repo.write_commit_push(
        "layer", {"lay/layer.txt": "layer contents", "lay/sub/deep.txt": "deep"}
    )
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.layer_add("lay", layer_repo, "lay/")

    assert sorted(os.listdir(os.path.join(swfs_repo.path, "lay"))) == [
        "layer.txt",
        "sub",
    ]
    with swfs_repo.open_file("lay/layer.txt") as layer_file:
        assert layer_file.read() == "layer contents"
    assert _frozen_files(swfs_repo.status(scan=True, level="debug")) == [0]

    swfs_repo.layer_remove("lay", layer_repo)

    assert not os.path.exists(os.path.join(swfs_repo.path, "lay", "layer.txt"))
    assert not os.path.exists(os.path.join(swfs_repo.path, "lay", "sub", "deep.txt"))
    with swfs_repo.open_file("main.txt") as main_file:
        assert main_file.read() == "main contents"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_link_repin_keeps_the_users_changes_below_the_link(
    new_lore_repo, background_lore_service
):
    """A repin described by the link's mount path refreshes what the new pin changed and keeps
    the user's edit and the user's own file below the link (REQ-F-5, M7, M10)."""
    swfs_repo, linked_repo = _swfs_instance_with_a_committed_link(new_lore_repo)
    swfs_repo.write_files({"lib/linked.txt": "edited\n", "lib/user.txt": "mine\n"})
    linked_repo.write_commit_push("two", {"other.txt": "other two\n"})

    swfs_repo.link_update("lib")

    def read(path):
        with swfs_repo.open_file(path) as file:
            return file.read()

    assert read("lib/other.txt") == "other two\n"
    assert read("lib/linked.txt") == "edited\n"
    assert read("lib/user.txt") == "mine\n"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_repin_over_many_files_describes_only_the_link_path(
    new_lore_repo, background_lore_service
):
    """A repin is described to SWFS by the link's mount path, one entry however many files the
    new pin changes, rather than one per file (REQ-NF-5)."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    linked_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("parent", {"README.txt": "parent\n"})
    many = 20
    linked_repo.write_commit_push(
        "one", {f"d/f{i}.txt": f"one {i}\n" for i in range(many)}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.link_add("lib", linked_repo.get_id(), "/")
    swfs_repo.commit(message="add the link")
    linked_repo.write_commit_push(
        "two", {f"d/f{i}.txt": f"two {i}\n" for i in range(many)}
    )

    updated = swfs_repo.link_update("lib", level="debug")

    assert _described(updated) == [1]
    for i in (0, many - 1):
        with swfs_repo.open_file(f"lib/d/f{i}.txt") as file:
            assert file.read() == f"two {i}\n"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_forced_link_remove_leaves_none_of_the_users_files(
    new_lore_repo, background_lore_service
):
    """`link remove --force` over a user edit and a user file leaves nothing at the link's path,
    as a plain working tree's removal does (REQ-F-6)."""
    swfs_repo, _linked_repo = _swfs_instance_with_a_committed_link(new_lore_repo)
    swfs_repo.write_files({"lib/linked.txt": "edited\n", "lib/user.txt": "mine\n"})

    swfs_repo.link_remove("lib", force=True)

    assert not os.path.exists(os.path.join(swfs_repo.path, "lib"))
    assert sorted(os.listdir(swfs_repo.path)) == ["README.txt"]


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
@pytest.mark.parametrize("purge", [False, True])
def test_swfs_layer_remove_keeps_an_untracked_file_unless_purged(
    new_lore_repo, background_lore_service, purge
):
    """`layer remove` takes the layer's files and keeps the user's untracked file, and
    `layer remove --purge` takes that too (REQ-F-6, M11)."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    layer_repo = new_lore_repo(
        original_repo.name + "_layer",
        environment_vars=LORE_SERVICE_ENVIRONMENT.copy(),
    )
    original_repo.write_commit_push("main", {"main.txt": "main contents"})
    layer_repo.write_commit_push("layer", {"lay/layer.txt": "layer contents"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.layer_add("lay", layer_repo, "lay/")
    swfs_repo.write_files({"lay/untracked.txt": "mine"})

    swfs_repo.layer_remove("lay", layer_repo, purge=purge)

    assert not os.path.exists(os.path.join(swfs_repo.path, "lay", "layer.txt"))
    assert os.path.exists(os.path.join(swfs_repo.path, "lay", "untracked.txt")) == (
        not purge
    )


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_scan_with_a_link_is_answered_from_the_record(
    new_lore_repo, background_lore_service
):
    """A scan of an instance holding a link over many directories is answered from the
    modified-file record, as it is with no link: a change outside the link reads nothing of the
    link's tree, which a walk reads directory by directory through the view, and a change inside
    it is found (REQ-F-9)."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    linked_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("parent", {"README.txt": "parent\n"})
    many = 30
    linked_repo.write_commit_push(
        "linked", {f"d{i}/f{i}.txt": f"linked {i}\n" for i in range(many)}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.link_add("lib", linked_repo.get_id(), "/")
    swfs_repo.commit(message="add the link")

    swfs_repo.write_files({"README.txt": "edited\n"})
    scanned = swfs_repo.status(scan=True, level="debug")

    answered, fell_back = _answered(scanned)
    assert fell_back == 0, scanned
    assert answered < many // 3, (
        f"the scan read {answered} paths, as a walk of the link's {many} directories does"
    )
    assert [entry["path"] for entry in unstaged_entries(swfs_repo)] == ["README.txt"]

    swfs_repo.write_files({"lib/d0/f0.txt": "edited\n"})
    assert [entry["path"] for entry in unstaged_entries(swfs_repo)] == [
        "README.txt",
        "lib/d0/f0.txt",
    ]


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_restarted_service_serves_the_links_and_layers_it_did(
    new_lore_repo, lore_service_runner
):
    """A service restarted over an instance with a link and a layer serves both as before, and
    the first scan after the restart reports nothing, answered by the record's rebuild rather
    than by reads through the mount (REQ-F-8)."""
    lore_service_runner.start()
    env = LORE_SERVICE_ENVIRONMENT.copy()
    original_repo = new_lore_repo(environment_vars=env)
    linked_repo = new_lore_repo(environment_vars=env)
    layer_repo = new_lore_repo(original_repo.name + "_layer", environment_vars=env)
    original_repo.write_commit_push("parent", {"README.txt": "parent\n"})
    linked_repo.write_commit_push("linked", {"linked.txt": "linked content\n"})
    layer_repo.write_commit_push("layer", {"lay/layer.txt": "layer content\n"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.link_add("lib", linked_repo.get_id(), "/")
    swfs_repo.commit(message="add the link")
    swfs_repo.layer_add("lay", layer_repo, "lay/")

    lore_service_runner.terminate()
    lore_service_runner.start()

    assert sorted(os.listdir(os.path.join(swfs_repo.path, "lib"))) == ["linked.txt"]
    assert sorted(os.listdir(os.path.join(swfs_repo.path, "lay"))) == ["layer.txt"]
    with swfs_repo.open_file("lib/linked.txt") as linked_file:
        assert linked_file.read() == "linked content\n"
    with swfs_repo.open_file("lay/layer.txt") as layer_file:
        assert layer_file.read() == "layer content\n"
    scanned = swfs_repo.status(scan=True, level="debug")
    _, fell_back = _answered(scanned)
    assert fell_back == 0, scanned
    assert unstaged_entries(swfs_repo) == []


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_commands_in_succession_each_freeze_and_thaw(
    new_lore_repo, background_lore_service
):
    """Each command on an SWFS instance freezes the mount and thaws it, and the next starts at
    once. SWFS returns from a thaw before it has finished, so a command that follows another
    must wait for it rather than fail to freeze."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"changed.txt": "first contents"})
    swfs_repo = _swfs_clone(original_repo)

    for _ in range(20):
        swfs_repo.status(scan=True)
        swfs_repo.file_diff("changed.txt")

    _repositories_match_files(original_repo, swfs_repo, ["changed.txt"])


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_file_written_back_to_what_the_revision_holds_leaves_swfs_list(
    new_lore_repo, background_lore_service
):
    """SWFS keeps a file the user wrote back to what the revision holds listed as modified, so the
    scan that finds it unchanged names it in its thaw, and the scans after it compare it no more."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"f0.txt": "original", "f1.txt": "other"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.write_files({"f0.txt": "edited"}, flush_writes=True)
    assert "M f0.txt" in swfs_repo.status(scan=True)

    swfs_repo.write_files({"f0.txt": "original"}, flush_writes=True)
    written_back = swfs_repo.status(scan=True, level="debug")
    after = swfs_repo.status(scan=True, level="debug")

    assert "M f0.txt" not in written_back, written_back
    assert _hash_checks(written_back) == 1, written_back
    assert _frozen_files(after) and all(count == 0 for count in _frozen_files(after)), after
    assert _hash_checks(after) == 0, after
    assert "M f0.txt" not in after, after
    _repositories_match_files(original_repo, swfs_repo, ["f0.txt", "f1.txt"])


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_renamed_file_keeps_its_content_when_a_new_file_at_its_origin_is_committed(
    new_lore_repo, background_lore_service
):
    """SWFS reads a renamed file it holds no bytes of from the source at the path it came from,
    and stops listing that origin once a new file is made there. Committing the new file changes
    what the source holds at that path, so the renamed file must hold bytes of its own by then.
    Nothing is read before the commit: SWFS's driver keeps what a read fetched for a while, which
    would hide the source changing."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"a.txt": "the original contents of a"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.move("a.txt", "b.txt")
    swfs_repo.write_files({"a.txt": "new"}, flush_writes=True)

    swfs_repo.stage("a.txt")
    swfs_repo.commit(message="add the new a.txt")

    with swfs_repo.open_file("b.txt", "r") as renamed:
        assert renamed.read() == "the original contents of a"
    with swfs_repo.open_file("a.txt", "r") as new:
        assert new.read() == "new"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_reverting_the_add_of_a_file_written_through_the_mount_removes_it(
    new_lore_repo, background_lore_service
):
    """A revert removes the file in its own operation and commits the removal in the next, where
    SWFS holds no deletion of the file for the commit to clear: an entry naming the path would
    leave an empty file behind (ADR-00032)."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"a.txt": "a"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.write_files({"added.txt": "added"}, flush_writes=True)
    swfs_repo.stage("added.txt")
    swfs_repo.commit(message="add added.txt")

    swfs_repo.revision_revert(revision=swfs_repo.history()[-1].signature, message="revert the add")

    assert not swfs_repo.file_exists("added.txt")
    with swfs_repo.open_file("a.txt", "r") as kept:
        assert kept.read() == "a"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_committed_directory_deletion_lets_the_directory_come_back(
    new_lore_repo, background_lore_service
):
    """A directory deleted whole is committed as one node, its files not walked. The commit's thaw
    names the directory and each deletion SWFS lists below it: one left listed reads a later
    thaw's entry for the path, which describes the file again, as the deletion to commit, and the
    directory never comes back."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {"Dir/a.txt": "a", "Dir/Sub/B.txt": "b", "kept.txt": "k"}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.branch_create("delete-dir")
    swfs_repo.rmtree("Dir")
    swfs_repo.stage(".", scan=True)
    swfs_repo.commit(message="delete dir")
    assert not swfs_repo.path_exists("Dir")

    swfs_repo.branch_switch("main")

    with swfs_repo.open_file("Dir/a.txt", "r") as a:
        assert a.read() == "a"
    with swfs_repo.open_file("Dir/Sub/B.txt", "r") as b:
        assert b.read() == "b"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_file_deleted_after_it_was_staged_comes_back(
    new_lore_repo, background_lore_service
):
    """A commit removes a staged file the working tree no longer holds, and its thaw names the
    deletion SWFS lists there as it does one staged as deleted: one left listed keeps the file
    off the mount when a later operation restores it."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"a.txt": "a", "kept.txt": "k"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.branch_create("delete-file")
    swfs_repo.write_files({"a.txt": "changed"}, flush_writes=True)
    swfs_repo.stage("a.txt")
    swfs_repo.remove_file("a.txt")
    swfs_repo.commit(message="delete a staged file")
    assert not swfs_repo.file_exists("a.txt")

    swfs_repo.branch_switch("main")

    with swfs_repo.open_file("a.txt", "r") as a:
        assert a.read() == "a"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_directory_deleted_after_a_file_in_it_was_staged_comes_back(
    new_lore_repo, background_lore_service
):
    """A commit removes a staged directory the working tree no longer holds, with all it held,
    where everything in it was staged, and its thaw names the directory and each deletion SWFS
    lists below it."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {"Dir/a.txt": "a", "Dir/Sub/B.txt": "b", "kept.txt": "k"}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.branch_create("delete-dir")
    swfs_repo.write_files({"Dir/a.txt": "changed", "Dir/Sub/B.txt": "changed"}, flush_writes=True)
    swfs_repo.stage(".", scan=True)
    swfs_repo.rmtree("Dir")
    swfs_repo.commit(message="delete a staged directory")
    assert not swfs_repo.path_exists("Dir")

    swfs_repo.branch_switch("main")

    with swfs_repo.open_file("Dir/a.txt", "r") as a:
        assert a.read() == "a"
    with swfs_repo.open_file("Dir/Sub/B.txt", "r") as b:
        assert b.read() == "b"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_file_renamed_after_it_was_staged_keeps_its_contents(
    new_lore_repo, background_lore_service
):
    """A commit removes a staged file the user renamed since, and names the deletion SWFS lists
    at its path, which is where SWFS lists the renamed file as coming from: the renamed file
    holds bytes of its own, and keeps them."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"a.txt": "a", "kept.txt": "k"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.branch_create("rename-file")
    swfs_repo.write_files({"a.txt": "changed"}, flush_writes=True)
    swfs_repo.stage("a.txt")
    os.rename(os.path.join(swfs_repo.path, "a.txt"), os.path.join(swfs_repo.path, "b.txt"))
    swfs_repo.commit(message="delete a staged file renamed since")

    assert not swfs_repo.file_exists("a.txt")
    with swfs_repo.open_file("b.txt", "r") as b:
        assert b.read() == "changed"
    scanned = parse_status_json(swfs_repo.status(json=True, scan=True))
    assert [(e["path"].replace("\\", "/"), e["action"]) for e in scanned] == [("b.txt", "add")]

    swfs_repo.branch_switch("main")

    with swfs_repo.open_file("a.txt", "r") as a:
        assert a.read() == "a"
    with swfs_repo.open_file("b.txt", "r") as b:
        assert b.read() == "changed"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_staged_file_deleted_with_its_directory_leaves_the_rest_deleted(
    new_lore_repo, background_lore_service
):
    """A commit removes a staged file the user deleted with its directory, and leaves the
    directory, which holds a file that was not staged: the thaw names the file alone, and a
    scan still finds the rest of the directory deleted."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push(
        "first", {"Dir/a.txt": "a", "Dir/kept.txt": "k", "top.txt": "t"}
    )
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.write_files({"Dir/a.txt": "changed"}, flush_writes=True)
    swfs_repo.stage("Dir/a.txt")
    swfs_repo.rmtree("Dir")
    swfs_repo.commit(message="delete a staged file")

    assert not swfs_repo.path_exists("Dir")
    scanned = parse_status_json(swfs_repo.status(json=True, scan=True))
    assert sorted(e["path"].replace("\\", "/") for e in scanned if e["action"] == "delete") == [
        "Dir",
        "Dir/kept.txt",
    ]


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_directory_moved_back_after_its_move_was_staged_stays_as_committed(
    new_lore_repo, background_lore_service
):
    """A commit leaves out the staged move of a directory the user moved back, and the
    directory reads as committed, with nothing left at the path it was moved to."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"Dir/a.txt": "a", "top.txt": "t"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.branch_create("move-back")
    swfs_repo.move("Dir", "Moved")
    swfs_repo.write_files({"top.txt": "changed"}, flush_writes=True)
    swfs_repo.stage_move("Dir", "Moved")
    swfs_repo.stage("top.txt")
    swfs_repo.move("Moved", "Dir")
    swfs_repo.commit(message="change top")

    assert not swfs_repo.path_exists("Moved")
    with swfs_repo.open_file("Dir/a.txt", "r") as a:
        assert a.read() == "a"
    assert parse_status_json(swfs_repo.status(json=True, scan=True)) == []

    swfs_repo.branch_switch("main")

    assert not swfs_repo.path_exists("Moved")
    with swfs_repo.open_file("Dir/a.txt", "r") as a:
        assert a.read() == "a"


# What an operation logs for each renamed file it gives bytes of its own (`renamed_at_risk`).
_REWRITTEN = "bytes of its own"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_an_atomic_save_rewrites_nothing_through_the_mount(
    new_lore_repo, background_lore_service
):
    """An editor saves by writing a temporary file and renaming it over the file it saves. The
    saved file holds bytes of its own, so the next temporary file made where the first one was
    puts nothing at risk, and no operation rewrites it."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"f0.txt": "original", "f1.txt": "other"})
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.status(scan=True)

    swfs_repo.write_files({"f0.txt.tmp": "first save"}, flush_writes=True)
    os.replace(os.path.join(swfs_repo.path, "f0.txt.tmp"), os.path.join(swfs_repo.path, "f0.txt"))
    swfs_repo.write_files({"f0.txt.tmp": "second save"}, flush_writes=True)
    scanned = swfs_repo.status(scan=True, level="debug")
    after = swfs_repo.status(scan=True, level="debug")

    assert _REWRITTEN not in str(scanned) and _REWRITTEN not in str(after), (scanned, after)
    with swfs_repo.open_file("f0.txt", "r") as saved:
        assert saved.read() == "first save"


def _dependency_synced(new_lore_repo) -> tuple[Lore, Lore]:
    """A plain clone and an SWFS clone of a repository in which `src/main.rs` depends on
    `src/lib.rs`, behind a revision that changes all four files of the first, removes `src/old.rs`
    and adds `src/new.rs`."""
    source = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    source.write_commit_push(
        "first",
        {
            "src/main.rs": "main v1\n",
            "src/lib.rs": "lib v1\n",
            "src/util.rs": "util v1\n",
            "src/old.rs": "old v1\n",
        },
    )
    source.file_dependency_add("src/main.rs", ["src/lib.rs"], offline=True)
    source.write_commit_push("dependencies", {".deps_marker": "deps\n"})
    plain = source.clone()
    swfs = _swfs_clone(source)

    source.remove_file("src/old.rs")
    source.stage(".", scan=True)
    source.write_commit_push(
        "second",
        {
            "src/main.rs": "main v2\n",
            "src/lib.rs": "lib v2\n",
            "src/util.rs": "util v2\n",
            "src/new.rs": "new v2\n",
        },
    )
    return plain, swfs


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_dependency_sync_is_refused_and_leaves_the_working_tree_as_it_was(
    new_lore_repo, background_lore_service
):
    """A sync restricted to a dependency set leaves what lies outside the set as the working tree
    held it, which an SWFS mount, serving the new revision whole, does not support: the sync is
    refused, where a plain instance's goes through."""
    plain, swfs = _dependency_synced(new_lore_repo)
    before = swfs.read_files()

    plain.sync(root_files=["src/main.rs"])
    with pytest.raises(ImproperArgumentsError, match="not supported on an SWFS-backed instance"):
        swfs.sync(root_files=["src/main.rs"])

    assert plain.read_files()["src/main.rs"] == "main v2\n"
    assert swfs.read_files() == before
    # A sync of the whole revision still goes through.
    swfs.sync()
    assert swfs.read_files()["src/util.rs"] == "util v2\n"


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_merge_writing_many_conflicted_files_to_the_mount_completes(
    new_lore_repo, background_lore_service
):
    """A merge writes each conflicted file's markers and copies to the mount and reads them back
    in the same operation. Each of those syscalls can wait on an SWFS callback that reads the
    store through the syscall pool they hold threads of, so they are bounded below its size, and
    many of them at once complete."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    base = ["One line\n", "Another line\n", "Third line\n"]
    names = [f"text{index}.txt" for index in range(64)]
    original_repo.write_commit_push("base", {name: base for name in names})
    original_repo.branch_create("theirs")
    original_repo.write_commit_push(
        "theirs", {name: base + ["Fourth line\n"] for name in names}
    )
    original_repo.branch_switch("main")
    original_repo.write_commit_push(
        "mine", {name: base + ["Fourth conflicting line\n"] for name in names}
    )
    swfs_repo = _swfs_clone(original_repo)

    swfs_repo.branch_merge("theirs")

    files = swfs_repo.read_files()
    for name in names:
        assert "<<<<<<<" in files[name], name
        for suffix in ("~mine", "~theirs", "~base"):
            assert name + suffix in files, name + suffix


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_a_dependency_clone_holds_what_a_plain_one_does(
    new_lore_repo, background_lore_service
):
    """A clone restricted to a dependency set writes the set alone, and a plain instance's disk
    holds nothing else. SWFS serves the revision whole, so the clone records the rest as absent."""
    source = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    source.write_commit_push(
        "first",
        {"src/main.rs": "main\n", "src/lib.rs": "lib\n", "src/util.rs": "util\n"},
    )
    source.file_dependency_add("src/main.rs", ["src/lib.rs"], offline=True)
    source.write_commit_push("dependencies", {".deps_marker": "deps\n"})
    plain = source.clone(root_files=["src/main.rs"])
    source.shared_store_create(source.remote)
    swfs = source.clone(vfs="swfs", use_shared_store=True, root_files=["src/main.rs"])

    assert "src/util.rs" not in plain.read_files()
    assert swfs.read_files() == plain.read_files()


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_mount_fetches_what_its_store_evicted(new_lore_repo, lore_service_runner):
    """A mount the service restores, whose store has evicted what it serves, fetches it
    from the instance's remote, where it used to serve the files as zeros."""
    lore_service_runner.start()
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    file_contents = {f"f{i}.txt": os.urandom(256 * 1024) for i in range(8)}
    original_repo.write_commit_push("first", file_contents)
    swfs_repo = _swfs_clone(original_repo)
    swfs_repo.set_store_caps(max_size="100", max_capacity="100")
    lore_service_runner.terminate()
    lore_service_runner.start()

    gc_output = swfs_repo.repository_gc(json=True, offline=True)
    evicted = parse_jsonl(gc_output, "evictionEnd")
    assert evicted and evicted[0]["totalEvicted"] >= len(file_contents), gc_output

    _repositories_match_files(original_repo, swfs_repo, list(file_contents))
    swfs_repo.status()


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_resetting_a_directory_move_puts_the_directory_back(
    new_lore_repo, background_lore_service
):
    """Resetting both ends of a directory the user moved on the mount and marked moved puts the
    directory back with what it held, at any depth, and leaves the destination gone."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    files = {
        "dir/a.txt": "a content\n",
        "dir/b.txt": "b content\n",
        "dir/sub/c.txt": "c content\n",
    }
    original_repo.write_commit_push("first", files)
    swfs_repo = _swfs_clone(original_repo)
    os.rename(swfs_repo._fix_path("dir"), swfs_repo._fix_path("moved"))
    swfs_repo.dirty_move("dir", "moved")

    swfs_repo.reset(["dir", "moved"])

    for path, content in files.items():
        with swfs_repo.open_file(path, "r") as f:
            assert f.read() == content, path
    assert not swfs_repo.file_exists("moved")
    status = swfs_repo.status(scan=True)
    assert "moved" not in status and "dir" not in status, status


@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_swfs_resetting_a_file_move_puts_the_file_back(new_lore_repo, background_lore_service):
    """Resetting both ends of a file the user moved on the mount and marked moved puts the file
    back with its content, and leaves the destination gone."""
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    original_repo.write_commit_push("first", {"old.txt": "old content\n"})
    swfs_repo = _swfs_clone(original_repo)
    os.rename(swfs_repo._fix_path("old.txt"), swfs_repo._fix_path("new.txt"))
    swfs_repo.dirty_move("old.txt", "new.txt")

    swfs_repo.reset(["old.txt", "new.txt"])

    with swfs_repo.open_file("old.txt", "r") as f:
        assert f.read() == "old content\n"
    assert not swfs_repo.file_exists("new.txt")
    status = swfs_repo.status(scan=True)
    assert "old.txt" not in status and "new.txt" not in status, status


@pytest.mark.verified_volume
@pytest.mark.skipif(sys.platform != "win32", reason="swfs is only available on Windows")
@pytest.mark.swfs
@pytest.mark.smoke
def test_a_move_by_rename_mounts_an_swfs_instance_again_from_the_new_root(
    new_lore_repo, lore_service_runner, scratch_dir, global_dir_name
):
    """A move by rename stops the service, which closes the SWFS instance's mount, moves its
    external directory with the root, and starts the service again, which mounts the instance
    from the new root with its files."""
    lore_service_runner.start()
    original_repo = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    file_contents = _random_files()
    original_repo.write_commit_push("test message", file_contents)
    swfs_repo = _swfs_clone(original_repo)
    target = scratch_dir("external-root") / "root"

    try:
        original_repo.service_set_external_directory(target)

        assert os.path.normcase(external_root(global_dir_name)) == os.path.normcase(
            str(target)
        )
        _repositories_match_files(original_repo, swfs_repo, file_contents)
    finally:
        stop_lore_service(original_repo.lore_executable_path, global_dir_name)
