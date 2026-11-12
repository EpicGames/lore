# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""FUSE instances served by the background service: clone, read, sync, a clone
that starts the service, a service stop that meets a busy mount, a forced stop after a
replacement service remounted, two service starts racing one that is mounting, the
relayed staged-link and latest-list listings, an interactive commit through the mount,
and a diff after a sync."""

import contextlib
import glob
import os
import platform
import re
import shutil
import stat
import subprocess
from pathlib import Path
from time import monotonic, sleep

import pytest
from error_types import MountsBusyError, RepositoryAlreadyExistsError, RevisionNotFound
from service_util import (
    LORE_NO_SERVICE_MESSAGE,
    LORE_SERVICE_CLAIM_REFUSED_MESSAGE,
    LORE_SERVICE_ENVIRONMENT,
    LORE_SERVICE_SOCKET_VAR,
    SERVICE_UNAVAILABLE,
)

from lore import Lore

# A marker rather than a skip at import: a module skipped while it is collected
# is reported as skipped even in a run whose marker expression selects none of
# its tests.
pytestmark = [
    pytest.mark.skipif(
        platform.system() != "Linux"
        or not os.path.exists("/dev/fuse")
        or shutil.which("fusermount3") is None,
        reason="FUSE needs Linux with /dev/fuse and fusermount3",
    ),
    pytest.mark.own_vfs(
        "makes its virtual clones from ordinary ones and counts what the service mounts"
    ),
]


def lore_mounts_at(paths: list[str]) -> list[str]:
    """The mount table's FUSE Lore entries at any of `paths`, one per mount
    stacked there, read from the table so the mounts themselves are never
    touched."""
    targets = {os.path.realpath(path) for path in paths}
    entries = []
    with open("/proc/self/mountinfo") as mountinfo:
        for line in mountinfo:
            fields, _, rest = line.partition(" - ")
            parts = fields.split()
            if len(parts) > 4 and parts[4] in targets and rest.startswith("fuse.lore "):
                entries.append(line.strip())
    return entries


def is_mounted(path: str) -> bool:
    """Whether a FUSE Lore mount sits at `path`."""
    return bool(lore_mounts_at([path]))


@pytest.fixture
def fuse_mounts(background_lore_service, lore_executable_path, global_dir_name):
    """Collects the mount paths a test makes and leaves none behind.

    Taken after the service so it is torn down first: the service is stopped with
    `--force`, which detaches whatever is still mounted, and anything left is
    detached lazily before the repository directories are removed."""
    paths: list[str] = []
    yield paths
    env = os.environ.copy()
    env["LORE_GLOBAL_PATH"] = global_dir_name
    env["LORE_AUTH_PATH"] = global_dir_name
    subprocess.run(
        [lore_executable_path, "service", "stop", "--force"],
        capture_output=True,
        check=False,
        env=env,
        cwd="/",
    )
    for path in paths:
        if is_mounted(path):
            subprocess.run(
                ["fusermount3", "-u", "-z", path], capture_output=True, check=False
            )
    left = [path for path in paths if is_mounted(path)]
    assert not left, f"FUSE mounts left behind: {left}"


def fuse_clone(repo: Lore, fuse_mounts: list[str]) -> Lore:
    clone = repo.clone(vfs="fuse", use_shared_store=True)
    fuse_mounts.append(clone.path)
    assert is_mounted(clone.path), "the FUSE clone is mounted"
    return clone


def listing(path: str) -> list[str]:
    return sorted(
        subprocess.run(
            ["ls", path], capture_output=True, text=True, check=True
        ).stdout.split()
    )


def cat(path: str) -> bytes:
    return subprocess.run(["cat", path], capture_output=True, check=True).stdout


@pytest.mark.smoke
def test_clone_vfs_fuse_lists_and_reads(new_lore_repo, fuse_mounts):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    readme = b"hello through fuse\n"
    nested = os.urandom(300_000)
    repo.write_commit_push("initial", {"readme.txt": readme, "dir/nested.bin": nested})

    fuse = fuse_clone(repo, fuse_mounts)
    assert listing(fuse.path) == ["dir", "readme.txt"]
    assert listing(os.path.join(fuse.path, "dir")) == ["nested.bin"]
    assert cat(os.path.join(fuse.path, "readme.txt")) == readme
    assert cat(os.path.join(fuse.path, "dir", "nested.bin")) == nested

    other: Lore = repo.clone()
    other.sync()
    added = b"committed from an OS clone\n"
    other.write_commit_push("add", {"added.txt": added})

    fuse.sync()
    assert listing(fuse.path) == ["added.txt", "dir", "readme.txt"]
    assert cat(os.path.join(fuse.path, "added.txt")) == added


def store_holding(repo: Lore, instance: Lore) -> str | None:
    """The path of the shared store whose registry lists `instance`, read through
    `repo`, an ordinary clone, or `None` where none lists it."""
    wanted = os.path.realpath(instance.path)
    for store, entry in repo.shared_store_list(include_instances=True).entries.items():
        if any(os.path.realpath(path) == wanted for _, path in entry.instances or []):
            return store
    return None


@pytest.mark.smoke
def test_fuse_clone_uses_a_shared_store_unasked(
    new_lore_repo, fuse_mounts, scratch_dir
):
    """A FUSE clone uses a shared store with no `--use-shared-store`, so the service
    finds it when it starts, and takes `--shared-store-path` without the flag."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"hello\n"})

    clone = repo.clone(vfs="fuse")
    fuse_mounts.append(clone.path)
    assert is_mounted(clone.path), "the FUSE clone is mounted"
    assert store_holding(repo, clone) is not None, "a shared store lists the clone"

    base = os.path.realpath(scratch_dir("fuse-clone-store", create=True))
    placed = repo.clone(vfs="fuse", shared_store_path=base)
    fuse_mounts.append(placed.path)
    assert is_mounted(placed.path), "the FUSE clone is mounted"
    store = store_holding(repo, placed)
    assert store is not None and os.path.realpath(store).startswith(base + os.sep), (
        f"the store under {base} lists the clone, not {store}"
    )


