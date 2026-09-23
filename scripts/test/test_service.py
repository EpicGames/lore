# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import platform
import signal
import subprocess
from pathlib import Path

import pytest

from lore import Lore
from service_util import (
    LORE_NO_SERVICE_EXECUTABLE_MESSAGE,
    LORE_NO_SERVICE_MESSAGE,
    LORE_SERVICE_ENVIRONMENT,
    LORE_SERVICE_RUNNING_MESSAGE,
    SERVICE_UNAVAILABLE,
    stop_lore_service,
)

logger = logging.getLogger(__name__)

# Commands run at once with no service running. Each of them starts one, only
# one of those can hold the socket, and the rest have to reach the one that does.
CONCURRENT_COMMAND_COUNT = 5


def service_command_environment(repo: Lore) -> dict[str, str]:
    """The environment a command run outside the `Lore` wrapper needs to reach
    the service, matching what the wrapper sets for its own commands."""
    return repo.sandboxed_env(**LORE_SERVICE_ENVIRONMENT)


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
def test_a_command_starts_the_service_when_none_is_running(
    new_lore_repo, stops_background_services, global_dir_name
):
    """A command in service mode that finds nothing listening starts a service
    and runs on it, rather than failing outright.

    Only the start is asserted. Whether state written through a service can be
    read back afterwards depends on when that service flushes it, which is a
    question about the service's guarantees rather than about starting one;
    `test_service_call` covers a write and a read against a service held open
    across both.
    """
    # Built without service mode, so nothing is listening when the command below
    # runs: the fixture cleared any service, and creating the repository started
    # none.
    repo: Lore = new_lore_repo()

    command = subprocess.run(
        [repo.lore_executable_path, "--repository", repo.path, "status"],
        capture_output=True,
        text=True,
        env=service_command_environment(repo),
        cwd=repo.path,
    )
    output = command.stdout + command.stderr

    assert command.returncode == 0, (
        f"a command that finds no service must start one and run: {output}"
    )
    # In service mode the command has no local path to fall back on, so output
    # at all means it reached a service.
    assert "On branch" in output, f"the command must report the repository: {output}"

    # The service it started outlives it, which is what stopping one here reports.
    assert LORE_NO_SERVICE_MESSAGE not in stop_lore_service(
        repo.lore_executable_path, global_dir_name
    ), "the command must leave the service it started running"


@pytest.mark.smoke
def test_relaying_without_an_executable_or_running_service_fails(
    new_lore_repo, stops_background_services, global_dir_name
):
    """A command that tries to relay without an executable set and without a
    running service fails with ServiceUnavailable.

    Commands can use a service that is already running, but starting one when
    none is running requires an executable. Without either, the command cannot
    proceed.
    """
    repo: Lore = new_lore_repo()

    env = repo.sandboxed_env(**LORE_SERVICE_ENVIRONMENT)
    # Left unnamed on purpose, which is the whole of what this test is about, so
    # the name `sandboxed_env` supplies for the other tests is dropped again.
    env.pop("LORE_SERVICE_EXECUTABLE", None)

    command = subprocess.run(
        [repo.lore_executable_path, "--repository", repo.path, "status"],
        capture_output=True,
        text=True,
        env=env,
        cwd=repo.path,
    )
    output = command.stdout + command.stderr

    assert command.returncode == SERVICE_UNAVAILABLE, (
        f"the command must fail with ServiceUnavailable: {output}"
    )

    # Nothing to stop proves nothing was started.
    assert LORE_NO_SERVICE_MESSAGE in stop_lore_service(
        repo.lore_executable_path, global_dir_name
    ), "the command must not have started a service"


@pytest.mark.smoke
def test_relaying_without_an_executable_uses_a_running_service(
    new_lore_repo, background_lore_service
):
    """A command that tries to relay without an executable set can still use a
    service that is already running.

    This allows a user to start a service manually with `lore service run` and
    have commands use it without configuring an executable.
    """
    repo: Lore = new_lore_repo()

    env = repo.sandboxed_env(**LORE_SERVICE_ENVIRONMENT)
    # Left unnamed on purpose: the running service is enough.
    env.pop("LORE_SERVICE_EXECUTABLE", None)

    command = subprocess.run(
        [repo.lore_executable_path, "--repository", repo.path, "status"],
        capture_output=True,
        text=True,
        env=env,
        cwd=repo.path,
    )
    output = command.stdout + command.stderr

    assert command.returncode == 0, (
        f"the command must succeed using the running service: {output}"
    )
    assert "On branch" in output, f"the command must report the repository: {output}"


