# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import random
import subprocess
import time
from pathlib import Path

import pytest

from lore import Lore
from lore_parsers import parse_jsonl

logger = logging.getLogger(__name__)


@pytest.mark.slow
def test_store_compaction(new_lore_repo, lore_executable_path):
    repo: Lore = new_lore_repo()

    # Generate 10k files
    for i in range(10):
        subpath = str(i)
        for j in range(10):
            subsubpath = os.path.join(subpath, str(j))
            repo.make_dirs(subsubpath)
            for k in range(1000):
                with repo.open_file(
                    os.path.join(subsubpath, str(k) + ".uasset"), "w+b"
                ) as output_file:
                    output_file.write(os.urandom(10 + k + (i * k * j)))

    # Also one big file for re-fragmentation
    repo.make_dirs(os.path.join("large", "file"))
    with repo.open_file(os.path.join("large", "file", "test.png"), "w+b") as output_file:
        output_file.write(os.urandom(160 * 1024 * 1024))

    # Add a copy for deduplication
    repo.copy_file(
        os.path.join("large", "file", "test.png"),
        os.path.join("large", "file", "test2.png"),
    )

    # Commit local to ensure data gets written to local store
    repo.stage(scan=True)
    repo.commit("Generate files", local=True)
    repo.push(max_connections=16)

    # Incremental background GC is the default on every write; it spawns, steps, and
    # stops at the next step when the store Arc drops at command completion (the
    # store-drop cancellation itself is covered directly by the maintenance unit
    # tests evictor_exits_when_store_dropped / compactor_exits_when_store_dropped).
    # Under the default caps the store is below the limits, so these writes do no
    # eviction/compaction work but must stay consistent across the spawn/stop cycle.
    # (Tiny caps are NOT used here: incremental GC racing a write would compact away
    # state the same command needs.)
    for i in range(10):
        subsubpath = os.path.join("incremental", str(i))
        repo.make_dirs(subsubpath)
        with repo.open_file(
            os.path.join(subsubpath, "data.uasset"), "w+b"
        ) as output_file:
            output_file.write(os.urandom(256 * 1024))
        repo.stage(scan=True)
        repo.commit(f"Incremental write {i}", local=True)
    repo.repository_verify()
    # Push so every fragment is durable on the remote before aggressive GC; the
    # tiny-caps passes below evict local copies that the verifies then re-fetch.
    repo.push(max_connections=16)

    # Tiny caps so the dedicated `repository gc` (which suppresses the incremental
    # tasks and runs a full pass) collects to completion and emits the eviction and
    # compaction event series. No stage/commit runs after this point: gc evicts local
    # fragments and the later verifies re-fetch them from the remote.
    _set_store_caps(repo, "100", "100")
    gc_out = repo.repository_gc(json=True)
    # Expect: both event series fired end-to-end (begin + end for each).
    assert parse_jsonl(gc_out, "compactionBegin"), "compaction should begin"
    assert parse_jsonl(gc_out, "compactionEnd"), "compaction should end"
    assert parse_jsonl(gc_out, "evictionBegin"), "eviction should begin"
    assert parse_jsonl(gc_out, "evictionEnd"), "eviction should end"
    repo.repository_verify()

    # Repository metadata is among the very first fragments written and lives in
    # the local store. Reading it locally must still succeed right after the
    # aggressive eviction/compaction pass: non-durable local fragments are now
    # protected from GC, so `repository info --local` resolves the metadata from
    # the local store rather than failing on an evicted fragment.
    info_out = repo.repository_info(local=True)
    assert "Remote URL:" in info_out, info_out

    # Restore realistic caps so the remaining commands don't keep aggressively
    # evicting the re-fetched store (client defaults: 10 GiB / 2M fragments).
    _set_store_caps(repo, "10_737_418_240", "2_000_000")

    repo.status()
    repo.history()
    repo.repository_verify()

    # Stress the dedicated full GC: interrupt a `repository gc` mid-run (the next
    # step stops when the process — and the store Arc — is torn down) and run a
    # full pass to completion on alternating iterations, then confirm consistency.
    for i in range(0, 100):
        if i % 2 == 0:
            p = subprocess.Popen(
                [
                    lore_executable_path,
                    "--repository",
                    repo.path,
                    "--debug",
                    "repository",
                    "gc",
                ]
            )

            time.sleep(random.uniform(1.0, 5.0))

            p.terminate()
        else:
            repo.repository_gc(debug=True)

    repo.repository_verify()

    # Verify full gc run
    repo.repository_gc(debug=True)
    repo.repository_verify()


@pytest.mark.smoke
def test_repository_info_local_on_fresh_repo(new_lore_repo):
    """`repository info --local` resolves repository metadata from the local store
    without contacting the remote, even on a brand-new repository before any writes."""
    repo: Lore = new_lore_repo()
    out = repo.repository_info(local=True)
    assert "Remote URL:" in out, out


