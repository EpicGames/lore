# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import platform
import subprocess

logger = logging.getLogger(__name__)


LORE_SERVICE_ENVIRONMENT = {"LORE_USE_SERVICE": "1"}

# `LoreError::ServiceUnavailable`: what a command carried out by the service
# reports when none could be reached or started, so it is distinct from the
# command itself having run and failed. Paired with the value in
# `lore-revision/src/interface.rs` and has to change with it.
SERVICE_UNAVAILABLE = 32


def name_service_executable(
    env: dict[str, str], lore_executable_path: str
) -> dict[str, str]:
    """Names the executable under test as the service.

    `service start` needs an executable to spawn, and commands relaying to a
    service need an executable to start one when none is running. Setting it
    here rather than in each test ensures no test can leave it out, and that
    the service serving a test is always the build being tested rather than
    whichever Lore is installed on the machine running the suite.

    A test that names one itself keeps it.
    """
    if not env.get("LORE_SERVICE_EXECUTABLE"):
        env["LORE_SERVICE_EXECUTABLE"] = lore_executable_path
    return env


# Names the socket a service listens on. Set once per test run so the suite gets
# a service of its own: without it every service on a machine answers on the same
# per-user socket, so a run would stop a service the developer is using, and two
# runs could not proceed at once.
LORE_SERVICE_SOCKET_VAR = "LORE_SERVICE_SOCKET"

# Lines the client prints about the service. They are matched rather than
# parsed, so they are kept here next to each other: each one pairs with a string
# in the Rust sources and has to be changed with it.
#
# `lore service run` prints this once it has bound its socket.
LORE_SERVICE_LISTENING_MESSAGE = "Lore service listening"
# `lore service stop` prints this when there was no service to stop.
LORE_NO_SERVICE_MESSAGE = "No Lore service is running"
# `lore service start` prints this once a service is reachable.
LORE_SERVICE_RUNNING_MESSAGE = "Lore service is running"
# Printed by the `service` setters when relaying is on but no executable is set.
# Commands can use a running service, but cannot start one without an executable.
LORE_NO_SERVICE_EXECUTABLE_MESSAGE = "No service executable is set"


def service_supported():
    return platform.system() in ("Windows", "Linux", "Darwin")


def stop_lore_service(lore_executable_path: str, global_dir_name: str) -> str:
    """Stops the running service, if there is one, and reports the output.

    A test that starts a service without a process of its own to end it - by
    running a command that starts one - stops it through this. The command
    returns once the socket is free, so nothing has to wait afterwards.
    """
    env = os.environ.copy()
    env["LORE_GLOBAL_PATH"] = global_dir_name
    env["LORE_AUTH_PATH"] = global_dir_name
    stop = subprocess.run(
        [lore_executable_path, "service", "stop"],
        capture_output=True,
        text=True,
        env=env,
    )
    output = stop.stdout + stop.stderr
    logger.info("Stopping Lore service: %s", output)
    return output