@pytest.mark.smoke
@pytest.mark.parametrize("named", [True, False], ids=["named", "none-named"])
def test_fuse_clone_starts_the_service_when_none_is_running(
    new_lore_repo, stops_background_services, named
):
    """A FUSE clone always goes to the service, and with none running it starts
    one, which mounts the clone: from the executable named for the service, or
    with none named, from the `lore` that ran the clone, which runs the service
    as well.

    Runs the CLI directly rather than through `Lore.clone()`, to run it with no
    executable named."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("initial", {"file.txt": b"content"})

    clone_path = str(Path(repo.path).parent / Lore.generate_random_name("fuse-clone"))
    env = repo.sandboxed_env()
    if not named:
        # `sandboxed_env` names the build under test, which the other case uses.
        env.pop("LORE_SERVICE_EXECUTABLE", None)

    def lore(*args: str) -> str:
        ran = subprocess.run(
            [repo.lore_executable_path, *args],
            capture_output=True,
            text=True,
            env=env,
            cwd=repo.path,
        )
        output = ran.stdout + ran.stderr
        assert ran.returncode == 0, f"lore {' '.join(args)} must succeed: {output}"
        return output

    try:
        lore(
            *("--repository", repo.path, "repository", "clone"),
            *(repo.remote_path, clone_path, "--vfs", "fuse", "--use-shared-store"),
        )
        assert is_mounted(clone_path), "the service the clone started mounts it"
        assert cat(os.path.join(clone_path, "file.txt")) == b"content"

        status = lore("service", "status")
        assert os.path.realpath(repo.lore_executable_path) in status, (
            f"the service must be the build under test: {status}"
        )
    finally:
        # Nothing else here holds the mount, so a stop takes it down. Forced and
        # detached anyway, so a failure above leaves no mount behind.
        subprocess.run(
            [repo.lore_executable_path, "service", "stop", "--force"],
            capture_output=True,
            check=False,
            env=env,
            cwd="/",
        )
        if is_mounted(clone_path):
            subprocess.run(
                ["fusermount3", "-u", "-z", clone_path],
                capture_output=True,
                check=False,
            )


@pytest.mark.smoke
def test_fuse_clone_with_a_missing_named_executable_fails(
    new_lore_repo, stops_background_services, global_dir_name
):
    """A named executable is the one a FUSE clone starts the service from, even
    when it is missing and the `lore` that ran the clone could serve: the clone
    fails with ServiceUnavailable naming it, and creates nothing.

    Runs the CLI directly rather than through `Lore.clone()`, which raises a
    typed exception from the command's output and does not expose the raw
    return code."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("initial", {"file.txt": b"content"})
    clone_path = str(Path(repo.path).parent / Lore.generate_random_name("fuse-clone"))
    missing = str(Path(global_dir_name) / "no-such-lore")

    command = subprocess.run(
        [
            *(repo.lore_executable_path, "--repository", repo.path),
            *("repository", "clone", repo.remote_path, clone_path),
            *("--vfs", "fuse", "--use-shared-store"),
        ],
        capture_output=True,
        text=True,
        env=repo.sandboxed_env(LORE_SERVICE_EXECUTABLE=missing),
        cwd=repo.path,
    )
    output = command.stdout + command.stderr

    assert command.returncode == SERVICE_UNAVAILABLE, output
    assert f"starting {missing} failed" in output, output
    assert not Path(clone_path).exists(), "a clone that reached no service creates nothing"


