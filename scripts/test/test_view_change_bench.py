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
import statistics
import subprocess
import time
from dataclasses import asdict, dataclass, field
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
# Where the layer rows mount a second repository, and the view taking it out of view.
LAYER_MOUNT = "lay"
LAYER_EXCLUDED_VIEW = ["/lay"]


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

    def columns(self) -> list[str]:
        """What the run cost besides the clock, for a caller printing its own timing."""
        parts = []
        if self.peak_rss_kib is not None:
            parts.append(f"{self.peak_rss_kib / 1024:.1f} MB peak RSS")
        if self.bytes_written is not None:
            parts.append(f"{self.bytes_written} bytes written")
        if self.files is not None:
            parts.append(f"{self.files} files")
        if self.allocations is not None:
            parts.append(f"{self.allocations} allocations")
        return parts

    def __str__(self) -> str:
        return ", ".join(
            [
                f"{self.label}: {self.seconds * 1000:.0f} ms",
                *self.columns(),
                *self.notes,
            ]
        )


def timed_lore(
    instance: Lore, label: str, args: list[str], path: str | None, counts: Path
) -> Measured:
    """Runs one client command against `path`, timing it and reading its peak RSS.

    `Lore.run` is bypassed because the measurement needs the command wrapped in
    `/usr/bin/time` and its own environment, not because the arguments differ.
    `path` names the working tree the command runs against, which is the instance's
    own where it is absent -- a clone names the directory it is about to create.
    `counts` is where the allocation interposer writes, outside every working
    tree so a measurement leaves no file for a later scan to find.
    """
    command = [
        instance.lore_executable_path,
        "--repository",
        path or instance.path,
        "--json",
    ] + args
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


def write_tree(repo: Lore, root: str = "") -> None:
    """Half the files under a prefix a narrowing drops, half under one it keeps.

    `root` puts the whole tree under one directory, which is what a layer mounted at
    that path holds.
    """
    for index in range(FILES):
        half = DROPPED_PREFIX if index % 2 else "half-a"
        directory = os.path.join(root, half, str(index % DIRECTORIES))
        repo.make_dirs(directory)
        with repo.open_file(os.path.join(directory, f"{index}.uasset"), "w+b") as out:
            out.write(os.urandom(FILE_BYTES))


def view_files(scratch_dir, rules: dict[str, list[str]]) -> dict[str, str]:
    """One view filter file per named rule set, beside the repositories."""
    directory = scratch_dir("view", create=True)
    written = {}
    for name, lines in rules.items():
        path = os.path.join(directory, name + ".txt")
        with open(path, "w+") as view:
            view.writelines(rule + "\n" for rule in lines)
        written[name] = path
    return written


# Passes over the rows, for a comparison between two of them rather than against a
# recorded number. One run each says nothing on a machine with this one's spread.
PASSES = 8


@dataclass
class Summarized:
    """What one row cost across the passes.

    The minimum is the cleanest run the machine gave and the median is what it gives
    typically; a difference between two rows has to show in both to be one. The rest
    is the last pass: the bytes, the files and the allocations are the same in every
    pass, and the peak resident size is one sample of it.
    """

    label: str
    minimum: float
    median: float
    passes: int
    last: Measured

    def __str__(self) -> str:
        return ", ".join(
            [
                f"{self.label}: {self.minimum * 1000:.0f} ms min",
                f"{self.median * 1000:.0f} ms median of {self.passes}",
                *self.last.columns(),
            ]
        )


def interleaved(
    rows: list[tuple[Lore, str, list[str]]], counts: Path
) -> list[Summarized]:
    """Runs every row once per pass, answering what each cost across them.

    Interleaved rather than one row at a time, so a drift in the machine's state
    lands on every row instead of on whichever block ran last. Each pass leaves the
    instances as it found them, which is what makes a pass repeatable: a sync that
    carries nothing carries nothing again, and the view rows alternate.
    """
    runs: dict[str, list[Measured]] = {label: [] for _instance, label, _args in rows}
    for _pass in range(PASSES):
        for instance, label, args in rows:
            runs[label].append(timed_lore(instance, label, args, None, counts))
    return [
        Summarized(
            label=label,
            minimum=min(run.seconds for run in measured),
            median=statistics.median(run.seconds for run in measured),
            passes=len(measured),
            last=measured[-1],
        )
        for label, measured in runs.items()
    ]


@pytest.mark.slow
def test_view_change_against_the_clone_it_replaces(new_lore_repo, scratch_dir):
    """A narrowing, a widening, and the clones each would otherwise be."""
    repo: Lore = new_lore_repo()
    write_tree(repo)
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    views = view_files(scratch_dir, {"narrow": NARROW_VIEW, "wide": []})
    narrow, wide = views["narrow"], views["wide"]

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
    print(json.dumps([asdict(measurement) for measurement in measurements], indent=2))


@pytest.mark.slow
def test_layer_mount_view_change(new_lore_repo, scratch_dir):
    """What carrying a layer mount in and out of view costs, beside the syncs that
    leave it alone.

    The first two rows are the pair the change has to leave alone: a sync with no
    layer configured, and the same sync with one whose revision has not moved. The
    layer is left out of the list a sync carries in that case, so the two are the
    same work and the rows say whether they are the same cost.
    """
    plain: Lore = new_lore_repo()
    write_tree(plain)
    plain.stage(scan=True)
    plain.commit()
    plain.push()

    repo: Lore = new_lore_repo(plain.name + "_mounting")
    write_tree(repo)
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    layer_repo: Lore = new_lore_repo(plain.name + "_layer")
    write_tree(layer_repo, root=LAYER_MOUNT)
    layer_repo.stage(scan=True)
    layer_repo.commit()
    layer_repo.push()
    repo.layer_add(LAYER_MOUNT, layer_repo, LAYER_MOUNT + "/")

    views = view_files(scratch_dir, {"mount-out": LAYER_EXCLUDED_VIEW, "wide": []})
    counts = Path(scratch_dir("allocations", create=True))
    instance = plain.clone()
    rows = [
        (instance, "sync (no layer configured)", ["sync"]),
        (repo, "sync (layer at a standing revision)", ["sync"]),
        (
            repo,
            "sync --view (mount leaving the view)",
            ["sync", "--view", views["mount-out"]],
        ),
        (
            repo,
            "sync --view (mount entering the view)",
            ["sync", "--view", views["wide"]],
        ),
    ]
    measurements = interleaved(rows, counts)

    logger.info(
        "Layer mount view change over %d files in %d directories, %d passes:\n%s",
        FILES,
        DIRECTORIES,
        PASSES,
        "\n".join(str(measurement) for measurement in measurements),
    )
    print(json.dumps([asdict(measurement) for measurement in measurements], indent=2))
