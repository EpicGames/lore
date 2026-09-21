# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""What a view change costs, against the clone it replaces.

Reports rather than asserts, for the reason `lore-revision/tests/path_allocations.rs`
gives: the numbers are measurements to compare across changes, and a threshold
would either be loose enough to catch nothing or tight enough to break on an
unrelated change somewhere else in the client.

Run it deliberately, against the release binaries, with the tree size the
comparison needs:

```text
LORE_BENCH_FILES=4000 uv run pytest scripts/test/test_view_change_bench.py \
    -m slow --log-cli-level=INFO -s \
    --lore-server-binary=./target/release/loreserver \
    --lore-client-binary=./target/release/lore
```

Peak resident size comes from `/usr/bin/time -v`, so each command is run through
it rather than through `Lore.run`; without it the run still reports timings.
Allocation counts need the glibc interposer preloaded and the system allocator
selected, as that file describes:

```text
gcc -shared -fPIC -O2 -o /tmp/libcount.so scripts/allocations/allocount.c
LORE_BENCH_COUNT_ALLOCATIONS=/tmp/libcount.so ...
```
"""

import json
import logging
import os
import re
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

import pytest
from lore_parsers import parse_jsonl

from lore import Lore

logger = logging.getLogger(__name__)

# Files per directory and directories per tree, so a narrowing drops a subtree
# rather than a handful of files.
FILES = int(os.environ.get("LORE_BENCH_FILES", "2000"))
DIRECTORIES = 20
FILE_BYTES = 1024
# The half of the tree a narrowing drops.
DROPPED_PREFIX = "half-b"
NARROW_VIEW = ["/half-b"]


@dataclass
class Measured:
    """One command's cost."""

    label: str
    seconds: float
    peak_rss_kib: int | None
    bytes_written: int | None = None
    files: int | None = None
    allocations: int | None = None
    notes: list[str] = field(default_factory=list)

    def __str__(self) -> str:
        parts = [f"{self.label}: {self.seconds * 1000:.0f} ms"]
        if self.peak_rss_kib is not None:
            parts.append(f"{self.peak_rss_kib / 1024:.1f} MB peak RSS")
        if self.bytes_written is not None:
            parts.append(f"{self.bytes_written} bytes written")
        if self.files is not None:
            parts.append(f"{self.files} files")
        if self.allocations is not None:
            parts.append(f"{self.allocations} allocations")
        return ", ".join(parts + self.notes)


def timed_lore(
    instance: Lore, label: str, args: list[str], path: str, counts: Path
) -> Measured:
    """Runs one client command against `path`, timing it and reading its peak RSS.

    `Lore.run` is bypassed because the measurement needs the command wrapped in
    `/usr/bin/time` and its own environment, not because the arguments differ.
    `counts` is where the allocation interposer writes, outside every working
    tree so a measurement leaves no file for a later scan to find.
    """
    command = [instance.lore_executable_path, "--repository", path, "--json"] + args
    env = instance.sandboxed_env()
    interposer = os.environ.get("LORE_BENCH_COUNT_ALLOCATIONS")
    count_file = None
    if interposer:
        count_file = str(counts / f"{re.sub(r'[^a-z]+', '-', label)}.txt")
        env |= {
            "LORE_ALLOCATOR": "system",
            "LD_PRELOAD": interposer,
            "LORE_ALLOC_COUNT_FILE": count_file,
        }
    timer = shutil.which("time") if os.path.exists("/usr/bin/time") else None
    wrapped = ["/usr/bin/time", "-v"] + command if timer else command

    started = time.monotonic()
    result = subprocess.run(
        wrapped, capture_output=True, text=True, check=True, env=env
    )
    seconds = time.monotonic() - started

    peak = re.search(r"Maximum resident set size \(kbytes\): (\d+)", result.stderr)
    measured = Measured(
        label=label,
        seconds=seconds,
        peak_rss_kib=int(peak.group(1)) if peak else None,
    )
    progress = parse_jsonl(result.stdout, "revisionSyncProgress")
    if progress:
        measured.bytes_written = max(event["bytesUpdate"] for event in progress)
        measured.files = max(event["fileUpdate"] for event in progress) + max(
            event["fileDelete"] for event in progress
        )
    clone = [
        event["count"] for event in parse_jsonl(result.stdout, "repositoryCloneEnd")
    ]
    if clone:
        measured.bytes_written = max(count["bytesTransferred"] for count in clone)
        measured.files = max(count["fileComplete"] for count in clone)
    if count_file and os.path.exists(count_file):
        with open(count_file) as report:
            body = report.read()
        total = re.search(r"total[^0-9]*(\d+)", body)
        if total:
            measured.allocations = int(total.group(1))
        else:
            measured.notes.append(f"allocation report unparsed: {body.strip()}")
    return measured


def write_tree(repo: Lore) -> None:
    """Half the files under a prefix a narrowing drops, half under one it keeps."""
    for index in range(FILES):
        half = DROPPED_PREFIX if index % 2 else "half-a"
        directory = os.path.join(half, str(index % DIRECTORIES))
        repo.make_dirs(directory)
        with repo.open_file(os.path.join(directory, f"{index}.uasset"), "w+b") as out:
            out.write(os.urandom(FILE_BYTES))


@pytest.mark.slow
def test_view_change_against_the_clone_it_replaces(new_lore_repo, scratch_dir):
    """A narrowing, a widening, and the clones each would otherwise be."""
    repo: Lore = new_lore_repo()
    write_tree(repo)
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    view_dir = scratch_dir("view", create=True)
    narrow = os.path.join(view_dir, "narrow.txt")
    wide = os.path.join(view_dir, "wide.txt")
    with open(narrow, "w+") as view:
        view.writelines(rule + "\n" for rule in NARROW_VIEW)
    with open(wide, "w+") as view:
        view.write("")

    counts = Path(scratch_dir("allocations", create=True))
    instance = repo.clone()
    # The re-applied view carries nothing, so what it costs is what a sync costs
    # before any change is realized -- the baseline the other rows are read against.
    syncs = [
        ("sync --view (narrowing)", narrow),
        ("sync --view (re-applied, carries nothing)", narrow),
        ("sync --view (widening)", wide),
    ]
    measurements = [
        timed_lore(instance, label, ["sync", "--view", view], instance.path, counts)
        for label, view in syncs
    ]

    for label, view in [
        ("clone --view (narrow)", narrow),
        ("clone (whole tree)", None),
    ]:
        target = Path(scratch_dir("clone"))
        target.mkdir(parents=True, exist_ok=True)
        instance.created_paths.append(str(target))
        arguments = ["repository", "clone", repo.remote_path, str(target)]
        if view:
            arguments += ["--view", view]
        measurements.append(timed_lore(instance, label, arguments, str(target), counts))

    logger.info(
        "View change over %d files in %d directories:\n%s",
        FILES,
        DIRECTORIES,
        "\n".join(str(measurement) for measurement in measurements),
    )
    print(json.dumps([measurement.__dict__ for measurement in measurements], indent=2))