@pytest.mark.smoke
def test_a_command_in_a_clone_served_on_another_socket_starts_no_service(
    new_lore_repo, fuse_mounts
):
    """A command in a virtual clone whose service listens on another socket than
    the command's starts no service, which would not hold the clone's mount, and
    names the socket it looked on."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("initial", {"readme.txt": b"served elsewhere\n"})
    fuse = fuse_clone(repo, fuse_mounts)
    other = repo.sandboxed_env(
        **{LORE_SERVICE_SOCKET_VAR: Lore.generate_random_name("lore_service-other")}
    )

    try:
        status = subprocess.run(
            [repo.lore_executable_path, "status"],
            capture_output=True,
            text=True,
            env=other,
            cwd=fuse.path,
            timeout=60,
        )
        output = status.stdout + status.stderr

        assert status.returncode == SERVICE_UNAVAILABLE, output
        assert other[LORE_SERVICE_SOCKET_VAR] in output, output
        assert listing(fuse.path) == ["readme.txt"], "the clone is still served"
    finally:
        stop = subprocess.run(
            [repo.lore_executable_path, "service", "stop", "--force"],
            capture_output=True,
            text=True,
            env=other,
            cwd="/",
        )
    assert LORE_NO_SERVICE_MESSAGE in stop.stdout + stop.stderr, (
        "no service was started on the command's socket"
    )


@pytest.mark.smoke
def test_a_command_in_a_clone_whose_service_died_starts_one(
    new_lore_repo, background_lore_service, fuse_mounts
):
    """A command in a virtual clone whose service died without taking its mount
    down starts a service, which detaches the dead mount and mounts the clone
    again."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("initial", {"readme.txt": b"served again\n"})
    fuse = fuse_clone(repo, fuse_mounts)
    background_lore_service.kill()
    background_lore_service.wait()

    status = subprocess.run(
        [repo.lore_executable_path, "--repository", fuse.path, "status"],
        capture_output=True,
        text=True,
        env=repo.sandboxed_env(),
        cwd=str(Path(fuse.path).parent),
        timeout=60,
    )

    assert status.returncode == 0, status.stdout + status.stderr
    assert listing(fuse.path) == ["readme.txt"], "the clone is mounted again"


@pytest.mark.smoke
def test_fuse_clone_with_relaying_off_goes_to_the_running_service(
    new_lore_repo, fuse_mounts
):
    """A `lore clone --vfs fuse` reaches a service already running on its own,
    forced by the clone's own vfs argument, even with process-wide relaying
    off: `fuse_mounts` starts the service, and `repo` carries no
    `LORE_USE_SERVICE` override."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("initial", {"readme.txt": b"hello through fuse\n"})

    fuse = fuse_clone(repo, fuse_mounts)
    assert listing(fuse.path) == ["readme.txt"]


@pytest.mark.smoke
def test_link_list_staged_and_latest_list_answer_on_a_virtual_clone(
    new_lore_repo, fuse_mounts
):
    """`lore link list --staged`, `lore branch latest list` and an interactive
    `lore commit`, which lists staged links and layers, relay to the service
    that owns the mount."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"hello through fuse\n"})

    fuse = fuse_clone(repo, fuse_mounts)

    fuse.link_list(staged=True)
    assert len(fuse.branch_latest_list().strip().splitlines()) == 1

    fuse.write_files({"added.txt": b"more content\n"})
    fuse.stage("added.txt")
    fuse.commit("interactive commit through the mount")

    assert len(fuse.branch_latest_list().strip().splitlines()) == 2


@pytest.mark.smoke
def test_diff_after_a_sync_leaves_the_service_running(
    new_lore_repo, background_lore_service, fuse_mounts
):
    """`lore diff` of a file on a virtual clone after a sync relays to the service,
    which keeps running and keeps serving the mount."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"one\n", "dir/other.txt": b"o\n"})

    fuse = fuse_clone(repo, fuse_mounts)
    other: Lore = repo.clone()
    other.sync()
    other.write_commit_push("change", {"readme.txt": b"two\n", "added/new.txt": b"n\n"})
    fuse.sync()
    fuse.write_files({"readme.txt": b"edit\n"})
    fuse.diff("readme.txt")
    assert background_lore_service.poll() is None, (
        "the service keeps running after the diff"
    )
    assert listing(os.path.join(fuse.path, "added")) == ["new.txt"]
    assert listing(os.path.join(fuse.path, "dir")) == ["other.txt"]


@pytest.mark.smoke
def test_failed_virtual_clone_leaves_nothing_and_the_path_clones_again(
    new_lore_repo, fuse_mounts, global_dir_name
):
    """A revision offset that walks back past the repository's only commit fails
    clone after `create_local` has already mounted the instance, so the failure
    exercises the cleanup: no mount, no external directory, the mount point
    `Lore.clone()` made beforehand kept, and the path clones again."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"hello through fuse\n"})
    only_revision = repo.revision_history()[0].signature
    unreachable_revision = f"{only_revision}~1"

    external_root = Path(global_dir_name) / "data" / "external"
    before = set(external_root.iterdir()) if external_root.exists() else set()

    clone_path = str(
        Path(repo.path).parent / Lore.generate_random_name("failed-fuse-clone")
    )
    fuse_mounts.append(clone_path)

    with pytest.raises(RevisionNotFound) as failed:
        repo.clone(
            path=clone_path,
            vfs="fuse",
            use_shared_store=True,
            revision=unreachable_revision,
            level="debug",
        )

    assert f"Initialized repository on disk in {clone_path}" in str(failed.value), (
        "the clone fails after `create_local` made the instance"
    )
    assert not is_mounted(clone_path), "a failed clone leaves no mount behind"
    after = set(external_root.iterdir()) if external_root.exists() else set()
    assert after == before, "a failed clone leaves no external directory behind"
    assert Path(clone_path).is_dir(), (
        "a failed clone keeps the mount point that existed before it"
    )

    fuse = repo.clone(path=clone_path, vfs="fuse", use_shared_store=True)
    fuse_mounts.append(fuse.path)
    assert is_mounted(fuse.path), "the path clones again after the failed attempt"
    assert listing(fuse.path) == ["readme.txt"]


