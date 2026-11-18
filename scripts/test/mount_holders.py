# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""What held the mounts a command reported busy, for the failure a test shows.

Lore's `Mounts are busy` names the paths it could not unmount, not what held
them. A smoke test that fails with it gets, appended to its error, the processes
whose working directory, root, open files or mapped files lie in each mount, and
the service log's lines on the mounts that stayed busy. Linux only: the holders
come from `/proc`, which lists only the processes this user may read, and only
those holding the mount when the command returned.
"""

import glob
import os
import re

_BUSY = re.compile(r"Mounts are busy: (.+)")


def _inside(target: str, mount: str) -> bool:
    return target == mount or target.startswith(mount + os.sep)


def _held_through(proc: str, mount: str) -> list[str]:
    """The ways the process at `proc` holds `mount`: `cwd`, `root`, an open file
    descriptor or a mapped file."""
    held = []
    for link in ("cwd", "root"):
        try:
            if _inside(os.readlink(os.path.join(proc, link)), mount):
                held.append(link)
        except OSError:
            pass
    try:
        for fd in os.listdir(os.path.join(proc, "fd")):
            try:
                target = os.readlink(os.path.join(proc, "fd", fd))
            except OSError:
                continue
            if _inside(target, mount):
                held.append(f"fd {fd} {target}")
    except OSError:
        pass
    try:
        with open(os.path.join(proc, "maps")) as maps:
            mapped = {
                fields[5].strip()
                for fields in (line.split(maxsplit=5) for line in maps)
                if len(fields) == 6
            }
        held.extend(f"maps {path}" for path in sorted(mapped) if _inside(path, mount))
    except OSError:
        pass
    return held


def mount_holders(mount: str) -> list[str]:
    """Each readable process holding `mount`, as `pid <pid> (<command line>): <ways>`."""
    mount = os.path.realpath(mount)
    holders = []
    for pid in filter(str.isdigit, os.listdir("/proc")):
        proc = os.path.join("/proc", pid)
        held = _held_through(proc, mount)
        if not held:
            continue
        try:
            with open(os.path.join(proc, "cmdline"), "rb") as cmdline:
                command = cmdline.read().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            command = "?"
        holders.append(f"pid {pid} ({command.strip()}): {', '.join(held)}")
    return holders


def busy_mounts_report(output: str, global_dir: str | None) -> str:
    """For the paths `output` reports busy, the processes holding each, and the
    lines of the service logs in `global_dir` on mounts that stayed busy. Empty
    where `output` reports none or there is no `/proc`."""
    busy = _BUSY.search(output)
    if busy is None or not os.path.isdir("/proc"):
        return ""
    report = []
    for mount in busy.group(1).strip().split(", "):
        holders = mount_holders(mount)
        report.append(f"Holding {mount}:")
        report.extend(f"  {holder}" for holder in holders or ["no readable process"])
    logs = (
        glob.glob(os.path.join(global_dir, "lore-service-*.log")) if global_dir else []
    )
    for log in sorted(logs):
        with open(log, errors="replace") as lines:
            report.extend(
                f"{os.path.basename(log)}: {line.rstrip()}"
                for line in lines
                if "stayed busy" in line
            )
    return "\n".join(report) + "\n"
