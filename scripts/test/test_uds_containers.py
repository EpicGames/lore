# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""End-to-end test of the machine server with many containers on its sockets.

The topology, on one Linux host with Docker:

- the mock auth service of mock_auth_server.py, in the test process;
- the upstream U, a loreserver with authentication on, which holds the
  repositories and is the only server the host's own clients reach;
- the machine server M, a loreserver whose QUIC and HTTP listeners are off,
  serving one socket per container: its immutable store is a local tier over U,
  its mutable store is U, and it forwards gRPC for `127.0.0.1` to U and for
  `localhost` to the auth service. It reaches U with a login of its own;
- N containers without a network, each with its socket's directory mounted at
  /run/lore and LORE_UDS_SOCKET naming the socket, running the client and the
  Lore service from binaries mounted read-only. USER1's containers may use
  repositories A, B and C, USER2's A and C. Only the cold-clone test reads C.
  One more container mounts a socket but lacks the variable.

Opt-in: LORE_UDS_CONTAINER_IMAGE names a local Docker image to run the
containers from, such as `ubuntu:26.04`, and LORE_UDS_CONTAINERS their number,
8 by default. The rig also needs the `uds-bench` example, found beside the
client binary under `examples/` or where LORE_UDS_BENCH_BINARY names it, and
fails without it. The tests share one rig, and none depends on another having
run. Each run names its containers after a run id of its own and labels them
`lore.test=uds-containers`, so `docker ps --filter label=lore.test=uds-containers`
finds any that an aborted run left behind.
"""

import logging
import os
import re
import shutil
import statistics
import subprocess
import sys
import uuid
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from time import monotonic
from types import SimpleNamespace

import pytest
from cleanup_util import remove_tree
from lore_server import _kill_server_by_pid
from mock_auth_server import (
    USER1,
    USER2,
    USER2_API_KEY,
    MockAuthServer,
    MockUser,
    user_token_response,
)
from test_auth_online import (
    authz_resources,
    login_api_key,
    script_api_key_login,
    script_partition_access,
    script_repository_lifecycle,
)
from uds_servers import (
    LORE_HOST,
    MACHINE,
    UNIX_REMOTE,
    host_client,
    launch_upstream,
    log_in_machine,
    machine_server,
    settled_footprint,
)

from lore import Lore

logger = logging.getLogger(__name__)

IMAGE = os.environ.get("LORE_UDS_CONTAINER_IMAGE", "")
# At least one container for each user.
CONTAINER_COUNT = max(2, int(os.environ.get("LORE_UDS_CONTAINERS", "8")))


def _image_present(image: str) -> bool:
    if not image or shutil.which("docker") is None:
        return False
    inspected = subprocess.run(
        ["docker", "image", "inspect", image], capture_output=True, check=False
    )
    return inspected.returncode == 0


pytestmark = [
    pytest.mark.slow,
    pytest.mark.xdist_group("uds_containers"),
    pytest.mark.skipif(
        sys.platform != "linux",
        reason="the rig bind-mounts Unix sockets into Linux containers on the host",
    ),
    pytest.mark.skipif(
        not _image_present(IMAGE),
        reason="LORE_UDS_CONTAINER_IMAGE must name a Docker image present locally",
    ),
]

API_KEYS = {
    USER1.user_id: "uds-user1-api-key",
    USER2.user_id: USER2_API_KEY,
}

CONTAINER_LABEL = "lore.test=uds-containers"
CONTAINER_SOCKET = "/run/lore/lore.sock"
EXEC_TIMEOUT = 300

MIB = 1024 * 1024


def repository_files(tag: str) -> dict[str, bytes]:
    """A few MiB of random data and a few hundred small files naming `tag`, so
    that two repositories share no content and no address."""
    files = {
        "readme.txt": f"repository {tag}\n".encode(),
        "data/large-1.bin": os.urandom(3 * MIB),
        "data/large-2.bin": os.urandom(MIB + 12345),
    }
    for index in range(300):
        files[f"src/{index // 50:02}/file-{index:03}.txt"] = (
            f"{tag} small file {index}\n".encode() * (1 + index % 7)
        )
    return files


def large_bytes(files: dict[str, bytes]) -> int:
    return sum(
        len(content) for name, content in files.items() if name.startswith("data/")
    )


def write_tree(root: Path, files: dict[str, bytes]) -> None:
    for name, content in files.items():
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_bytes(content)


def assert_tree(root: Path, files: dict[str, bytes]) -> None:
    for name, content in files.items():
        path = root / name
        assert path.is_file(), f"{path} is missing"
        assert path.read_bytes() == content, f"{path} differs"


@dataclass
class Outcome:
    code: int
    output: str
    seconds: float


def run_logged(argv: list[str], timeout: float = EXEC_TIMEOUT) -> Outcome:
    logger.info("Running: %s", " ".join(argv))
    start = monotonic()
    try:
        proc = subprocess.run(
            argv, capture_output=True, text=True, timeout=timeout, check=False
        )
        code, output = proc.returncode, proc.stdout + proc.stderr
    except subprocess.TimeoutExpired as expired:
        text = [
            part.decode(errors="replace") if isinstance(part, bytes) else part
            for part in (expired.stdout, expired.stderr)
            if part
        ]
        code, output = -1, f"timed out after {timeout}s\n" + "".join(text)
    outcome = Outcome(code, output, monotonic() - start)
    logger.info("Exit %d after %.2fs:\n%s", code, outcome.seconds, output)
    return outcome


def docker(*args: str, timeout: float = EXEC_TIMEOUT) -> Outcome:
    return run_logged(["docker", *args], timeout)


def remove_container(name: str) -> None:
    """Removes the container `name`, if there is one. Its init is asked to stop
    from inside first: on some hosts the daemon's own kill waits 10 seconds for
    an exit event it never sees, and a removal can then fail outright."""
    if not docker("ps", "--all", "--quiet", "--filter", f"name=^{name}$").output:
        return
    docker("exec", name, "kill", "-TERM", "1")
    docker("wait", name, timeout=30)
    docker("rm", "--force", name)


@dataclass
class Container:
    name: str
    user: MockUser
    root: Path
    """The host directory mounted at /c."""

    def host_path(self, path: str) -> Path:
        """The host path of `path` under /c."""
        return self.root / Path(path).relative_to("/c")

    def exec(self, *argv: str, timeout: float = EXEC_TIMEOUT) -> Outcome:
        return docker("exec", "-w", "/c", self.name, *argv, timeout=timeout)

    def lore(self, *args: str) -> Outcome:
        return self.exec("lore", *args)

    def clone(self, repository: "Repository", directory: str, *args: str) -> Outcome:
        return self.lore(
            "repository",
            "clone",
            f"{UNIX_REMOTE}{repository.name}",
            f"/c/work/{directory}",
            *args,
        )

    def lore_pids(self) -> set[str]:
        """The container's processes running the mounted client: its service,
        once no command is running."""
        listing = self.exec(
            "sh",
            "-c",
            'for p in /proc/[0-9]*; do [ "$(readlink $p/exe)" = /opt/lore/lore ]'
            ' && echo "${p#/proc/}"; done',
        )
        return set(listing.output.split())


@dataclass
class Repository:
    name: str
    id: str
    files: dict[str, bytes]
    host: Lore
    """USER1's working tree on the host, reaching U directly."""