@pytest.mark.smoke
def test_failed_virtual_clone_onto_a_missing_path_removes_the_mount_point(
    new_lore_repo, fuse_mounts
):
    """`Lore.clone()` pre-creates the destination directory for any non-swfs VFS,
    so it never exercises the branch where `create_local` creates the mount point
    itself. Calling the CLI directly onto a path that does not exist does."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"hello through fuse\n"})
    only_revision = repo.revision_history()[0].signature
    unreachable_revision = f"{only_revision}~1"

    clone_path = str(
        Path(repo.path).parent
        / Lore.generate_random_name("failed-fuse-clone-missing")
    )
    fuse_mounts.append(clone_path)
    # Recorded before the clone runs, so a failure that leaves the directory
    # behind is still cleaned up with the test.
    repo.created_paths.append(clone_path)
    assert not Path(clone_path).exists(), "the path must not pre-exist for this test"

    with pytest.raises(RevisionNotFound) as failed:
        repo.run(
            [
                "repository",
                "clone",
                repo.remote_path,
                clone_path,
                "--revision",
                unreachable_revision,
                "--vfs",
                "fuse",
                "--use-shared-store",
            ],
            level="debug",
        )

    assert f"Initialized repository on disk in {clone_path}" in str(failed.value), (
        "the clone fails after `create_local` made the mount point"
    )
    assert not is_mounted(clone_path), "a failed clone leaves no mount behind"
    assert not Path(clone_path).exists(), (
        "a failed clone removes the mount point it created"
    )


@pytest.mark.smoke
@pytest.mark.skipif(
    hasattr(os, "geteuid") and os.geteuid() == 0,
    reason="0o000 does not block root, so the seal below would not simulate an unreadable store",
)
def test_service_start_mounts_past_a_store_it_cannot_list(
    new_lore_repo, fuse_mounts, global_dir_name, lore_service_runner
):
    """A shared store the service cannot open or list at start must not keep it
    from mounting the instances of the stores it can reach."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"hello through fuse\n"})
    fuse = fuse_clone(repo, fuse_mounts)

    repo.service_stop(force=True)
    lore_service_runner.terminate()
    assert not is_mounted(fuse.path), "the stopped service detaches the mount"

    broken_store = Path(global_dir_name) / "broken-shared-store"
    broken_store.mkdir()
    registry_path = Path(global_dir_name) / "data" / "stores" / "registry.toml"
    try:
        os.chmod(broken_store, 0o000)
        with open(registry_path, "a") as registry_file:
            registry_file.write(
                '\n[[entries]]\n'
                'remote_url = "lore://broken"\n'
                f'path = "{broken_store}"\n'
            )

        lore_service_runner.start()

        # The service keeps its log for `service status` to hand back, rather than printing it.
        service_output = repo.service_status()
        assert f"Unable to open shared store at {broken_store}" in service_output, (
            "the service reports the store it could not open"
        )
        assert is_mounted(fuse.path), "the readable store's instance remounts"
        assert listing(fuse.path) == ["readme.txt"]
    finally:
        os.chmod(broken_store, 0o755)


@pytest.mark.smoke
def test_service_stop_reports_busy_then_force_unmounts(new_lore_repo, fuse_mounts):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"held/file.txt": b"held open"})
    fuse = fuse_clone(repo, fuse_mounts)

    outside = str(Path(fuse.path).parent)
    # A process whose working directory is inside the mount keeps it busy, as a
    # shell left in the instance does.
    holder = subprocess.Popen(["sleep", "300"], cwd=os.path.join(fuse.path, "held"))
    try:
        with pytest.raises(MountsBusyError):
            repo.service_stop(use_os_dir=True, cwd=outside)
        assert is_mounted(fuse.path), "a refused stop leaves the mount in place"
        assert cat(os.path.join(fuse.path, "held", "file.txt")) == b"held open"

        repo.service_stop(force=True, use_os_dir=True, cwd=outside)
        assert not is_mounted(fuse.path), "a forced stop detaches the busy mount"
    finally:
        holder.kill()
        holder.wait()


