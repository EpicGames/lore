# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import platform
import subprocess

import pytest

from lore import Lore

logger = logging.getLogger(__name__)

# First 2 bytes of a 3-byte UTF-8 character (U+4E00), missing final byte.
TRUNCATED_UTF8 = b"hello \xe4\xb8"


def run_lore_raw(repo: Lore, urc_args: list, check: bool = True):
    """Call repo with raw byte arguments, bypassing text=True, in the environment the
    test set up for it."""
    cmd = [
        repo.lore_executable_path.encode(),
        b"--repository",
        repo.path.encode(),
    ] + [a.encode() if isinstance(a, str) else a for a in urc_args]
    return subprocess.run(
        cmd, capture_output=True, check=check, env=repo.sandboxed_env()
    )


@pytest.mark.smoke
@pytest.mark.skipif(
    platform.system() == "Windows",
    reason=(
        "Windows argv goes through CreateProcessW (UTF-16); non-UTF-8 bytes "
        "cannot reach the CLI by OS construction. The bad-UTF-8 metadata "
        "invariants are covered cross-platform by Rust unit tests in "
        "lore-revision/src/metadata.rs: to_string_rejects_truncated_utf8, "
        "walk_with_invalid_utf8_key, get_with_invalid_utf8_key."
    ),
)
def test_truncated_utf8_in_metadata(new_lore_repo):
    """
    Core must not panic when revision metadata contains truncated UTF-8.
    """
    repo: Lore = new_lore_repo()

    repo.write_commit_push("Initial commit", {"file.txt": "content\n"})

    # Inject truncated UTF-8 via raw subprocess (bypasses text=True encoding).
    result = run_lore_raw(
        repo,
        [b"revision", b"metadata", b"set", b"bad-key", TRUNCATED_UTF8],
        check=False,
    )
    assert result.returncode >= 0, (
        f"metadata set failed (returncode={result.returncode}):\n"
        f"stderr={result.stderr!r}"
    )

    # Read the key back — core should handle the bad data gracefully.
    result = run_lore_raw(
        repo, [b"revision", b"metadata", b"get", b"bad-key"], check=False
    )
    assert result.returncode == 0, (
        f"metadata get by key failed (returncode={result.returncode}):\n"
        f"stderr={result.stderr!r}"
    )

    # List all metadata — exercises the display/formatting path.
    result = run_lore_raw(repo, [b"revision", b"metadata", b"get"], check=False)
    assert result.returncode == 0, (
        f"metadata get (list all) failed (returncode={result.returncode}):\n"
        f"stderr={result.stderr!r}"
    )

    # History — exercises the revision summary path that reads metadata.
    result = run_lore_raw(repo, [b"history"], check=False)
    assert result.returncode == 0, (
        f"history failed (returncode={result.returncode}):\nstderr={result.stderr!r}"
    )