def ok(container: Container, outcome: Outcome) -> Outcome:
    assert outcome.code == 0, f"{container.name} failed:\n{outcome.output}"
    return outcome


def at_once(containers: list[Container], action) -> tuple[list[Outcome], float]:
    """Runs `action` for every container at the same time. Returns the outcomes
    and the wall time of the whole."""
    start = monotonic()
    with ThreadPoolExecutor(max_workers=len(containers)) as pool:
        outcomes = list(pool.map(action, containers))
    return outcomes, monotonic() - start


def report(label: str, outcomes: list[Outcome], wall: float) -> None:
    times = sorted(outcome.seconds for outcome in outcomes)
    line = (
        f"{label}: {len(times)} containers at once, wall {wall:.2f}s, "
        f"each min {times[0]:.2f}s median {statistics.median(times):.2f}s "
        f"max {times[-1]:.2f}s"
    )
    logger.info(line)
    print(line)


def start_container(name, run, root, socket_dir, bin_dir, socket_variable=True) -> None:
    for directory in ("home", "run", "auth", "work"):
        (root / directory).mkdir(parents=True, exist_ok=True)
        (root / directory).chmod(0o700)
    env = {
        "PATH": "/opt/lore:/usr/bin:/bin",
        "HOME": "/c/home",
        "XDG_RUNTIME_DIR": "/c/run",
        "LORE_AUTH_STORE": "fallback",
        "LORE_AUTH_PATH": "/c/auth",
        "LORE_USE_SERVICE": "1",
        "LORE_SERVICE_EXECUTABLE": "/opt/lore/lore",
        "RUST_BACKTRACE": "1",
    }
    if socket_variable:
        env["LORE_UDS_SOCKET"] = CONTAINER_SOCKET
    outcome = docker(
        "run",
        "--detach",
        "--init",
        "--name",
        name,
        "--label",
        CONTAINER_LABEL,
        "--label",
        f"lore.test.run={run}",
        "--network",
        "none",
        "--user",
        f"{os.getuid()}:{os.getgid()}",
        "--volume",
        f"{bin_dir}:/opt/lore:ro",
        "--volume",
        f"{socket_dir}:/run/lore",
        "--volume",
        f"{root}:/c",
        *(arg for key, value in env.items() for arg in ("--env", f"{key}={value}")),
        IMAGE,
        "sleep",
        "infinity",
    )
    assert outcome.code == 0, outcome.output