@pytest.mark.smoke
def test_repository_info_local_survives_gc(new_lore_repo):
    """Repository metadata is a non-durable local fragment that is never pushed and
    is among the first fragments written. After an aggressive eviction/compaction
    pass it must still load via `repository info --local` — non-durable local
    fragments are protected from GC."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo, count=8)
    _set_store_caps(repo, "100", "100")
    repo.repository_gc()
    out = repo.repository_info(local=True)
    assert "Remote URL:" in out, out


def _seed_committed_data(repo: Lore, count: int = 8) -> None:
    """Write and commit some MB locally so the store holds real fragments."""
    repo.make_dirs("data")
    for k in range(count):
        with repo.open_file(os.path.join("data", f"{k}.uasset"), "w+b") as f:
            f.write(os.urandom(256 * 1024))
    repo.stage(scan=True)
    repo.commit("Seed data", local=True)


def _set_store_caps(repo: Lore, max_size: str, max_capacity: str) -> None:
    """Rewrite the repository's [store] GC caps in config.toml."""
    config_path = Path(os.path.join(repo.dot_path(), "config.toml"))
    lines = config_path.read_text(encoding="utf-8").splitlines(keepends=True)
    for i, line in enumerate(lines):
        if line.strip().startswith("max_size"):
            lines[i] = f"max_size = {max_size}\n"
        elif line.strip().startswith("max_capacity"):
            lines[i] = f"max_capacity = {max_capacity}\n"
    config_path.write_text("".join(lines), encoding="utf-8")


@pytest.mark.smoke
def test_repository_gc_emits_event_series(new_lore_repo):
    """`repository gc --json` emits the eviction and compaction event series when the
    store is over its configured caps."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo, count=64)
    # Push so the fragments are durable: eviction protects non-durable local
    # fragments, so only durable data drives the eviction event series.
    repo.push(max_connections=16)
    # Tiny caps so both passes do real work and emit their begin/end events.
    _set_store_caps(repo, max_size="100", max_capacity="100")

    out = repo.repository_gc(json=True)

    assert parse_jsonl(out, "compactionBegin"), "compaction should begin"
    assert parse_jsonl(out, "compactionEnd"), "compaction should end"
    eviction_begin = parse_jsonl(out, "evictionBegin")
    eviction_end = parse_jsonl(out, "evictionEnd")
    assert eviction_begin, "eviction should begin"
    assert eviction_end, "eviction should end"
    # Event data is delivered with camelCase fields.
    assert "targetFragments" in eviction_begin[0]
    assert "totalEvicted" in eviction_end[0]


@pytest.mark.smoke
def test_repository_gc_prints_final_summary(new_lore_repo):
    """`repository gc` prints the final eviction/compaction totals on a normal line at
    completion, so the result survives after the live progress bar is cleared."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo)

    out = repo.repository_gc()

    assert "Garbage collection complete" in out


@pytest.mark.smoke
def test_no_gc_emits_no_gc_events(new_lore_repo):
    """`--no-gc` on a write prevents the automatic incremental GC, so no eviction or
    compaction events are emitted."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo)

    repo.make_dirs("more")
    with repo.open_file(os.path.join("more", "x.uasset"), "w+b") as f:
        f.write(os.urandom(256 * 1024))
    repo.stage(scan=True)
    out = repo.commit("No gc", local=True, no_gc=True, json=True)

    assert not parse_jsonl(out, "compactionBegin")
    assert not parse_jsonl(out, "evictionBegin")


@pytest.mark.smoke
def test_read_emits_no_gc_events(new_lore_repo):
    """A read command runs no GC, so it emits no eviction or compaction events."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo)

    out = repo.status(json=True)

    assert not parse_jsonl(out, "compactionBegin")
    assert not parse_jsonl(out, "evictionBegin")


@pytest.mark.smoke
def test_plain_write_emits_no_full_gc_events(new_lore_repo):
    """A plain write runs only the automatic incremental GC, which does no work (and
    so emits no events) while the store is under its caps; it never runs the full
    `repository gc` pass. The incremental spawn gating is covered deterministically by
    the `incremental_gc_options` unit tests in lore-revision."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo)

    repo.make_dirs("more")
    with repo.open_file(os.path.join("more", "y.uasset"), "w+b") as f:
        f.write(os.urandom(512 * 1024))
    repo.stage(scan=True)
    out = repo.commit("Plain write", local=True, json=True)

    assert not parse_jsonl(out, "compactionBegin")
    assert not parse_jsonl(out, "evictionBegin")


@pytest.mark.smoke
def test_sync_reload_triggers_load_driven_gc(new_lore_repo):
    """Loading enough of the store fires the automatic GC without an explicit
    `repository gc`.

    Round-tripping the working tree back to a data-heavy revision re-materializes every
    file, which deserializes its buckets and resumes its packstores. That load pushes
    the GC counters over the configured caps and fires a compaction pass directly — the
    load-can-trigger path that replaced the per-command startup scan. A write op is used
    (sync) because read-only opens disable the caps and `repository verify` stops GC.
    """
    repo: Lore = new_lore_repo()
    repo.write_commit_push("base", {"base.txt": ["base\n"]})

    # 110 MiB across individual 10 MiB commits.
    repo.make_dirs("bulk")
    for i in range(11):
        with repo.open_file(os.path.join("bulk", f"{i}.bin"), "w+b") as f:
            f.write(os.urandom(10 * 1024 * 1024))
        repo.stage(scan=True)
        repo.commit(f"Bulk {i}", local=True)
    repo.push(max_connections=16)

    # Tiny size cap so the forward sync's reload trips compaction; keep the capacity cap
    # high so eviction doesn't remove fragments the same sync is still materializing.
    _set_store_caps(repo, max_size="100", max_capacity="2_000_000")

    repo.sync(revision="@1", reset=True)
    out = repo.sync(reset=True, json=True)

    assert parse_jsonl(out, "compactionBegin"), (
        "sync's full reload should fire the load-driven compaction trigger"
    )
