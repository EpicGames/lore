# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Removal of the directories the smoke tests leave on disk.

Every test writes a repository and a global config directory under pytest's
basetemp, and many write a shared store, a clone target or a whole server root
beside them. Nothing removed any of it, so a full run left about ten gigabytes
behind and each worker's peak usage was the entire session's output rather than
the handful of tests actually in flight.

Removal is best effort and never raises. A test that has already made its
assertions must not be turned red by a file the OS is slow to unlock, so the
callers here log and carry on; the end-of-session sweep in `lore_server.py`
takes whatever a per-test cleanup could not.
"""

import logging
import os
import shutil
import stat
from pathlib import Path
from time import sleep

logger = logging.getLogger(__name__)

# Windows releases file handles asynchronously: a server we just killed, or a
# virus scanner walking in behind it, can still hold a fragment open for a short
# while after the process is gone, and the unlink fails with PermissionError
# until it lets go. Retry across roughly two seconds before giving up. POSIX
# unlinks an open file without complaint, so these delays cost nothing there.
_RETRY_DELAYS_SECONDS = (0.1, 0.25, 0.5, 1.0)


def _retry_without_readonly(func, path, exc):
    """`shutil.rmtree` error handler that clears the read-only bit and retries.

    Windows refuses `os.unlink` on a read-only file, where POSIX only asks for
    write permission on the parent directory. Lore leaves read-only files in a
    store, so on Windows the first pass over one of those raises rather than
    removes. An entry that has already gone is not an error; anything else that
    is not a permission problem is re-raised for the retry loop in
    `remove_tree` to see.
    """
    if isinstance(exc, FileNotFoundError):
        # Something removed this entry first: a duplicate in the caller's list
        # of paths, or a subprocess still tidying up as we walk. Carry on with
        # the rest of the tree rather than abandoning the walk over one entry
        # that has already reached the state we wanted.
        return
    if not isinstance(exc, PermissionError):
        raise exc
    # Add the write bit rather than assign it. stat.S_IWRITE is 0o200, so
    # assigning would strip read and execute, and a directory left at 0o200
    # cannot be listed or removed at all -- not by the retry below, and not by
    # any later attempt or by a plain rm -rf.
    mode = os.stat(path).st_mode
    if stat.S_ISDIR(mode):
        mode |= stat.S_IXUSR
    os.chmod(path, mode | stat.S_IWUSR)
    func(path)


def remove_tree(path, *, label: str = "test data") -> bool:
    """Remove `path` and everything under it, returning whether it is gone.

    Never raises: a cleanup failure is reported as a warning and a `False`
    return, because losing a directory to a stuck handle is a disk-space
    problem and failing the test that created it would be a correctness one.
    """
    path = Path(path)
    attempts = len(_RETRY_DELAYS_SECONDS) + 1
    for attempt in range(attempts):
        try:
            shutil.rmtree(path, onexc=_retry_without_readonly)
            return True
        except OSError as error:
            # Success is the tree being gone, not the call returning cleanly.
            # Checking rather than trusting the exception type keeps a
            # already-removed path (a duplicate entry, a test that cleaned up
            # after itself) from being reported as a failure, and stops a
            # vanished child from being read as a removed parent.
            if not path.exists():
                return True
            if attempt == attempts - 1:
                logger.warning("Could not remove %s at %s: %s", label, path, error)
                return False
            sleep(_RETRY_DELAYS_SECONDS[attempt])
    return False


def remove_tree_contents(path, *, label: str = "test data") -> bool:
    """Empty `path` without removing `path` itself, returning whether it is bare.

    The session sweep runs against pytest's basetemp, which is a directory the
    user may have named with `--basetemp` and which pytest's own numbered
    directory bookkeeping still owns. Emptying it returns the disk without
    reaching into either.
    """
    path = Path(path)
    if not path.is_dir():
        return True
    try:
        # Listed up front, and inside the guard: this runs from
        # pytest_sessionfinish, where an escaping OSError would turn a run whose
        # tests have all passed into an INTERNALERROR with a non-zero exit.
        entries = list(path.iterdir())
    except OSError as error:
        logger.warning("Could not list %s at %s: %s", label, path, error)
        return False
    emptied = True
    for entry in entries:
        # A symlink to a directory is unlinked, not walked: pytest points a
        # `<basename>current` symlink at the newest numbered directory, and
        # following it would remove the target twice.
        if entry.is_dir() and not entry.is_symlink():
            emptied &= remove_tree(entry, label=label)
            continue
        try:
            entry.unlink()
        except FileNotFoundError:
            pass
        except OSError as error:
            logger.warning("Could not remove %s at %s: %s", label, entry, error)
            emptied = False
    return emptied
