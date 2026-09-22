# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Regression tests for the local store location and disk space warnings.

A server with its store in a temporary directory and no `RUST_LOG` set warns
about the location and the free space on it. Both parts matter: the location is
decided by where the path points, and the default log filter has to carry a
warning on its own.

Assertions look for the level and the resolved path rather than the sentence.
"""

import json
import shutil
import tempfile
import time
from pathlib import Path

import pytest

from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)

# Threshold no volume can satisfy, so the low-space branch is always taken.
UNREACHABLE_THRESHOLD_BYTES = 1 << 60


def _logged(path: Path) -> str:
    """`path` as a JSON log line spells it.

    Windows separators are escaped there, so the plain string never matches.
    """
    return json.dumps(str(path))[1:-1]


def _names(path: Path, line: str) -> bool:
    """Whether `line` names `path`, whichever way the log spells a separator.

    A text log carries the path as it stands and a JSON log escapes the Windows
    separator, so either spelling counts. Reading one form alone passes on a
    platform whose separator needs no escaping and fails on Windows.
    """
    return str(path) in line or _logged(path) in line


def _wait_for_log_lines(log_path: Path, matches, timeout: float = 30.0):
    """Poll the server log until `matches` selects at least one line."""
    deadline = time.monotonic() + timeout

    while True:
        text = log_path.read_text(encoding="utf-8", errors="ignore")
        found = [line for line in text.splitlines() if matches(line)]
        if found:
            return found
        if time.monotonic() >= deadline:
            return []
        time.sleep(0.1)


@pytest.mark.smoke
@pytest.mark.xdist_group("local_store_warnings")
class TestLocalStoreWarnings:
    """One server start, both warnings read back out of its log."""

    @pytest.fixture(scope="class")
    def store_directory(self, request):
        """A store directory under the system temporary directory.

        Not `tmp_path_factory`: CI sets `--basetemp=/dev/shm/lore-smoke`, which
        is not a temporary directory as the server understands one.
        """
        path = Path(tempfile.mkdtemp(prefix="lore-regression-store-"))
        yield path
        if not request.config.getoption("--keep-test-data"):
            shutil.rmtree(path, ignore_errors=True)

    @pytest.fixture(scope="class")
    def server_log_path(
        self, request, tmp_path_factory, store_directory, lore_server_executable_path
    ):
        shared_port = allocate_free_port()
        ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        server_root, server_env = generate_server_config(
            request, tmp_path_factory, ports
        )

        # The reproduction sets no RUST_LOG: the default filter has to carry
        # warnings on its own.
        server_env.pop("RUST_LOG", None)
        server_env["LORE__IMMUTABLE_STORE__LOCAL__PATH"] = str(store_directory)
        server_env["LORE__MUTABLE_STORE__LOCAL__PATH"] = str(store_directory)
        server_env["LORE__SERVER__LOCAL_STORE_MONITOR__LOW_SPACE_THRESHOLD_BYTES"] = (
            str(UNREACHABLE_THRESHOLD_BYTES)
        )

        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        try:
            yield log_path
        finally:
            _kill_server_by_pid(
                server_proc.pid, log_path, label="local store warning server"
            )
            log_fd.close()

    def test_a_temporary_local_store_is_reported(
        self, server_log_path, store_directory
    ):
        """The store path is named at warning level, with RUST_LOG unset."""
        found = _wait_for_log_lines(
            server_log_path,
            lambda line: "WARN" in line and _names(store_directory, line),
        )

        assert found, (
            f"Expected a warning naming the local store at {store_directory}. "
            f"Server log:\n{server_log_path.read_text(encoding='utf-8', errors='ignore')}"
        )

    def test_a_configured_store_is_not_reported_as_unconfigured(self, server_log_path):
        """The two conditions are reported independently.

        This store has a path, so only the ephemeral-location warning applies.
        """
        found = _wait_for_log_lines(
            server_log_path,
            lambda line: (
                "WARN" in line and "no local store path is configured" in line.lower()
            ),
            timeout=2.0,
        )

        assert not found, (
            "A configured store must not be reported as unconfigured. "
            f"Server log:\n{server_log_path.read_text(encoding='utf-8', errors='ignore')}"
        )

    def test_low_disk_space_is_reported(self, server_log_path):
        """The monitor reports a volume that cannot meet the threshold.

        The mount point separates this from the warning a path no mounted
        filesystem matches draws, and the threshold proves the configured
        value reached the monitor.
        """
        found = _wait_for_log_lines(
            server_log_path,
            lambda line: (
                "WARN" in line
                and "mount_point" in line
                and str(UNREACHABLE_THRESHOLD_BYTES) in line
            ),
        )

        assert found, (
            "Expected a warning that the local store is running out of disk space. "
            f"Server log:\n{server_log_path.read_text(encoding='utf-8', errors='ignore')}"
        )


@pytest.mark.smoke
@pytest.mark.xdist_group("local_store_warnings_unconfigured")
class TestUnconfiguredLocalStore:
    """A server the configuration names no store path for."""

    @pytest.fixture(scope="class")
    def temp_root(self, request):
        """The temporary directory the server generates its store path under.

        Supplied through the environment so the generated path lands somewhere
        the test owns.
        """
        path = Path(tempfile.mkdtemp(prefix="lore-regression-temp-root-"))
        yield path
        if not request.config.getoption("--keep-test-data"):
            shutil.rmtree(path, ignore_errors=True)

    @pytest.fixture(scope="class")
    def server_log_path(
        self, request, tmp_path_factory, temp_root, lore_server_executable_path
    ):
        shared_port = allocate_free_port()
        ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        server_root, server_env = generate_server_config(
            request, tmp_path_factory, ports
        )

        server_env.pop("RUST_LOG", None)
        server_env["LORE__IMMUTABLE_STORE__LOCAL__PATH"] = ""
        server_env["LORE__MUTABLE_STORE__LOCAL__PATH"] = ""
        # TMP and TEMP cover Windows, TMPDIR the rest.
        server_env["TMPDIR"] = str(temp_root)
        server_env["TMP"] = str(temp_root)
        server_env["TEMP"] = str(temp_root)

        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        try:
            yield log_path
        finally:
            _kill_server_by_pid(
                server_proc.pid, log_path, label="unconfigured local store server"
            )
            log_fd.close()

    def test_an_unconfigured_store_is_reported(self, server_log_path, temp_root):
        """Both conditions are reported for a generated temporary path.

        Each store draws two warnings, so the generated path is named more than
        once. Counting rather than matching a sentence proves they are separate
        messages without fixing their wording.
        """
        generated = temp_root / "lore-server"
        found = _wait_for_log_lines(
            server_log_path,
            lambda line: "WARN" in line and _names(generated, line),
        )

        assert len(found) >= 2, (
            f"Expected separate warnings naming the generated store at {generated}, "
            f"got {len(found)}. "
            f"Server log:\n{server_log_path.read_text(encoding='utf-8', errors='ignore')}"
        )