@pytest.mark.smoke
def test_service_stop_force_after_a_remount_detaches_every_mount(
    new_lore_repo, fuse_mounts, lore_service_runner
):
    """A service killed with mounts leaves them stale, and the next command
    starts a replacement that remounts them. A second service started while the
    replacement runs, as a command racing it starts one, must not mount them
    again once the replacement lets go, so a forced stop of the replacement
    leaves none of them in the mount table."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"remounted\n"})
    paths = [fuse_clone(repo, fuse_mounts).path for _ in range(4)]

    killed = lore_service_runner.service_processes[None]
    killed.kill()
    killed.wait()

    repo.status()
    assert {path: listing(path) for path in paths} == {
        path: ["readme.txt"] for path in paths
    }, "the replacement serves every instance again"

    second, second_log = lore_service_runner.launch(str(Path(repo.path).parent))
    # Given its chance to exit first, so the stop cannot hand it the socket.
    with contextlib.suppress(subprocess.TimeoutExpired):
        second.wait(timeout=10)
    repo.service_stop(force=True)
    with contextlib.suppress(subprocess.TimeoutExpired):
        second.wait(timeout=60)

    left = lore_mounts_at(paths)
    second_output = second_log.read_text(errors="replace")
    ending = second_output.splitlines()[-2:]
    assert not left, (
        f"a forced stop leaves mounts attached: {left}; "
        f"the second service {'exited' if second.poll() is not None else 'runs'}: "
        f"{ending}"
    )
    assert LORE_SERVICE_CLAIM_REFUSED_MESSAGE in second_output, (
        f"the second service reports the refused claim: {ending}"
    )


def services_on(socket_name: str) -> list[int]:
    """The `lore service run` processes serving `socket_name`."""
    marker = f"{LORE_SERVICE_SOCKET_VAR}={socket_name}".encode()
    found = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            argv = (entry / "cmdline").read_bytes().split(b"\0")
            environ = (entry / "environ").read_bytes().split(b"\0")
        except OSError:
            continue
        if argv[1:3] == [b"service", b"run"] and marker in environ:
            found.append(int(entry.name))
    return found


@pytest.mark.smoke
def test_a_write_through_a_virtual_clone_survives_a_service_restart(
    new_lore_repo, fuse_mounts, lore_service_runner, global_dir_name
):
    """A file written through a virtual clone lands in the instance's external
    directory, and the replacement service that remounts the instance after the
    first is killed serves it from there."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"tracked\n"})
    fuse = fuse_clone(repo, fuse_mounts)
    content = b"written through the mount\n"
    Path(fuse.path, "local.txt").write_bytes(content)

    [written] = glob.glob(
        os.path.join(global_dir_name, "data", "external", "*", "write", "local.txt")
    )
    assert Path(written).read_bytes() == content, "the write lands under write/"

    killed = lore_service_runner.service_processes[None]
    killed.kill()
    killed.wait()
    repo.status()
    assert cat(os.path.join(fuse.path, "local.txt")) == content, (
        "the replacement serves the write after its remount"
    )


