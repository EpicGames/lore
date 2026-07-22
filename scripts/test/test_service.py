# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import platform
import shutil
import subprocess

import pytest

from lore import Lore

logger = logging.getLogger(__name__)

LORE_SERVICE_ENVIRONMENT = {"LORE_USE_SERVICE": "1"}


def service_supported():
    return platform.system() in ("Windows", "Linux", "Darwin")


def run_lore(lore_executable_path, args, global_dir):
    """Runs the Lore CLI with an isolated global config directory."""
    environment = os.environ.copy()
    environment["LORE_GLOBAL_PATH"] = global_dir
    return subprocess.run(
        [lore_executable_path, *args],
        capture_output=True,
        text=True,
        env=environment,
    )


@pytest.fixture
def stopped_service(lore_executable_path, global_dir_name):
    """Leaves no service process behind.

    The socket is per user rather than per test, so a service surviving one
    test would be picked up by the next one.
    """
    yield
    run_lore(lore_executable_path, ["service", "stop"], global_dir_name)


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_start_stop(lore_executable_path, global_dir_name, stopped_service):
    start = run_lore(lore_executable_path, ["service", "start"], global_dir_name)
    assert start.returncode == 0, start.stdout + start.stderr

    # Starting again is a no-op rather than an error, because a service is
    # already listening.
    again = run_lore(lore_executable_path, ["service", "start"], global_dir_name)
    assert again.returncode == 0, again.stdout + again.stderr

    stop = run_lore(lore_executable_path, ["service", "stop"], global_dir_name)
    assert stop.returncode == 0, stop.stdout + stop.stderr

    # Stopping when nothing is running is also a no-op.
    stop_again = run_lore(lore_executable_path, ["service", "stop"], global_dir_name)
    assert stop_again.returncode == 0, stop_again.stdout + stop_again.stderr


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_set_use_automatically(lore_executable_path, global_dir_name):
    config_path = os.path.join(global_dir_name, "config", "config.toml")

    enable = run_lore(
        lore_executable_path,
        ["service", "set-use-automatically", "true"],
        global_dir_name,
    )
    assert enable.returncode == 0, enable.stdout + enable.stderr
    with open(config_path, encoding="utf-8") as config_file:
        assert "use_service_automatically = true" in config_file.read()

    disable = run_lore(
        lore_executable_path,
        ["service", "set-use-automatically", "false"],
        global_dir_name,
    )
    assert disable.returncode == 0, disable.stdout + disable.stderr
    with open(config_path, encoding="utf-8") as config_file:
        assert "use_service_automatically" not in config_file.read()


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_config_setting_routes_to_service(
    lore_executable_path, global_dir_name, tmp_path, stopped_service
):
    """The use_service_automatically config setting routes commands through the
    service, without the LORE_USE_SERVICE override.

    Everything runs against the test's isolated LORE_GLOBAL_PATH, so the
    developer's real global config is never read or written. Routing is proven
    without starting a real daemon: a binary not named `lore` refuses to
    auto-start the service, so a command that tries to route fails with that
    refusal, while the same binary forced to run locally does not.
    """
    refusal = "start the Lore service automatically"

    # No service must be listening, so a routed command takes the auto-start
    # path where a non-`lore` binary refuses.
    run_lore(lore_executable_path, ["service", "stop"], global_dir_name)

    enable = run_lore(
        lore_executable_path,
        ["service", "set-use-automatically", "true"],
        global_dir_name,
    )
    assert enable.returncode == 0, enable.stdout + enable.stderr

    binary_name = "notlore.exe" if platform.system() == "Windows" else "notlore"
    not_lore = tmp_path / binary_name
    shutil.copy(lore_executable_path, not_lore)
    not_lore.chmod(0o755)

    env = os.environ.copy()
    env["LORE_GLOBAL_PATH"] = global_dir_name

    routed = subprocess.run(
        [str(not_lore), "status"],
        capture_output=True,
        text=True,
        cwd=str(tmp_path),
        env=env,
    )
    assert refusal in (routed.stdout + routed.stderr), (
        "the config setting should have routed the command to the service, "
        f"got: {routed.stdout}{routed.stderr}"
    )

    # Control: the same binary forced to run locally does not try to reach the
    # service, so the failure above was the routing decision, not the rename.
    local_env = env.copy()
    local_env["LORE_USE_SERVICE"] = "0"
    local = subprocess.run(
        [str(not_lore), "status"],
        capture_output=True,
        text=True,
        cwd=str(tmp_path),
        env=local_env,
    )
    assert refusal not in (local.stdout + local.stderr), (
        f"forcing local execution should not route to the service: "
        f"{local.stdout}{local.stderr}"
    )


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_call(new_lore_repo, background_lore_service):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())

    # Add a single file so status has output
    file_name = "test.uasset"
    with repo.open_file(file_name, "w+b") as output_file:
        output_file.write(os.urandom(30))

    repo.stage(scan=True)

    status_output = repo.status()

    # Assert that single file is added
    assert "A " + file_name in map(
        lambda line: line.strip(" "), status_output.splitlines()
    )


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_resolves_relative_paths_against_caller(
    new_lore_repo, lore_service_in_directory, tmp_path
):
    """Relative paths belong to the directory the command was run in.

    The service resolves them, and its own working directory is unrelated to
    the caller's, so a service started elsewhere must not pull them towards
    itself. Every other service test passes an absolute repository path, which
    cannot catch this.
    """
    # Start the service in a directory unrelated to where the commands run, so
    # that a relative path resolved there rather than at the caller would show.
    service_directory = tmp_path / "service_elsewhere"
    caller_directory = tmp_path / "caller"
    service_directory.mkdir()
    caller_directory.mkdir()
    lore_service_in_directory(service_directory)

    # Seed a remote to clone from. Routed through the service like the rest,
    # but against the repository's own absolute path, so unaffected by the
    # service's directory.
    source: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    with source.open_file("seed.txt", "w+") as seed_file:
        seed_file.write("seed\n")
    source.stage(scan=True, offline=True)
    source.commit("Seed", offline=True)
    source.push()

    # Clone to a relative path from the caller's directory. It must land there,
    # not under the service's directory.
    clone_name = "relative_clone"
    source.run(
        ["repository", "clone", source.remote_path, clone_name],
        cwd=str(caller_directory),
        use_os_dir=True,
    )

    clone_path = caller_directory / clone_name
    assert (clone_path / ".lore").is_dir(), (
        f"Clone must land under the caller's directory, not the service's. "
        f"{caller_directory} contains {list(caller_directory.iterdir())}"
    )
    assert not (service_directory / clone_name).exists(), (
        f"Clone must not land under the service's directory. "
        f"{service_directory} contains {list(service_directory.iterdir())}"
    )

    # Stage a relative path from inside the clone.
    clone = Lore(
        lore_executable_path=source.lore_executable_path,
        path=str(clone_path),
        name=clone_name,
        global_dir=source.global_dir,
        environment_vars=LORE_SERVICE_ENVIRONMENT.copy(),
        remote_url=source.remote,
        remote_path=source.remote_path,
        create_repo=False,
    )
    file_name = "added.uasset"
    (clone_path / file_name).write_bytes(os.urandom(30))
    clone.stage(file_name, relative_paths=True)

    status_output = clone.status()
    assert "A " + file_name in map(
        lambda line: line.strip(" "), status_output.splitlines()
    ), f"Staged file should show as added: {status_output}"


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_starts_on_demand(new_lore_repo, stopped_service):
    """A call routed to the service starts one when none is running.

    This replaces an older test asserting the opposite: before automatic
    start-up, the same call failed with a connection error.
    """
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())

    file_name = "test.uasset"
    with repo.open_file(file_name, "w+b") as output_file:
        output_file.write(os.urandom(30))

    repo.stage(scan=True)

    assert "A " + file_name in map(
        lambda line: line.strip(" "), repo.status().splitlines()
    )