@pytest.fixture(scope="module")
def rig(request, tmp_path_factory, lore_executable_path, lore_server_executable_path):
    root = tmp_path_factory.mktemp("uds-rig")
    # Copies, so that a rebuild during the run changes nothing under it.
    bin_dir = root / "bin"
    bin_dir.mkdir()
    bench = Path(
        os.environ.get("LORE_UDS_BENCH_BINARY")
        or Path(lore_executable_path).parent / "examples" / "uds-bench"
    )
    if not bench.is_file():
        pytest.fail(
            f"no uds-bench at {bench}: build the example beside the client "
            "(`cargo build --release -p lore-server --features uds-bench "
            "--example uds-bench` for a release client), "
            "or name it with LORE_UDS_BENCH_BINARY"
        )
    for source, target in (
        (lore_executable_path, "lore"),
        (lore_server_executable_path, "loreserver"),
        (bench, "uds-bench"),
    ):
        if not Path(source).is_file():
            pytest.fail(f"no {target} binary at {source}")
        shutil.copy2(source, bin_dir / target)

    mock = MockAuthServer().start()
    cleanups = [mock.stop]
    try:
        upstream = launch_upstream(
            request, tmp_path_factory, mock, bin_dir / "loreserver"
        )
        cleanups.append(lambda: upstream.log_fd.close())
        cleanups.append(lambda: _kill_server_by_pid(upstream.proc.pid))
        upstream_url = f"lore://{LORE_HOST}:{upstream.port}/"
        lore_bin = str(bin_dir / "lore")
        machine_login = log_in_machine(mock, root, lore_bin, upstream_url)

        repositories = {tag: uuid.uuid4().hex for tag in ("A", "B", "C")}
        login_tokens = {MACHINE.user_id: machine_login.token}
        for user in (USER1, USER2):
            login_tokens[user.user_id] = mock.mint_token(user)
            script_api_key_login(
                mock, user, login_tokens[user.user_id], API_KEYS[user.user_id]
            )
        grants = {USER1: ("A", "B", "C"), MACHINE: ("A", "B", "C"), USER2: ("A", "C")}
        for user, tags in grants.items():
            for tag in tags:
                resource_id = f"urc-{repositories[tag]}"
                script_partition_access(
                    mock,
                    user,
                    login_tokens[user.user_id],
                    resource_id,
                    mock.mint_token(user, resources=authz_resources(resource_id)),
                )
        for repository_id in repositories.values():
            script_repository_lifecycle(mock, f"urc-{repository_id}")

        owner_auth = root / "auth" / "owner"
        owner_auth.mkdir(parents=True)
        login_api_key(
            host_client(root, lore_bin, owner_auth, upstream_url),
            upstream_url,
            API_KEYS[USER1.user_id],
        )
        repos = {}
        for tag, repository_id in repositories.items():
            host = host_client(
                root, lore_bin, owner_auth, upstream_url, name=f"repo-{tag}"
            )
            host.repository_create(repo_id=repository_id, identity=USER1.user_id)
            files = repository_files(tag)
            write_tree(Path(host.path), files)
            host.stage(scan=True)
            host.commit(f"files of {tag}")
            host.push()
            repos[tag] = Repository(host.name, repository_id, files, host)

        # Names of this run's own, so that another run's containers are left
        # alone and an aborted run's never clash with this one's.
        run = uuid.uuid4().hex[:8]
        names = [f"lore-uds-{run}-c{index}" for index in range(CONTAINER_COUNT + 1)]
        # One directory per socket, mounted rather than the socket itself, so a
        # container sees the socket M binds anew when it restarts. Named by
        # index, which keeps the socket's path short.
        socket_dirs = {
            name: root / "sockets" / f"s{index}" for index, name in enumerate(names)
        }
        for directory in socket_dirs.values():
            directory.mkdir(parents=True, mode=0o700)
        machine = machine_server(
            request,
            tmp_path_factory,
            mock,
            bin_dir / "loreserver",
            upstream,
            [socket_dirs[name] / "lore.sock" for name in names],
            machine_login.auth,
        )
        cleanups.append(machine.stop)
        machine.start()

        def remove_containers():
            with ThreadPoolExecutor(max_workers=len(names)) as pool:
                list(pool.map(remove_container, names))

        cleanups.append(remove_containers)
        containers = []
        for index, name in enumerate(names):
            # USER2 on every fourth socket from the second, USER1 elsewhere, so
            # that even two containers have one of each.
            user = USER2 if index % 4 == 2 else USER1
            container = Container(name, user, root / "containers" / name)
            start_container(
                name,
                run,
                container.root,
                socket_dirs[name],
                bin_dir,
                socket_variable=index > 0,
            )
            containers.append(container)
        bare, containers = containers[0], containers[1:]

        logins, _ = at_once(
            containers,
            lambda c: c.lore(
                "auth",
                "login",
                UNIX_REMOTE,
                "--token-type",
                "api-key",
                "--token",
                API_KEYS[c.user.user_id],
            ),
        )
        for container, outcome in zip(containers, logins):
            ok(container, outcome)

        yield SimpleNamespace(
            root=root,
            mock=mock,
            machine=machine,
            repos=repos,
            containers=containers,
            bare=bare,
            login_tokens=login_tokens,
        )
    finally:
        for cleanup in reversed(cleanups):
            cleanup()
        if not request.config.getoption("--keep-test-data"):
            remove_tree(root, label="container rig")