@pytest.mark.smoke
def test_two_service_starts_succeed_while_the_winner_is_still_mounting(
    new_lore_repo,
    fuse_mounts,
    lore_service_runner,
    global_dir_name,
    lore_service_socket,
):
    """Two `lore service start` at once each start a service, and only one claims
    the socket. The other waits for the winner to answer before it exits, so its
    start succeeds too, even while the winner is still mounting."""
    # Imported here, as Windows has no `fcntl` and still collects this module.
    import fcntl

    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"mounted late\n"})
    fuse = fuse_clone(repo, fuse_mounts)
    repo.service_stop(force=True)
    lore_service_runner.terminate()

    # Mounting the instance takes the shared store's lock, so holding it keeps the
    # winner mounting past the grace a start gives a service that has exited.
    store_locks = Path(global_dir_name) / "data" / "shared_store"
    [store_lock] = glob.glob(
        str(store_locks / "*" / "shared_store" / "immutable" / "lock")
    )
    with open(store_lock) as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        starts = [
            subprocess.Popen(
                [repo.lore_executable_path, "service", "start"],
                cwd=repo.path,
                env=repo.sandboxed_env(),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            for _ in range(2)
        ]
        both_ran = False
        deadline = monotonic() + 4
        while monotonic() < deadline and all(start.poll() is None for start in starts):
            both_ran = both_ran or len(services_on(lore_service_socket)) == 2
            with contextlib.suppress(subprocess.TimeoutExpired):
                starts[0].wait(timeout=0.1)
        fcntl.flock(held, fcntl.LOCK_UN)

    outputs = [start.communicate(timeout=60)[0] for start in starts]
    assert [start.returncode for start in starts] == [0, 0], outputs
    assert both_ran, "each start started a service while the winner was mounting"
    deadline = monotonic() + 10
    while monotonic() < deadline and len(services_on(lore_service_socket)) > 1:
        sleep(0.05)
    assert len(services_on(lore_service_socket)) == 1, (
        "the service that lost the claim exits once the winner answers"
    )
    assert listing(fuse.path) == ["readme.txt"], "the winner serves the instance"


def tree(path: str) -> dict[str, tuple[bool, bytes] | None]:
    """Every directory (None) and file (executable bit, content) under `path`,
    `.lore` aside, keyed by its path relative to `path`."""
    found: dict[str, tuple[bool, bytes] | None] = {}
    for root, dirs, files in os.walk(path):
        relative = os.path.relpath(root, path)
        if relative == ".":
            dirs[:] = [name for name in dirs if name != ".lore"]
        for name in dirs:
            found[os.path.normpath(os.path.join(relative, name))] = None
        for name in files:
            full = os.path.join(root, name)
            with open(full, "rb") as file:
                content = file.read()
            executable = bool(os.stat(full).st_mode & stat.S_IXUSR)
            found[os.path.normpath(os.path.join(relative, name))] = (
                executable,
                content,
            )
    return found


def history_names(clone: Lore) -> dict[str, str]:
    """The revision signatures and branch ids `clone`'s own history names, each
    with a placeholder that keeps which revision or which branch it is."""
    names: dict[str, str] = {}
    branches: list[str] = []
    revision = None
    for line in clone.run(["history", "100"]).splitlines():
        key, _, value = line.partition(":")
        key, value = key.strip(), value.strip()
        if key == "Revision":
            revision = value
        elif key == "Signature" and revision is not None:
            names[value] = f"<revision {revision}>"
        elif key == "Merge" and revision is not None:
            names.setdefault(value, f"<merged into revision {revision}>")
        elif key == "Branch" and value not in branches:
            branches.append(value)
    for index, branch_id in enumerate(branches):
        names.setdefault(branch_id, f"<branch id {index}>")
    return names


def normalized(output: str, clone: Lore, branch: str, names: dict[str, str]) -> str:
    """`output` with what differs between two clones by construction replaced: the
    clone's path, its branch, the signatures and branch ids its history names,
    staged states, dates, and how a sync or switch reports the changes it
    verifies. Content addresses and node ids stay compared."""
    output = output.replace(clone.path, "<clone>").replace(branch, "<branch>")
    for name, placeholder in names.items():
        output = output.replace(name, placeholder)
    # A staged state is no revision, so no history names it.
    output = re.sub(
        r"(?m)^(Staged (?:merged )?repository state) [0-9a-f]{64}$",
        r"\1 <staged state>",
        output,
    )
    output = re.sub(r"(?m)^(Date\s*:).*$", r"\1 <date>", output)
    # A mount verifies against the State it holds (`hold::check`), an ordinary
    # clone calculates deltas and verifies against its files (`realize_state`);
    # the change count must still match.
    output = re.sub(
        r"(?m)^Verifying (\d+) changes with the held state$",
        r"<verifying \1 changes>",
        output,
    )
    output = re.sub(
        r"(?m)^Calculating deltas \d+ -> \d+\nVerifying (\d+) changes with local file system$",
        r"<verifying \1 changes>",
        output,
    )
    return output


@pytest.mark.smoke
def test_edit_loop_matches_an_ordinary_clone(
    new_lore_repo, background_lore_service, fuse_mounts, global_dir_name
):
    """The edit loop on a virtual clone and on an ordinary clone of one repository,
    each on a branch of its own: edits, `status` at once, `stage`, an interactive
    `commit`, `push`, `sync` of a remote change, `branch switch` both ways,
    `reset`, a merging `sync` and `diff` give the same output, and leave the same
    files, after every step. The ordinary clone finds its edits with a scan; the
    mount records them itself."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push(
        "base",
        {
            "readme.txt": b"base readme\n",
            "src/main.c": b"int main(void) { return 0; }\n",
            "src/util.c": b"int util(void) { return 1; }\n",
            "docs/guide.md": b"# Guide\n",
            "gone.txt": b"deleted by the loop\n",
        },
    )
    branches = {"virtual": "loop-fuse", "ordinary": "loop-disk"}
    for branch in branches.values():
        repo.branch_create(branch)
        repo.branch_push(branch)
        repo.branch_switch("main")

    clones = {
        "virtual": fuse_clone_of(
            repo, branches["virtual"], fuse_mounts, global_dir_name
        ),
        "ordinary": repo.clone(branch=branches["ordinary"]),
    }
    remotes = {kind: repo.clone(branch=branch) for kind, branch in branches.items()}
    mismatches: list[str] = []

    def each(step: str, command) -> None:
        """Runs `command` on both clones and records any difference in its
        output or in the files it leaves."""
        outputs = {kind: command(kind, clone) or "" for kind, clone in clones.items()}
        outputs = {
            kind: normalized(outputs[kind], clone, branches[kind], history_names(clone))
            for kind, clone in clones.items()
        }
        if outputs["virtual"] != outputs["ordinary"]:
            mismatches.append(
                f"{step}: output differs\n--- virtual\n{outputs['virtual']}"
                f"\n--- ordinary\n{outputs['ordinary']}"
            )
        trees = {kind: tree(clone.path) for kind, clone in clones.items()}
        if trees["virtual"] != trees["ordinary"]:
            mismatches.append(
                f"{step}: files differ\n--- virtual\n{trees['virtual']}"
                f"\n--- ordinary\n{trees['ordinary']}"
            )

    def remote_change(kind: str, files: dict[str, bytes]) -> None:
        remote = remotes[kind]
        remote.sync()
        remote.write_commit_push(f"remote change to {branches[kind]}", files)

    def edit(kind: str, clone: Lore) -> None:
        clone.write_files(
            {
                "src/main.c": b"int main(void) { return util(); }\n",
                "added/feature.c": b"void feature(void) {}\n",
                "notes.txt": b"notes\n",
            }
        )
        clone.remove_file("gone.txt")
        if kind == "ordinary":
            clone.status(scan=True)

    each("clone", lambda kind, clone: None)
    each("edit", edit)
    # A plain status on a mount scans, its provider's diff being exact, and reports what a plain
    # status of the ordinary clone reports of the marks its scan above left.
    each("status", lambda kind, clone: clone.status())
    each("stage", lambda kind, clone: clone.stage("."))
    each("commit", lambda kind, clone: clone.commit("loop: local edits"))
    each("push", lambda kind, clone: clone.push())
    for kind in clones:
        remote_change(kind, {"src/util.c": b"int util(void) { return 2; }\n"})
    each("sync", lambda kind, clone: clone.sync())
    each("switch to main", lambda kind, clone: clone.branch_switch("main"))
    each("switch back", lambda kind, clone: clone.branch_switch(branches[kind]))

    def discard(kind: str, clone: Lore) -> None:
        clone.write_files({"readme.txt": b"discarded edit\n"})
        clone.remove_file("docs/guide.md")

    each("edit to reset", discard)
    each("reset", lambda kind, clone: clone.reset(["readme.txt", "docs/guide.md"]))

    def commit_unpushed(kind: str, clone: Lore) -> str:
        clone.write_files({"mine.txt": b"committed before the merge\n"})
        clone.stage("mine.txt")
        return clone.commit("loop: unpushed")

    each("commit unpushed", commit_unpushed)
    for kind in clones:
        remote_change(kind, {"theirs.txt": b"pushed before the merge\n"})
    each("merging sync", lambda kind, clone: clone.sync())

    def diff(kind: str, clone: Lore) -> str:
        clone.write_files({"readme.txt": b"base readme\nedited for the diff\n"})
        if kind == "ordinary":
            clone.status(scan=True)
        return clone.diff("readme.txt")

    each("diff", diff)

    assert background_lore_service.poll() is None, "no command takes the service down"
    assert not mismatches, "\n\n".join(mismatches)


def fuse_clone_of(
    repo: Lore, branch: str, fuse_mounts: list[str], global_dir_name: str
) -> Lore:
    """A virtual clone of `branch` with the test identity an ordinary clone gets,
    seeded into its config outside the mount, where `Lore.clone` does not look."""
    clone = repo.clone(branch=branch, vfs="fuse", use_shared_store=True)
    fuse_mounts.append(clone.path)
    assert is_mounted(clone.path), "the FUSE clone is mounted"
    [config] = glob.glob(
        os.path.join(global_dir_name, "data", "external", "*", ".lore", "config.toml")
    )
    text = Path(config).read_text()
    if not any(line.strip().startswith("identity") for line in text.splitlines()):
        Path(config).write_text('identity = "test-user"\n' + text)
    return clone


def external_dirs(global_dir_name: str) -> list[str]:
    """The external directories the service holds for FUSE instances."""
    return glob.glob(os.path.join(global_dir_name, "data", "external", "*"))


@pytest.mark.smoke
def test_instance_delete_unmounts_and_removes_a_fuse_instance(
    new_lore_repo, fuse_mounts, global_dir_name
):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"hello\n"})
    survivor = fuse_clone(repo, fuse_mounts)
    survivor_external = set(external_dirs(global_dir_name))
    fuse = fuse_clone(repo, fuse_mounts)
    [external] = set(external_dirs(global_dir_name)) - survivor_external
    outside = str(Path(fuse.path).parent)
    assert fuse.path in survivor.repository_instance_list(), "the shared store lists it"

    fuse.repository_instance_delete(dry_run=True, cwd=outside)
    assert is_mounted(fuse.path), "a dry run leaves the instance mounted"

    fuse.repository_instance_delete(cwd=outside)
    assert not is_mounted(fuse.path), "the instance is unmounted"
    assert not os.path.exists(fuse.path), "its mount point is removed"
    assert not os.path.exists(external), "its external directory is removed"
    listed = survivor.repository_instance_list()
    assert fuse.path not in listed, "it is no longer registered"
    assert survivor.path in listed, "the other instance still is"
    assert is_mounted(survivor.path), "and stays mounted"


@pytest.mark.smoke
def test_instance_delete_of_a_busy_fuse_instance_changes_nothing(
    new_lore_repo, fuse_mounts, global_dir_name
):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"held/file.txt": b"held open"})
    fuse = fuse_clone(repo, fuse_mounts)
    [external] = external_dirs(global_dir_name)
    outside = str(Path(fuse.path).parent)

    holder = subprocess.Popen(["sleep", "300"], cwd=os.path.join(fuse.path, "held"))
    try:
        with pytest.raises(MountsBusyError):
            fuse.repository_instance_delete(cwd=outside)
        assert is_mounted(fuse.path), "a refused delete leaves the instance mounted"
        assert cat(os.path.join(fuse.path, "held", "file.txt")) == b"held open"
        assert os.path.exists(external), "and its external directory in place"
        assert fuse.path in fuse.repository_instance_list(), "and it stays registered"
    finally:
        holder.kill()
        holder.wait()

    fuse.repository_instance_delete(cwd=outside)
    assert not is_mounted(fuse.path), "once nothing holds it, the delete goes ahead"
    assert not os.path.exists(external)


@pytest.mark.smoke
def test_forced_create_and_clone_refuse_to_replace_a_fuse_instance(
    new_lore_repo, fuse_mounts, global_dir_name
):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"hello\n"})
    fuse = fuse_clone(repo, fuse_mounts)
    [external] = external_dirs(global_dir_name)
    dot_lore = os.path.join(external, ".lore")
    tree = sorted(os.listdir(fuse.path))

    with pytest.raises(RepositoryAlreadyExistsError):
        fuse.run(
            [
                "repository",
                "create",
                repo.remote + Lore.generate_random_name(),
                "--force",
            ]
        )
    for destination in [fuse.path, os.path.join(fuse.path, "nested")]:
        with pytest.raises(RepositoryAlreadyExistsError):
            repo.run(["repository", "clone", repo.remote_path, destination, "--force"])

    assert os.path.isdir(dot_lore), "the instance's Lore directory stays"
    assert is_mounted(fuse.path), "the instance stays mounted"
    assert sorted(os.listdir(fuse.path)) == tree, "and nothing is written through it"
    assert fuse.path in fuse.repository_instance_list(), "and it stays registered"


@pytest.mark.smoke
def test_instance_move_remounts_a_fuse_instance(
    new_lore_repo, scratch_dir, fuse_mounts
):
    """The scratch directory is requested before `fuse_mounts`, so the mount at the target is
    detached before the directory holding it is removed."""
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    contents = {"readme.txt": b"hello\n", "dir/nested.txt": b"nested\n"}
    repo.write_commit_push("initial", contents)
    fuse = fuse_clone(repo, fuse_mounts)
    source = fuse.path
    target = scratch_dir("fuse_move_target_parent", create=True) / "moved_fuse"
    fuse_mounts.append(str(target))
    outside = str(target.parent)

    fuse.repository_instance_move(str(target), dry_run=True, cwd=outside)
    assert is_mounted(source), "a dry run leaves the instance mounted at the source"
    assert not os.path.exists(target), "and mounts nothing at the target"

    clean = fuse.status()
    fuse.repository_instance_move(str(target), cwd=outside)
    assert not os.path.exists(source), "the source mount point is gone"
    assert is_mounted(str(target)), "the instance is mounted at the target"
    for name, expected in contents.items():
        assert cat(os.path.join(target, name)) == expected

    fuse.path = str(target)
    assert fuse.status() == clean, "a clean instance stays clean"
    listed = fuse.repository_instance_list()
    assert str(target) in listed, "the registration names the target"
    assert source not in listed, "and no longer the source"
    fuse.write_files({"added.txt": b"written after the move\n"})
    fuse.stage(["added.txt"])
    fuse.commit("after the move")
    assert cat(os.path.join(target, "added.txt")) == b"written after the move\n"


@pytest.mark.smoke
def test_instance_move_of_a_busy_fuse_instance_changes_nothing(
    new_lore_repo, scratch_dir, fuse_mounts
):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"held/file.txt": b"held open"})
    fuse = fuse_clone(repo, fuse_mounts)
    target = scratch_dir("fuse_busy_move_target_parent", create=True) / "moved_fuse"
    outside = str(target.parent)

    holder = subprocess.Popen(["sleep", "300"], cwd=os.path.join(fuse.path, "held"))
    try:
        with pytest.raises(MountsBusyError):
            fuse.repository_instance_move(str(target), cwd=outside)
        assert is_mounted(fuse.path), "a refused move leaves the instance mounted"
        assert not os.path.exists(target), "and mounts nothing at the target"
        assert cat(os.path.join(fuse.path, "held", "file.txt")) == b"held open"
        assert fuse.path in fuse.repository_instance_list(), "and registered at the source"
    finally:
        holder.kill()
        holder.wait()

    fuse_mounts.append(str(target))
    fuse.repository_instance_move(str(target), cwd=outside)
    assert is_mounted(str(target)), "once nothing holds it, the move goes ahead"
    assert cat(os.path.join(target, "held", "file.txt")) == b"held open"


@pytest.mark.smoke
def test_instance_move_keeps_the_uncommitted_changes_of_a_fuse_instance(
    new_lore_repo, scratch_dir, fuse_mounts
):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push(
        "initial",
        {
            "kept.txt": b"kept\n",
            "edited.txt": b"before\n",
            "gone.txt": b"gone\n",
            "staged.txt": b"staged before\n",
        },
    )
    fuse = fuse_clone(repo, fuse_mounts)
    target = scratch_dir("fuse_move_changes_parent", create=True) / "moved_fuse"
    fuse_mounts.append(str(target))
    outside = str(target.parent)
    fuse.write_files(
        {
            "edited.txt": b"after\n",
            "added.txt": b"added\n",
            "staged.txt": b"staged after\n",
        }
    )
    fuse.remove_file("gone.txt")
    fuse.stage(["staged.txt"])
    changed = fuse.status()

    fuse.repository_instance_move(str(target), cwd=outside)
    fuse.path = str(target)
    assert fuse.status() == changed, "every change moves with it, staged or not"
    assert cat(os.path.join(target, "edited.txt")) == b"after\n"
    assert cat(os.path.join(target, "added.txt")) == b"added\n"
    assert cat(os.path.join(target, "staged.txt")) == b"staged after\n"
    assert not os.path.exists(os.path.join(target, "gone.txt"))


@pytest.mark.smoke
def test_a_moved_fuse_instance_is_mounted_at_its_target_after_a_service_restart(
    new_lore_repo, scratch_dir, fuse_mounts, lore_service_runner, global_dir_name
):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    repo.write_commit_push("initial", {"readme.txt": b"tracked\n"})
    fuse = fuse_clone(repo, fuse_mounts)
    source = fuse.path
    [external] = external_dirs(global_dir_name)
    target = scratch_dir("fuse_move_restart_parent", create=True) / "moved_fuse"
    fuse_mounts.append(str(target))
    fuse.write_files({"local.txt": b"written before the move\n"})
    fuse.repository_instance_move(str(target), cwd=str(target.parent))

    killed = lore_service_runner.service_processes[None]
    killed.kill()
    killed.wait()
    repo.status()
    assert is_mounted(str(target)), "the replacement mounts the instance at its target"
    assert not os.path.exists(source), "and nothing at its source"
    assert os.path.exists(external), "keeping its external directory"
    assert cat(os.path.join(target, "readme.txt")) == b"tracked\n"
    assert cat(os.path.join(target, "local.txt")) == b"written before the move\n"