@pytest.mark.smoke
def test_turning_relaying_on_without_an_executable_reports_it(new_lore_repo):
    """Turning relaying on with no executable named warns that commands will fail
    if no service is already running.

    Commands can use a running service without an executable, but cannot start
    one. The warning is reported here rather than on each command: the decision
    happens before a command has anywhere to send a message, and this is the
    point at which the advice can be acted on.

    Run through subprocess rather than `repo.run()` because the test environment
    always names an executable, and this test is specifically about having none.
    """
    repo: Lore = new_lore_repo()

    # An environment with no executable set, which is the whole point of this test.
    env = repo.sandboxed_env()
    env.pop("LORE_SERVICE_EXECUTABLE", None)

    def run_service_command(*args: str) -> str:
        result = subprocess.run(
            [repo.lore_executable_path, "service", *args],
            capture_output=True,
            text=True,
            env=env,
        )
        return result.stdout + result.stderr

    output = run_service_command("set-use-automatically", "true")
    assert LORE_NO_SERVICE_EXECUTABLE_MESSAGE in output, (
        f"turning relaying on with no executable named must warn: {output}"
    )

    # With one named, commands can start a service when needed.
    named = run_service_command("set-executable", repo.lore_executable_path)
    assert LORE_NO_SERVICE_EXECUTABLE_MESSAGE not in named, (
        f"naming the executable must silence the warning: {named}"
    )

    # Clearing it brings back the warning about needing a running service.
    cleared = run_service_command("set-executable", "")
    assert LORE_NO_SERVICE_EXECUTABLE_MESSAGE in cleared, (
        f"clearing the executable with relaying on must warn again: {cleared}"
    )


@pytest.mark.smoke
def test_commands_run_at_once_all_reach_one_service(
    new_lore_repo, stops_background_services
):
    """Commands that all find no service all start one, and only one of those
    can hold the socket. The callers whose service lost it must run on the one
    that won rather than fail."""
    # Set the repository up without service mode, so that nothing is listening
    # when the commands below start.
    repo: Lore = new_lore_repo()

    file_name = "test.uasset"
    with repo.open_file(file_name, "w+b") as output_file:
        output_file.write(os.urandom(30))
    repo.stage(scan=True)

    environment = service_command_environment(repo)
    command_args = [repo.lore_executable_path, "--repository", repo.path, "status"]
    logger.info(
        "Executing %d Lore commands at once: %s", CONCURRENT_COMMAND_COUNT, command_args
    )
    commands = [
        subprocess.Popen(
            command_args,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=environment,
            cwd=repo.path,
        )
        for _ in range(CONCURRENT_COMMAND_COUNT)
    ]

    outputs = [command.communicate()[0] for command in commands]

    for index, command in enumerate(commands):
        assert command.returncode == 0, (
            f"Command {index} of {CONCURRENT_COMMAND_COUNT} failed with "
            f"{command.returncode}: {outputs[index]}"
        )
        assert "A " + file_name in map(
            lambda line: line.strip(" "), outputs[index].splitlines()
        ), f"Command {index} must have reached a service: {outputs[index]}"


@pytest.mark.smoke
def test_start_reports_a_reachable_service_whether_or_not_it_started_one(
    new_lore_repo, stops_background_services
):
    """`service start` asks for a service to be running. It starts one when none
    is, and reports the one that is running when one already is."""
    repo: Lore = new_lore_repo()

    started = repo.run(["service", "start"])
    assert LORE_SERVICE_RUNNING_MESSAGE in started, (
        f"The first start must report a running service: {started}"
    )

    already_running = repo.run(["service", "start"])
    assert LORE_SERVICE_RUNNING_MESSAGE in already_running, (
        f"A second start must report the service already running: {already_running}"
    )


