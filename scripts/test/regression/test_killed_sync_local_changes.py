# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os
import subprocess
import time

import pytest

from lore import Lore

_MB = 1024 * 1024


@pytest.mark.regression
@pytest.mark.bug_reproduction
def test_killed_sync_leaves_blocking_orphan(new_lore_repo):
    """UCS-19888: a killed sync leaves the workspace half-synced and blocks later syncs.

    A sync can be interrupted by Ctrl+C or a killed process. When that happens, lore
    writes the file to disk but does not update the local state. The file on disk is
    then the new version, but the tracked state still points to the old revision.

    A later sync to a different version compares the file on disk to the incoming file.
    They differ, so the sync stops with "File has local changes". The user never edited
    the file.

    Steps:
      1. rev1: X.bin is 20 MB. Clone at rev1.
      2. rev2 (upstream): X.bin is 40 MB. Sync toward it, then kill it mid-write.
         The clone now has 40 MB on disk, but its state is still rev1.
      3. rev3 (upstream): X.bin is 60 MB.
      4. Sync. It should complete. The bug: it stops with
         "File has local changes (incoming 60 MB, file system 40 MB)" at realize.rs:481.

    This test kills a real sync process, so it depends on timing. It skips (it does not
    pass) if the kill misses the write.
    """
    repo: Lore = new_lore_repo()
    repo.write_commit_push("rev1", {"X.bin": b"a" * (20 * _MB)})
    clone = repo.clone(revision="main@1")
    xpath = clone._fix_path("X.bin")
    assert os.path.getsize(xpath) == 20 * _MB, "setup: X.bin is the rev1 size on disk"

    repo.write_commit_push("rev2", {"X.bin": b"b" * (40 * _MB)})

    # Start a sync toward rev2. Kill it as soon as X.bin changes from its rev1 size.
    env = os.environ.copy()
    env.update(clone.environment_vars)
    env["LORE_GLOBAL_PATH"] = clone.global_dir
    env.setdefault("LORE_AUTH_PATH", clone.global_dir)
    proc = subprocess.Popen(
        [clone.lore_executable_path, "--repository", clone.path, "sync"],
        cwd=clone.path, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env,
    )
    deadline = time.time() + 60
    while time.time() < deadline:
        try:
            if os.path.getsize(xpath) != 20 * _MB:
                proc.kill()
                break
        except OSError:
            pass
        if proc.poll() is not None:
            break
        time.sleep(0.002)
    proc.wait()

    # Confirm the interruption actually left a half-synced workspace. If the kill
    # landed too early or too late, skip rather than pass for the wrong reason.
    if os.path.getsize(xpath) == 20 * _MB:
        pytest.skip("kill landed before the file was written; re-run")
    if "behind remote" not in clone.run(["status"], check=False):
        pytest.skip("kill landed after the state was finalized; re-run")

    # Upstream moves to a different size, then sync. This is the failing step:
    # BUG -> raises "File has local changes"; FIXED -> sync completes.
    repo.write_commit_push("rev3", {"X.bin": b"c" * (60 * _MB)})
    clone.sync()
