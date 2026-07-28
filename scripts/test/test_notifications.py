#!/usr/bin/python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import subprocess
import threading
import concurrent.futures

import pytest

from lore import Lore

logger = logging.getLogger(__name__)

# Line the `notification subscribe` command prints once its subscription is
# established and it is ready to receive events.
LISTENING_MARKER = "Listening for notifications"


def notification_subscribe(
    repo: Lore,
    timeout: int,
    expected_messages: list[str] | None = None,
    listening_event: threading.Event | None = None,
):
    """Subscribe to notifications, exiting early when all expected_messages are found.

    When `listening_event` is given, it is set as soon as the subscriber reports
    it is listening, so the caller can wait until then before emitting events.
    """
    command_args = [
        repo.lore_executable_path,
        "--repository",
        repo.path,
        "--debug",
        "notification",
        "subscribe",
        str(timeout),
    ]

    logger.info(f"Starting notification subscribe: {' '.join(command_args)}")

    process = subprocess.Popen(
        command_args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )

    collected_output = ""
    try:
        if expected_messages is None:
            stdout, _ = process.communicate()
            return stdout

        remaining = set(expected_messages)
        for line in process.stdout:
            collected_output += line
            logger.info(f"notification output: {line.rstrip()}")
            if listening_event is not None and LISTENING_MARKER in line:
                listening_event.set()
            remaining = {msg for msg in remaining if msg not in collected_output}
            if not remaining:
                break
    finally:
        logger.info(f"notification final output: {collected_output!r}")
        process.terminate()
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    return collected_output


def subscribe_and_wait_listening(executor, repo: Lore, timeout: int, expected_messages):
    """Start a notification subscription and block until it is listening.

    Emitting events before the subscriber is established loses them, so this
    waits for the listening marker before returning. Establishing the
    subscription takes ~1s and races the test's own operations under parallel
    workers. Returns the future carrying the collected notification output.
    """
    listening = threading.Event()
    future = executor.submit(
        notification_subscribe, repo, timeout, expected_messages, listening
    )
    assert listening.wait(timeout=timeout), (
        "notification subscription did not start listening within the timeout"
    )
    return future


@pytest.mark.smoke
class TestNotifications:
    def test_branch_archive_event(
        self,
        new_lore_repo,
    ):
        repo: Lore = new_lore_repo("branch_archive_test")
        child_branch_name = "test-branch"

        with concurrent.futures.ThreadPoolExecutor() as executor:
            # Wait until the subscription is listening before emitting events,
            # otherwise the early "Branch pushed" can fire before the subscriber
            # is established and be missed (flaky under parallel workers).
            notification_future = subscribe_and_wait_listening(
                executor,
                repo,
                30,
                ["Branch pushed", "Branch deleted"],
            )

            # set up some stub data on main
            text_file = "text-File.txt"
            with repo.open_file(text_file, "w+") as file:
                file.writelines(["One line"])
            repo.stage(scan=True)
            repo.commit()
            repo.push()

            # create a child branch with some extra data
            repo.branch_create(child_branch_name)
            with repo.open_file(text_file, "w+") as file:
                file.writelines(["Two line"])
            repo.stage(scan=True)
            repo.commit()
            # push to raise the 'pushed' notification
            repo.push()

            # go back to main and delete the branch to raise the 'deleted' notification
            repo.branch_switch("main")
            repo.branch_archive(child_branch_name)

            # get the notification output
            notification_output = notification_future.result()

        assert "Branch pushed" in notification_output
        assert "Branch deleted" in notification_output

    def test_branch_created_event(
        self,
        new_lore_repo,
    ):
        repo: Lore = new_lore_repo("branch_CREATED_test")
        child_branch_name = "test-branch"

        with concurrent.futures.ThreadPoolExecutor() as executor:
            # Wait until the subscription is listening before emitting events
            # (see test_branch_archive_event).
            notification_future = subscribe_and_wait_listening(
                executor, repo, 30, ["Branch created"]
            )

            # set up some stub data on main
            text_file = "text-File.txt"
            with repo.open_file(text_file, "w+") as file:
                file.writelines(["One line"])
            repo.stage(scan=True)
            repo.commit()
            repo.push()

            # create a child branch
            repo.branch_create(child_branch_name)

            repo.push()

            # get the notification output
            notification_output = notification_future.result()

        assert "Branch created" in notification_output