@pytest.mark.smoke
def test_stop_ends_the_service_and_reports_when_there_is_none(
    new_lore_repo, stops_background_services
):
    """`service stop` stops the running service. With none running it reports
    that, rather than failing or starting one to stop."""
    repo: Lore = new_lore_repo()
    repo.run(["service", "start"])

    stopped = repo.run(["service", "stop"])
    assert LORE_NO_SERVICE_MESSAGE not in stopped, (
        f"The stop must have found the running service: {stopped}"
    )

    # No wait in between: a stop returns only once the socket is free, so the
    # next command sees no service rather than one that is still shutting down.
    with_none_running = repo.run(["service", "stop"])
    assert LORE_NO_SERVICE_MESSAGE in with_none_running, (
        f"A stop with no service running must report that: {with_none_running}"
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
        base_env=source.base_env,
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
@pytest.mark.skipif(
    platform.system() not in ("Linux", "Darwin"),
    reason="POSIX termination signals",
)
@pytest.mark.parametrize("termination_signal", [signal.SIGTERM, signal.SIGINT])
def test_the_service_shuts_down_cleanly_on_a_signal(
    lore_service_runner, tmp_path, termination_signal
):
    """A termination signal stops the service rather than ending it outright, so
    it releases its socket and exits 0.

    The handlers are registered before the socket is bound, which is what makes
    this hold for the whole time the service is reachable rather than only once
    it has reached its wait.
    """
    service_directory = tmp_path / f"service_{termination_signal}"
    service_directory.mkdir()
    service = lore_service_runner.start(str(service_directory))

    service.send_signal(termination_signal)
    try:
        code = service.wait(timeout=30)
    except subprocess.TimeoutExpired:
        service.kill()
        pytest.fail(f"the service did not exit on {termination_signal!r}")

    assert code == 0, (
        f"the service must stop rather than be killed by {termination_signal!r}, "
        f"got exit code {code}"
    )


@pytest.mark.smoke
def test_stops_run_at_once_all_report_success(new_lore_repo, stops_background_services):
    """Two stops racing one live service both report success.

    The one whose request finds the service already gone must still exit 0: it
    asked for no service to be running, and none is.
    """
    repo: Lore = new_lore_repo()

    env = repo.sandboxed_env()

    # Repeated, because which stop loses the race is timing rather than
    # something the test can choose.
    for attempt in range(3):
        started = repo.run(["service", "start"])
        assert LORE_SERVICE_RUNNING_MESSAGE in started, (
            f"attempt {attempt} must have a service to stop: {started}"
        )

        stops = [
            subprocess.Popen(
                [repo.lore_executable_path, "service", "stop"],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                env=env,
            )
            for _ in range(2)
        ]
        for index, stop in enumerate(stops):
            output = stop.communicate(timeout=30)[0]
            assert stop.returncode == 0, (
                f"stop {index} of attempt {attempt} failed with "
                f"{stop.returncode}: {output}"
            )


@pytest.mark.smoke
def test_the_setters_write_the_settings_the_config_reference_documents(
    new_lore_repo, global_dir_name
):
    """Both settings reach the shared config under the documented names, and
    clearing them removes them rather than storing a value that reads as unset.

    Asserted on the file rather than on what the setter printed: a setter that
    reports correctly while writing the wrong key passes otherwise, and the file
    is what the next process reads.
    """
    repo: Lore = new_lore_repo()
    config = Path(global_dir_name) / "config" / "config.toml"

    repo.run(["service", "set-executable", repo.lore_executable_path])
    repo.run(["service", "set-use-automatically", "true"])

    written = config.read_text(encoding="utf-8")
    assert "[service]" in written, written
    assert "executable = " in written, written
    assert "use_automatically = true" in written, written

    repo.run(["service", "set-executable", ""])
    repo.run(["service", "set-use-automatically", "false"])

    cleared = config.read_text(encoding="utf-8")
    assert "executable = " not in cleared, cleared
    assert "use_automatically" not in cleared, cleared


@pytest.mark.smoke
def test_a_command_that_reaches_no_service_reports_service_unavailable(
    new_lore_repo, stops_background_services, global_dir_name
):
    """The exit code an integrator driving the CLI branches on: the command never
    ran, because no service could be reached or started.

    Distinct from the command itself failing, which is the whole point of the
    code. Shown without a real service by naming an executable that cannot be
    spawned, and controlled against the same command forced to run locally.
    """
    repo: Lore = new_lore_repo()

    env = repo.sandboxed_env(
        **LORE_SERVICE_ENVIRONMENT,
        LORE_SERVICE_EXECUTABLE=str(Path(global_dir_name) / "no-such-lore-binary"),
    )

    command_args = [repo.lore_executable_path, "--repository", repo.path, "status"]
    routed = subprocess.run(
        command_args,
        capture_output=True,
        text=True,
        env=env,
        cwd=repo.path,
        check=False,
    )
    output = routed.stdout + routed.stderr
    assert routed.returncode == SERVICE_UNAVAILABLE, (
        f"expected the service-unavailable code {SERVICE_UNAVAILABLE}, got "
        f"{routed.returncode}: {output}"
    )

    # Control: the same command forced to run where it was called succeeds, so
    # the failure above was the routing rather than the command.
    local_env = dict(env)
    local_env["LORE_USE_SERVICE"] = "0"
    local = subprocess.run(
        command_args,
        capture_output=True,
        text=True,
        env=local_env,
        cwd=repo.path,
        check=False,
    )
    assert local.returncode == 0, (
        f"forcing local execution must not reach for the service: "
        f"{local.stdout}{local.stderr}"
    )