def users(rig, user: MockUser) -> list[Container]:
    return [container for container in rig.containers if container.user is user]


def test_concurrent_cold_clones(rig):
    """Every container clones C through its socket at once, while M's local tier
    holds none of it: each gets C byte for byte, and the tier holds C after."""
    repository = rig.repos["C"]
    addresses = {
        bytes.fromhex(info.hash)
        for info in repository.host.file_info(list(repository.files), offline=True)
    }
    before = settled_footprint(rig.machine.store)
    assert not addresses & before.hashes, "M's local tier is not cold to C"
    outcomes, wall = at_once(rig.containers, lambda c: c.clone(repository, "c-cold"))
    report("cold clone of C", outcomes, wall)
    for container, outcome in zip(rig.containers, outcomes):
        ok(container, outcome)
        assert_tree(container.host_path("/c/work/c-cold"), repository.files)

    after = settled_footprint(rig.machine.store)
    grown = after.bytes - before.bytes
    print(f"M's local tier grew by {grown} bytes across the cold clones")
    assert addresses <= after.hashes, (
        f"M's local tier lacks {len(addresses - after.hashes)} of C's file hashes"
    )
    assert grown >= large_bytes(repository.files), (
        f"M's local tier grew by {grown} bytes, less than C's random data"
    )


