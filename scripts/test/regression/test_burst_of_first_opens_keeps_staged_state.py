# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""A burst of relayed commands must each be answered with the staged state.

A relayed call is completed by the service, so its `Complete` reaches the caller
as an ordinary event and the read loop returns on the result following it, with
the events still queued for the caller's own forwarder. `run_service_call` did
not wait for that forwarder, so a caller reading what its callback collected
could read it before the last events landed: `lore status` assembles its sections
that way and printed a repository with no staged changes, about one burst in two.

Concurrency is what made it show. The events are delivered by a task, and only a
loaded process leaves them queued long enough for the result to overtake them.
"""

import os
import subprocess

import pytest

from lore import Lore
from service_util import LORE_SERVICE_ENVIRONMENT

# Commands in one burst, and bursts in the test. With the drain removed a burst
# loses the staged state about half the time, so this reports a regression on
# better than 99 runs in 100. Each burst needs a repository the service has not
# served, which is what an attempt costs.
BURST = 20
BURSTS = 8


@pytest.mark.regression
def test_a_burst_of_relayed_commands_keeps_the_staged_state(
    new_lore_repo, stops_background_services
):
    """Every command in a burst reports the staged state the repository holds."""
    for burst in range(BURSTS):
        repo: Lore = new_lore_repo()
        file_name = "test.uasset"
        with repo.open_file(file_name, "w+b") as output_file:
            output_file.write(os.urandom(30))
        repo.stage(scan=True)

        environment = repo.sandboxed_env(**LORE_SERVICE_ENVIRONMENT)
        command_args = [repo.lore_executable_path, "--repository", repo.path, "status"]
        commands = [
            subprocess.Popen(
                command_args,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                env=environment,
                cwd=repo.path,
            )
            for _ in range(BURST)
        ]
        outputs = [command.communicate()[0] for command in commands]

        staged = f"A {file_name}"
        for index, (command, output) in enumerate(zip(commands, outputs)):
            assert command.returncode == 0, (
                f"burst {burst}, command {index} of {BURST} failed with "
                f"{command.returncode}: {output}"
            )
            reported = [line.strip(" ") for line in output.splitlines()]
            assert staged in reported, (
                f"burst {burst}, command {index} of {BURST} reached a service and "
                f"was answered without the staged state: {output}"
            )
