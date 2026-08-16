# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import platform
import shutil
import signal
import subprocess

import pytest

from error_types import ServiceCallError
from lore import Lore
from service_util import LORE_SERVICE_ENVIRONMENT
from service_util import service_supported

logger = logging.getLogger(__name__)

# `LoreError::ServiceUnavailable` / `LORE_ERROR_CODE_SERVICE_UNAVAILABLE`: what
# a call routed to the service reports when none could be reached or started.
SERVICE_UNAVAILABLE = 50


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


def tmp_missing_executable(global_dir):
    """A path that names no binary, for the cases that need a configured
    executable which cannot start a service."""
    return os.path.join(global_dir, "no-such-lore-binary")


@pytest.fixture
def stopped_service(lore_executable_path, global_dir_name):
    """Leaves no service process behind.

    The socket is per user rather than per test, so a service surviving one
    test would be picked up by the next one.
    """
    yield
    run_lore(lore_executable_path, ["service", "stop"], global_dir_name)


@pytest.mark.smoke
@pytest.mark.skip(reason="Unknown issue specifically running in CI for OSS")
def test_service_down(new_lore_repo):
    with pytest.raises(ServiceCallError):
        new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_start_stop(lore_executable_path, global_dir_name, stopped_service):
    # A service is only ever started from an executable someone named, so the
    # one to launch is configured before anything is started.
    configure = run_lore(
        lore_executable_path,
        ["service", "set-executable", str(lore_executable_path)],
        global_dir_name,
    )
    assert configure.returncode == 0, configure.stdout + configure.stderr

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
def test_service_set_executable(lore_executable_path, global_dir_name):
    config_path = os.path.join(global_dir_name, "config", "config.toml")

    executable = (
        "C:\\lore\\lore.exe" if platform.system() == "Windows" else "/opt/lore/lore"
    )
    set_executable = run_lore(
        lore_executable_path,
        ["service", "set-executable", executable],
        global_dir_name,
    )
    assert set_executable.returncode == 0, set_executable.stdout + set_executable.stderr
    with open(config_path, encoding="utf-8") as config_file:
        assert "service_executable" in config_file.read()

    # An empty value clears the setting rather than storing a name that
    # resolves to no command at all.
    clear = run_lore(
        lore_executable_path, ["service", "set-executable", ""], global_dir_name
    )
    assert clear.returncode == 0, clear.stdout + clear.stderr
    with open(config_path, encoding="utf-8") as config_file:
        assert "service_executable" not in config_file.read()


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_routing_needs_an_executable_as_well_as_the_setting(
    lore_executable_path, global_dir_name, new_lore_repo, stopped_service
):
    """`use_service_automatically` on its own does not route.

    Routing sends a command to a service and starts one if none is listening,
    and a service is only ever started from an executable someone named. With
    the setting on and none configured there would be nothing to start, so the
    setting alone leaves commands running locally rather than failing every one
    of them with service-unavailable.
    """
    repo: Lore = new_lore_repo()
    run_lore(lore_executable_path, ["service", "stop"], global_dir_name)

    enable = run_lore(
        lore_executable_path,
        ["service", "set-use-automatically", "true"],
        global_dir_name,
    )
    assert enable.returncode == 0, enable.stdout + enable.stderr

    local = run_lore(
        lore_executable_path, ["--repository", repo.path, "status"], global_dir_name
    )
    assert local.returncode == 0, (
        "with no executable configured the command should run locally: "
        f"{local.stdout}{local.stderr}"
    )


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_config_setting_routes_to_service(
    lore_executable_path, global_dir_name, new_lore_repo, stopped_service
):
    """With both settings, commands are routed rather than run locally.

    Routing is proven without starting a real daemon: the configured executable
    is a path that does not exist, so a routed command reaches the spawn and
    fails there, while the same command forced to run locally succeeds.
    """
    repo: Lore = new_lore_repo()

    # No service must be listening, so a routed command takes the auto-start
    # path where the missing executable fails.
    run_lore(lore_executable_path, ["service", "stop"], global_dir_name)

    enable = run_lore(
        lore_executable_path,
        ["service", "set-use-automatically", "true"],
        global_dir_name,
    )
    assert enable.returncode == 0, enable.stdout + enable.stderr

    missing = str(tmp_missing_executable(global_dir_name))
    configure = run_lore(
        lore_executable_path, ["service", "set-executable", missing], global_dir_name
    )
    assert configure.returncode == 0, configure.stdout + configure.stderr

    status_args = ["--repository", repo.path, "status"]
    routed = run_lore(lore_executable_path, status_args, global_dir_name)

    # The code an integrator branches on: a routed call that never ran because
    # no service could be reached or started reports service-unavailable, not a
    # generic failure it cannot tell from the command's own errors.
    assert routed.returncode == SERVICE_UNAVAILABLE, (
        f"expected the service-unavailable code {SERVICE_UNAVAILABLE}, "
        f"got {routed.returncode}: {routed.stdout}{routed.stderr}"
    )
    assert "spawning" in (routed.stdout + routed.stderr), (
        "the failure should name the executable it could not spawn, got: "
        f"{routed.stdout}{routed.stderr}"
    )

    # Control: the same command forced to run locally does not try to reach the
    # service, so the failure above was the routing decision.
    environment = os.environ.copy()
    environment["LORE_GLOBAL_PATH"] = global_dir_name
    environment["LORE_USE_SERVICE"] = "0"
    local = subprocess.run(
        [lore_executable_path, *status_args],
        capture_output=True,
        text=True,
        env=environment,
    )
    assert local.returncode == 0, (
        f"forcing local execution should not route to the service: "
        f"{local.stdout}{local.stderr}"
    )


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_config_settings_start_and_use_a_service(
    lore_executable_path, global_dir_name, new_lore_repo, stopped_service
):
    """With both settings, a command starts a service and runs there — the whole
    automatic path, not a stand-in for it.

    That the command ran in a service rather than locally is shown by taking the
    service away: with the configured executable pointed at something that
    cannot start, the same command keeps working while a service is listening
    and reports service-unavailable once it is stopped. A command running
    locally would be unaffected by either.

    The suite gives itself a service socket of its own (see `conftest`), which
    is what makes starting a real service here safe: it neither meets nor stops
    whatever service the developer has running.
    """
    # Created before the setting is on, so this runs in the CLI process and
    # does not depend on what the rest of the test is proving.
    repo: Lore = new_lore_repo()
    run_lore(lore_executable_path, ["service", "stop"], global_dir_name)

    enable = run_lore(
        lore_executable_path,
        ["service", "set-use-automatically", "true"],
        global_dir_name,
    )
    assert enable.returncode == 0, enable.stdout + enable.stderr
    configure = run_lore(
        lore_executable_path,
        ["service", "set-executable", str(lore_executable_path)],
        global_dir_name,
    )
    assert configure.returncode == 0, configure.stdout + configure.stderr

    status_args = ["--repository", repo.path, "status"]

    # Nothing is listening, so this both starts a service and runs there.
    started = run_lore(lore_executable_path, status_args, global_dir_name)
    assert started.returncode == 0, started.stdout + started.stderr

    # Routing stays on, but nothing startable is configured any more. The
    # command still succeeds, so a service really is listening.
    missing = str(tmp_missing_executable(global_dir_name))
    reconfigure = run_lore(
        lore_executable_path, ["service", "set-executable", missing], global_dir_name
    )
    assert reconfigure.returncode == 0, reconfigure.stdout + reconfigure.stderr

    reused = run_lore(lore_executable_path, status_args, global_dir_name)
    assert reused.returncode == 0, (
        "the command should have reached the service started earlier: "
        f"{reused.stdout}{reused.stderr}"
    )

    # Take the service away and the same command has nowhere to run, which a
    # command running locally would not care about.
    stop = run_lore(lore_executable_path, ["service", "stop"], global_dir_name)
    assert stop.returncode == 0, stop.stdout + stop.stderr

    without = run_lore(lore_executable_path, status_args, global_dir_name)
    assert without.returncode == SERVICE_UNAVAILABLE, (
        f"expected {SERVICE_UNAVAILABLE} with no service to run in, "
        f"got {without.returncode}: {without.stdout}{without.stderr}"
    )