def test_warm_clones(rig):
    """With A in M's local tier, every container clones it again at once, and
    the tier takes in nothing new: the same hashes, the same bytes."""
    repository = rig.repos["A"]
    first = rig.containers[0]
    ok(first, first.clone(repository, "a-warmup"))
    before = settled_footprint(rig.machine.store)

    outcomes, wall = at_once(rig.containers, lambda c: c.clone(repository, "a-warm"))
    report("warm clone of A", outcomes, wall)
    for container, outcome in zip(rig.containers, outcomes):
        ok(container, outcome)
        assert_tree(container.host_path("/c/work/a-warm"), repository.files)

    after = settled_footprint(rig.machine.store)
    print(f"M's local tier across the warm clones: {before.bytes} -> {after.bytes}")
    assert after.hashes == before.hashes, (
        f"M's local tier indexed {len(after.hashes - before.hashes)} new hashes"
    )
    assert after.bytes == before.bytes, (
        f"M's local tier grew from {before.bytes} to {after.bytes} bytes"
    )


def test_isolation_at_the_machine_server(rig):
    """M holds B once USER1's containers have cloned it, and still serves none
    of it to USER2: a clone fails, and so do reads of the addresses of B's
    files, whether USER2 asks for B's partition with no grant, presents a token
    for A at M's session start for B, or reads through a session on A. USER1
    reads the same addresses."""
    repository = rig.repos["B"]
    readers = users(rig, USER1)
    outsider = users(rig, USER2)[0]

    before = settled_footprint(rig.machine.store)
    outcomes, wall = at_once(readers, lambda c: c.clone(repository, "b"))
    report("clone of B", outcomes, wall)
    for container, outcome in zip(readers, outcomes):
        ok(container, outcome)
        assert_tree(container.host_path("/c/work/b"), repository.files)
    after = settled_footprint(rig.machine.store)
    assert after.bytes - before.bytes >= large_bytes(repository.files), (
        "B must be in M's local tier before USER2 asks for it"
    )

    denied = outsider.clone(repository, "b-denied")
    assert denied.code != 0, f"USER2 cloned B:\n{denied.output}"
    assert not outsider.host_path("/c/work/b-denied/readme.txt").exists()
    print(f"USER2's clone of B: {denied.output.strip()[:300]}")

    # Each file's address, as `uds-bench get` reads them: the hash, then the
    # context. The clones fetched these through M.
    addresses = b"".join(
        bytes.fromhex(info.hash) + bytes.fromhex(info.context)
        for info in repository.host.file_info(list(repository.files), offline=True)
    )
    for container in (readers[0], outsider):
        container.host_path("/c/addresses-b.bin").write_bytes(addresses)

    def bench(container, repository_id):
        return container.exec(
            "uds-bench", "get", UNIX_REMOTE, repository_id, "/c/addresses-b.bin", "4"
        )

    allowed = ok(readers[0], bench(readers[0], repository.id))
    distinct = re.search(r"(\d+) distinct addresses", allowed.output)
    served = re.search(r"get pass=0 c=4: n=(\d+)", allowed.output)
    assert distinct and served, allowed.output
    assert int(served[1]) == int(distinct[1]) > 0, allowed.output
    print(f"USER1 reads B's files: {served[0]} of {len(addresses) // 48} addresses")

    def refused(label, repository_id, at_machine=True):
        refusals = rig.machine.refusals()
        outcome = bench(outsider, repository_id)
        assert outcome.code != 0, f"USER2 read B {label}:\n{outcome.output}"
        if at_machine:
            assert rig.machine.refusals() > refusals, (
                f"M logged no refusal when USER2 read B {label}:\n{outcome.output}"
            )
        error = outcome.output.split("Stack backtrace")[0].split("\n", 1)[-1]
        print(f"USER2 reads B {label}: {' '.join(error.split())}")

    # The auth service refuses USER2 a token for B, and the client stops there.
    refused("by its partition", repository.id, at_machine=False)

    # An auth service answering with a token for what USER2 holds elsewhere
    # brings the session start to M, which must check the grant against the
    # partition and not only the signature.
    b_resource, a_resource = (f"urc-{rig.repos[tag].id}" for tag in "BA")
    token_for_a = rig.mock.on(
        "ExchangeUserTokenForMultiresourceToken",
        bearer=rig.login_tokens[USER2.user_id],
        resource_id=b_resource,
    ).respond(
        user_token_response(
            USER2, rig.mock.mint_token(USER2, resources=authz_resources(a_resource))
        )
    )
    try:
        refused("by its partition with a token for A", repository.id)
    finally:
        rig.mock.remove(token_for_a)

    # A session on A, which M answers from that partition, holding none of B.
    refused("through a session on A", rig.repos["A"].id, at_machine=False)


