# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import random
import shutil
import subprocess
import time

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_file(new_lore_repo, lore_executable_path):
    repo: Lore = new_lore_repo()
    for i in range(10):
        subpath = str(i)
        repo.make_dirs(subpath)
        for j in range(10):
            subsubpath = os.path.join(subpath, str(j))
            repo.make_dirs(os.path.join(subsubpath))
            for k in range(100):
                with repo.open_file(
                    os.path.join(subsubpath, str(k) + ".uasset"), "w+b"
                ) as output_file:
                    # Minimum 5 KiB per file so every write crosses the compression
                    # threshold, which keeps the commit busy long enough that the
                    # mid-commit kill below lands deterministically.
                    output_file.write(os.urandom(5120 + k + (10 * i * k * j)))

    # One file exactly on size threshold
    repo.make_dirs(os.path.join("threshold", "file"))
    with repo.open_file(
        os.path.join("threshold", "file", "test.uasset"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(65536))

    # Also one big file for re-fragmentation
    repo.make_dirs(os.path.join("large", "file"))
    large_file_path = os.path.join("large", "file", "test.png")
    with repo.open_file(os.path.join(large_file_path), "w+b") as output_file:
        output_file.write(os.urandom(160 * 1024 * 1024))

    # Add a copy for deduplication
    repo.copy_file(large_file_path, os.path.join("large", "file", "test2.png"))

    repo.stage(scan=True)

    # Commit the files with a low file count limit to slow it down, then kill
    # the commit mid-flight to exercise the server's behavior when a QUIC
    # connection is abruptly closed, and verify local store consistency after
    # a random process kill.
    p = subprocess.Popen(
        [
            lore_executable_path,
            "--repository",
            repo.path,
            "--file-count-limit",
            "2",
            "commit",
            "Test commit",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env=repo.sandboxed_env(),
    )

    # Wait for commit to announce it has started fragmenting files, then give the
    # background upload tracker a brief moment to spawn before killing. This
    # anchors the kill inside the commit body regardless of machine speed.
    commit_started_marker = "Fragmenting files and updating tree hashes"
    commit_started = False
    start_deadline = time.monotonic() + 30
    assert p.stdout is not None
    while time.monotonic() < start_deadline:
        line = p.stdout.readline()
        if not line:
            break
        if commit_started_marker in line:
            commit_started = True
            break
    assert commit_started, (
        f"Commit did not reach {commit_started_marker!r} within 30 seconds"
    )

    time.sleep(0.2)
    p.terminate()
    p.wait(2)

    # What this checks is that a commit still succeeds after one was killed
    # while it was writing data. Where the kill lands is not something a sleep
    # can pin down: the commit records the revision early and then writes for
    # far longer, so a kill during the writing usually finds the stage already
    # cleared, and a faster commit makes that the common case. Stage something
    # again when that happens, so the commit below always has work to do and the
    # recovery is exercised either way.
    if "Changes staged for commit" not in repo.status():
        with repo.open_file("after_interrupted_commit.uasset", "w+b") as output_file:
            output_file.write(os.urandom(8192))
        repo.stage(scan=True)

    repo.commit("Test commit after waiting")
    repo.repository_verify()
    repo.push(max_connections=16)
    repository_id = repo.get_id()
    logger.info("Repository " + repository_id)

    # Clone the repository
    clone = repo.clone()

    # Verify files contents, mode and last modified timestamp
    for i in range(10):
        subpath = str(i)
        for j in range(10):
            subsubpath = os.path.join(subpath, str(j))
            for k in range(100):
                filepath = os.path.join(subsubpath, str(k) + ".uasset")
                assert repo.compare_file(clone, filepath), (
                    f'Path "{filepath}" does not match in cloned repository'
                )

    filepath = os.path.join("threshold", "file", "test.uasset")
    assert repo.compare_file(clone, filepath), (
        f'Path "{filepath}" does not match in cloned repository'
    )

    filepath = os.path.join("large", "file", "test.png")
    assert repo.compare_file(clone, filepath), (
        f'Path "{filepath}" does not match in cloned repository'
    )

    filepath = os.path.join("large", "file", "test2.png")
    assert repo.compare_file(clone, filepath), (
        f'Path "{filepath}" does not match in cloned repository'
    )

    clone.repository_verify()

    # Delete random source files and some directories
    for i in range(6000):
        fileint = random.randint(0, 100)
        dirint = random.randint(0, 100)
        path = os.path.join(
            str(int(dirint / 10)), str(int(dirint % 10)), str(fileint) + ".uasset"
        )
        # noinspection PyBroadException
        try:
            repo.remove_file(path)
        except:
            pass

    for i in range(20):
        dirint = random.randint(0, 100)
        path = os.path.join(str(int(dirint / 10)), str(int(dirint % 10)))
        shutil.rmtree(os.path.join(repo.path, path), ignore_errors=True)

    repo.stage(scan=True)
    repo.commit("Delete commit")
    repo.repository_verify()
    repo.push()

    # Sync the cloned repository
    clone.sync()

    for i in range(10):
        subpath = str(i)
        assert repo.path_exists(subpath) == clone.path_exists(subpath), (
            "Source directory does not match clone directory"
        )
        for j in range(10):
            subsubpath = os.path.join(subpath, str(j))
            assert repo.path_exists(subsubpath) == clone.path_exists(subsubpath), (
                "Source directory does not match clone directory"
            )
            for k in range(100):
                filepath = os.path.join(subsubpath, str(k) + ".uasset")
                assert repo.path_exists(filepath) == clone.path_exists(filepath), (
                    "Source directory does not match clone directory"
                )

    # Verify the repository
    clone.repository_verify()

    # Create a new clone
    clone = repo.clone()

    # Ensure we can commit offline directly after a clone
    with clone.open_file(large_file_path, "w+b") as output_file:
        output_file.write(os.urandom(160 * 1024))

    clone.stage(large_file_path, offline=True)

    # Commit the files
    clone.commit("Test commit offline after clone", offline=True)
    clone.push()
    clone.sync("@1")
    clone.branch_reset("@1")

    with clone.open_file(large_file_path, "w+b") as output_file:
        output_file.write(os.urandom(170 * 1024))

    clone.stage(large_file_path, offline=True)
    clone.commit("Test commit offline after sync", offline=True)