@pytest.mark.smoke
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
def test_service_resolves_relative_paths_against_caller(
    new_lore_repo, lore_service_runner, tmp_path
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
    lore_service_runner.start(str(service_directory))

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
def test_service_starts_on_demand(
    lore_executable_path, global_dir_name, new_lore_repo, stopped_service
):
    """A call routed to the service starts one when none is running.

    This replaces an older test asserting the opposite: before automatic
    start-up, the same call failed with a connection error.
    """
    # The executable to start is configured before the first routed call, which
    # is the repository creation below: a service is only ever started from one
    # that was named.
    configure = run_lore(
        lore_executable_path,
        ["service", "set-executable", str(lore_executable_path)],
        global_dir_name,
    )
    assert configure.returncode == 0, configure.stdout + configure.stderr

    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())

    file_name = "test.uasset"
    with repo.open_file(file_name, "w+b") as output_file:
        output_file.write(os.urandom(30))

    repo.stage(scan=True)

    assert "A " + file_name in map(
        lambda line: line.strip(" "), repo.status().splitlines()
    )


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_start_launches_a_named_executable(
    lore_executable_path, global_dir_name, tmp_path, stopped_service
):
    """`service start --executable` launches the binary it is given.

    A binary not named `lore` stands in for an application Lore is embedded in.
    Nothing is inferred from the running executable — not even for the CLI — so
    with none named and none configured there is nothing to launch, and naming
    one is how any caller starts a service.
    """
    refusal = "no executable to launch"

    binary_name = "notlore.exe" if platform.system() == "Windows" else "notlore"
    not_lore = tmp_path / binary_name
    shutil.copy(lore_executable_path, not_lore)
    not_lore.chmod(0o755)

    refused = run_lore(str(not_lore), ["service", "start"], global_dir_name)
    assert refused.returncode != 0, "nothing names an executable to launch"
    assert refusal in (refused.stdout + refused.stderr), (
        f"the refusal should say what is missing: {refused.stdout}{refused.stderr}"
    )

    # The same refusal for the CLI itself: being named `lore` is not what makes
    # an executable launchable.
    refused_cli = run_lore(lore_executable_path, ["service", "start"], global_dir_name)
    assert refused_cli.returncode != 0, (
        "the CLI must not infer itself as the service executable: "
        f"{refused_cli.stdout}{refused_cli.stderr}"
    )
    assert refusal in (refused_cli.stdout + refused_cli.stderr), (
        f"{refused_cli.stdout}{refused_cli.stderr}"
    )

    started = run_lore(
        str(not_lore),
        ["service", "start", "--executable", str(lore_executable_path)],
        global_dir_name,
    )
    assert started.returncode == 0, started.stdout + started.stderr

    # The service it launched is a real one, so stopping it succeeds.
    stop = run_lore(lore_executable_path, ["service", "stop"], global_dir_name)
    assert stop.returncode == 0, stop.stdout + stop.stderr


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    platform.system() not in ("Linux", "Darwin"),
    reason="POSIX termination signals",
)
@pytest.mark.parametrize("sig", [signal.SIGTERM, signal.SIGINT])
def test_service_shuts_down_gracefully_on_signal(lore_service_runner, tmp_path, sig):
    """A termination signal stops the service cleanly, exiting with code 0."""
    service_directory = tmp_path / f"service_{sig}"
    service_directory.mkdir()
    service_process = lore_service_runner.start(str(service_directory))

    service_process.send_signal(sig)
    try:
        code = service_process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        service_process.kill()
        pytest.fail(f"the service did not exit on {sig!r}")
    assert code == 0, f"the service should exit cleanly on {sig!r}, got {code}"


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_concurrent_service_stop_all_succeed(
    lore_executable_path, global_dir_name, stopped_service
):
    """Two stops racing one live service both report success.

    The one whose send finds the service already gone must still exit 0 rather
    than fail on the closed connection.
    """
    env = os.environ.copy()
    env["LORE_GLOBAL_PATH"] = global_dir_name

    configure = run_lore(
        lore_executable_path,
        ["service", "set-executable", str(lore_executable_path)],
        global_dir_name,
    )
    assert configure.returncode == 0, configure.stdout + configure.stderr

    for _ in range(3):
        start = run_lore(lore_executable_path, ["service", "start"], global_dir_name)
        assert start.returncode == 0, start.stdout + start.stderr

        stops = [
            subprocess.Popen(
                [lore_executable_path, "service", "stop"],
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            for _ in range(2)
        ]
        for stop in stops:
            out, err = stop.communicate(timeout=15)
            assert stop.returncode == 0, f"a concurrent stop failed: {out}{err}"