def test_concurrent_writes(rig):
    """Every USER1 container commits to a branch of its own of A and pushes
    through its socket at once. A host clone straight from U, and a clone
    through another container's socket, see every branch's file."""
    repository = rig.repos["A"]
    writers = users(rig, USER1)
    clones, _ = at_once(writers, lambda c: c.clone(repository, "a-write"))
    for container, outcome in zip(writers, clones):
        ok(container, outcome)

    branches = {}
    for container in writers:
        name = f"uds-{container.name}"
        content = f"written by {container.name}\n".encode() + os.urandom(256 * 1024)
        write_tree(
            container.host_path("/c/work/a-write"), {f"branches/{name}.bin": content}
        )
        branches[name] = (container, content)

    def push(container):
        name = f"uds-{container.name}"
        work = "/c/work/a-write"
        return container.exec(
            "sh",
            "-c",
            "set -e; "
            f"lore --repository {work} branch create {name}; "
            f"lore --repository {work} stage {work}/branches/{name}.bin; "
            f"lore --repository {work} commit 'branch {name}'; "
            f"lore --repository {work} push",
        )

    outcomes, wall = at_once(writers, push)
    report("branch push to A", outcomes, wall)
    for container, outcome in zip(writers, outcomes):
        ok(container, outcome)

    reader = users(rig, USER2)[0]
    for name, (container, content) in branches.items():
        host_clone = rig.root / "host" / f"{name}-from-upstream"
        repository.host.clone(path=str(host_clone), branch=name)
        assert (host_clone / "branches" / f"{name}.bin").read_bytes() == content
        assert_tree(host_clone, repository.files)

        ok(reader, reader.clone(repository, name, "--branch", name))
        through_socket = reader.host_path(f"/c/work/{name}")
        assert (through_socket / "branches" / f"{name}.bin").read_bytes() == content


def test_machine_server_restart(rig):
    """M restarts while every container's per-user service runs. The services
    keep running across the restart, and the same service processes then sync
    a new revision and clone again through the new socket."""
    repository = rig.repos["A"]
    outcomes, _ = at_once(rig.containers, lambda c: c.clone(repository, "a-restart"))
    for container, outcome in zip(rig.containers, outcomes):
        ok(container, outcome)
    services = {container.name: container.lore_pids() for container in rig.containers}
    for container in rig.containers:
        assert services[container.name], f"{container.name} runs no service"

    rig.machine.restart()

    content = os.urandom(64 * 1024)
    write_tree(Path(repository.host.path), {"after-restart.bin": content})
    repository.host.stage(scan=True)
    repository.host.commit("after the restart")
    repository.host.push()

    def again(container):
        synced = container.lore("--repository", "/c/work/a-restart", "sync")
        if synced.code != 0:
            return synced
        cloned = container.clone(repository, "a-after-restart")
        cloned.seconds += synced.seconds
        return cloned

    outcomes, wall = at_once(rig.containers, again)
    report("sync and clone of A after M restarted", outcomes, wall)
    for container, outcome in zip(rig.containers, outcomes):
        ok(container, outcome)
        for directory in ("a-restart", "a-after-restart"):
            synced = container.host_path(f"/c/work/{directory}/after-restart.bin")
            assert synced.read_bytes() == content, f"{container.name} {directory}"
        assert container.lore_pids() == services[container.name], (
            f"{container.name} carried on in another service than the one M's "
            "restart cut off"
        )
    print(f"{len(services)} services kept across M's restart")


def test_container_without_the_socket_variable(rig):
    """A container whose socket is mounted but not named in LORE_UDS_SOCKET is
    refused a `lore-unix://` clone, with an error that names the variable."""
    refused = rig.bare.clone(rig.repos["A"], "a")
    assert refused.code != 0, f"cloned without LORE_UDS_SOCKET:\n{refused.output}"
    assert "LORE_UDS_SOCKET" in refused.output, refused.output
    assert not rig.bare.host_path("/c/work/a/readme.txt").exists()
    print(f"clone without LORE_UDS_SOCKET: {refused.output.strip()[:300]}")
