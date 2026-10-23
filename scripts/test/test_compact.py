# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import hashlib
import logging
import os
import random
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

from lore import Lore
from lore_parsers import parse_jsonl
from store_layout import (
    assert_all_index_entries_reachable,
    torn_index_files,
    unreachable_index_entries,
)

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
    with repo.open_file(
        os.path.join("large", "file", "test.png"), "w+b"
    ) as output_file:
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
                ],
                env=repo.sandboxed_env(),
            )

            time.sleep(random.uniform(1.0, 5.0))

            p.terminate()
        else:
            repo.repository_gc(debug=True)

    repo.repository_verify()

    # Verify full gc run
    repo.repository_gc(debug=True)
    repo.repository_verify()
    assert_all_index_entries_reachable(repo)


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
def test_repository_gc_applies_each_cap_to_its_own_pass(new_lore_repo):
    """`repository gc` holds compaction to `max_size` and eviction to `max_capacity`, and
    loads the whole store index first: with only the capacity cap low, it evicts the
    pushed, durable fragments and does not compact."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo, count=64)
    repo.push(max_connections=16)
    _set_store_caps(repo, max_size="10_737_418_240", max_capacity="100")

    out = repo.repository_gc(json=True)

    assert parse_jsonl(out, "evictionBegin"), "eviction should begin"
    assert not parse_jsonl(out, "compactionBegin"), "compaction should not begin"


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


def _random_contents(count: int) -> list[bytes]:
    """`count` random contents of 1 KiB to 2 MiB each."""
    return [os.urandom(random.randint(1024, 2 * 1024 * 1024)) for _ in range(count)]


def _write_files(repo: Lore, directory: str, contents: list[bytes]) -> dict[str, str]:
    """Write each of `contents` to a file of its own under `directory`. Returns the SHA-256 of
    each file, by path."""
    repo.make_dirs(directory)
    digests = {}
    for index, data in enumerate(contents):
        path = os.path.join(directory, f"{index}.uasset")
        with repo.open_file(path, "w+b") as output_file:
            output_file.write(data)
        digests[path] = hashlib.sha256(data).hexdigest()
    return digests


def _assert_files_restore(repo: Lore, digests: dict[str, str], **kwargs) -> None:
    """Delete each file of `digests` from the working tree, restore it from the store, and
    require the content it was committed with."""
    for path in digests:
        os.remove(os.path.join(repo.path, path))
    repo.file_reset(list(digests), **kwargs)
    for path, digest in digests.items():
        with repo.open_file(path, "rb") as restored:
            assert hashlib.sha256(restored.read()).hexdigest() == digest, path


class _DeduplicatedStore:
    """Three repositories over one shared store, with caps low enough that every collection
    evicts and compacts. Each commits with `--local`, which stores payloads in the store alone.
    `local` never pushes, so its entries stay local-only and it has no remote copy to restore
    from. `pushing` and `mirror` push what they commit. Each repository is a partition of its
    own, and each path a context of its own. A round, written by the `write_` methods and
    `fetch` in order, with collections between them, holds contents:

    - that only `local` commits, or only `pushing` commits and pushes;
    - that `pushing` pushes and collection evicts before `local` commits them, so local-only
      entries store a payload for durable entries of another partition that hold none;
    - that `local` commits before `pushing` commits and pushes them, so durable entries share
      the payload of local-only entries of another partition;
    - that `pushing` and `mirror` both push, so durable entries of two partitions share a
      payload, and that `mirror` pushes and collection evicts before `pushing` pushes them;
    - that `pushing` pushes, collection evicts and `pushing` fetches again with `--cache`
      before `local` commits them, so local-only entries share a fetched payload;
    - at two paths of `pushing`, so under two contexts, that `local` commits at a third;
    - at two paths of `local`, that `pushing` commits and pushes at a third path, and commits
      at a fourth it pushes only the next round, so one partition holds a durable and a
      local-only entry of it;
    - that `pushing` pushes and collection evicts, which `pushing` commits again at another
      path."""

    def __init__(self, new_lore_repo):
        self.pushing: Lore = new_lore_repo(create_repo=False)
        self.pushing.shared_store_create(self.pushing.remote)
        self.pushing.repository_create(use_shared_store=True)
        self.mirror: Lore = new_lore_repo(create_repo=False)
        self.mirror.repository_create(use_shared_store=True)
        self.local: Lore = new_lore_repo(create_repo=False)
        self.local.repository_create(use_shared_store=True)
        for repo in (self.pushing, self.mirror, self.local):
            _set_store_caps(repo, max_size="1048576", max_capacity="1000")
        self.store_root = _shared_store_path(self.pushing)
        self._windows: dict[tuple[str, str], float] = {}
        self.local_files: dict[str, str] = {}
        self.pushed_files: dict[str, str] = {}
        self.mirrored_files: dict[str, str] = {}
        self._pushed: list[bytes] = []
        self._pushed_first: list[bytes] = []
        self._local_first: list[bytes] = []
        self._mirror_first: list[bytes] = []
        self._pushing_content = self._local_content = b""

    def write_first(self, round_index: int) -> None:
        """Write the contents `pushing` and `mirror` commit and push before collection."""
        name = str(round_index)
        self._pushed = _random_contents(8)
        self._pushed_first = _random_contents(3)
        self._mirror_first = _random_contents(3)
        mirrored = _random_contents(3)
        self._pushing_content, self._local_content = _random_contents(2)
        self.pushed_files = (
            _write_files(self.pushing, os.path.join("pushed", name), self._pushed)
            | _write_files(
                self.pushing, os.path.join("pushed-first", name), self._pushed_first
            )
            | _write_files(
                self.pushing,
                os.path.join("contexts", name),
                [self._pushing_content] * 2,
            )
            | _write_files(self.pushing, os.path.join("mirrored", name), mirrored)
        )
        self.mirrored_files = _write_files(
            self.mirror, os.path.join("mirrored", name), mirrored
        ) | _write_files(
            self.mirror, os.path.join("mirror-first", name), self._mirror_first
        )

    def fetch(self, round_index: int) -> None:
        """Restore two of the files `pushing` pushed this round with `--cache`, which stores the
        payloads it fetches."""
        directory = os.path.join("pushed", str(round_index))
        _assert_files_restore(
            self.pushing,
            {
                path: digest
                for path, digest in self.pushed_files.items()
                if os.path.dirname(path) == directory
                and os.path.basename(path) in ("0.uasset", "1.uasset")
            },
            cache=True,
        )

    def write_local(self, round_index: int) -> None:
        """Write the contents `local` commits."""
        name = str(round_index)
        self._local_first = _random_contents(3)
        self.local_files |= (
            _write_files(self.local, os.path.join("local", name), _random_contents(6))
            | _write_files(
                self.local, os.path.join("pushed-first", name), self._pushed_first
            )
            | _write_files(
                self.local, os.path.join("local-first", name), self._local_first
            )
            | _write_files(self.local, os.path.join("fetched", name), self._pushed[:2])
            | _write_files(
                self.local,
                os.path.join("contexts", name),
                [self._local_content] * 2 + [self._pushing_content],
            )
        )

    def write_second(self, round_index: int) -> None:
        """Write the contents `pushing` commits and pushes after `local` commits."""
        name = str(round_index)
        self.pushed_files |= (
            _write_files(
                self.pushing, os.path.join("local-first", name), self._local_first
            )
            | _write_files(
                self.pushing, os.path.join("mirror-first", name), self._mirror_first
            )
            | _write_files(
                self.pushing,
                os.path.join("contexts", f"{name}-local"),
                [self._local_content],
            )
            | _write_files(
                self.pushing, os.path.join("more", name), _random_contents(40)
            )
        )

    def write_pending(self, round_index: int) -> None:
        """Write the contents `pushing` commits and pushes only the next round."""
        self.pushed_files |= _write_files(
            self.pushing,
            os.path.join("pending", str(round_index)),
            [self._local_content, self._pushed[2]],
        )

    def assert_intact(self) -> None:
        """Require every repository to verify, every file `local` committed and every file the
        others committed this round to restore with its committed content, and every entry of
        the store's index to be reachable."""
        for repo, files in (
            (self.local, self.local_files),
            (self.pushing, self.pushed_files),
            (self.mirror, self.mirrored_files),
        ):
            repo.repository_verify()
            _assert_files_restore(repo, files)
        findings = unreachable_index_entries(os.path.join(self.store_root, "immutable"))
        assert not findings, "\n".join(findings)

    def assert_files_whole(self) -> None:
        """Require every bucket and level file of both stores to be one a whole write left,
        before any command opens the store and resets a torn one."""
        for store in ("immutable", "mutable"):
            findings = torn_index_files(os.path.join(self.store_root, store))
            assert not findings, "\n".join(findings)

    def killed(self, repo: Lore, args: list[str], within: float) -> bool:
        """Run lore with `args` in `repo`, kill it at a random point within `within` seconds
        unless it finishes first, and require the store's files whole. Returns whether it was
        killed."""
        killed = _kill_at_random(
            [repo.lore_executable_path, "--repository", repo.path, *args],
            repo.sandboxed_env(),
            within,
        )
        self.assert_files_whole()
        return killed

    def killed_step(self, repo: Lore, step: str, args: list[str]) -> bool:
        """As `killed`, within 1.5 times the last run of `step` in `repo` that finished."""
        key = (repo.path, step)
        start = time.monotonic()
        killed = self.killed(repo, args, self._windows.get(key, 0.5) * 1.5)
        if not killed:
            self._windows[key] = max(time.monotonic() - start, 0.1)
        return killed

    def commit_killed(self, repo: Lore, message: str) -> bool:
        """Stage every change in `repo` and commit it with `--local` by a command killed at a
        random point, committing again if it was killed, and require the revision committed.
        Returns whether it was killed."""
        repo.stage(scan=True)
        step = message.split()[0]
        killed = self.killed_step(repo, step, ["--local", "commit", message])
        if killed:
            _commit(repo, message, check=False)
        assert repo.history(length=1)[0].message.strip() == message
        return killed

    def push_killed(self, repo: Lore) -> bool:
        """Push `repo` with a command killed at a random point, pushing again if it was killed,
        and require the branch in sync with the remote. Returns whether it was killed."""
        killed = self.killed_step(repo, "push", ["push"])
        if killed:
            repo.push(max_connections=16, check=False)
        assert "in sync with remote" in repo.status()
        return killed


def _shared_store_path(repo: Lore) -> str:
    """The path of the shared store `repo` uses."""
    stores = repo.shared_store_info().stores
    assert len(stores) == 1, stores
    return next(iter(stores.values())).path


def _commit(repo: Lore, message: str, **kwargs) -> None:
    """Stage every change in `repo` and commit it with `--local`."""
    repo.stage(scan=True)
    repo.commit(message, local=True, **kwargs)


def _seed_random() -> None:
    """Seed `random` from `LORE_KILL_SEED`, or from the clock, and log the seed, so that the kill
    points of a failed run can be aimed at again."""
    seed = int(os.environ.get("LORE_KILL_SEED", time.time_ns()))
    random.seed(seed)
    logger.info("Kill seed %d", seed)


def _kill_at_random(command: list[str], env: dict[str, str], within: float) -> bool:
    """Run `command` and kill it with SIGKILL at a random point within `within` seconds, unless
    it finishes first, which it must do successfully. Returns whether it was killed."""
    with tempfile.TemporaryFile() as output:
        process = subprocess.Popen(command, env=env, stdout=output, stderr=output)
        try:
            process.wait(timeout=random.uniform(0.0, within))
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            return True
        output.seek(0)
        assert process.returncode == 0, (
            f"{command} failed with {process.returncode}:\n"
            + output.read().decode(errors="replace")
        )
        return False


@pytest.mark.slow
@pytest.mark.runs_in_process("kills the processes carrying out collections")
def test_killed_repository_gc_loses_no_local_content(new_lore_repo):
    """`repository gc` over a shared store, killed at any point of eviction or compaction,
    loses no local-only content, leaves every index file whole and no entry pointing at the
    wrong bytes, with contents deduplicated across partitions and contexts whose entries differ
    in durability and in holding a payload, as `_DeduplicatedStore` describes. Each round
    collects after the first pushes, and kills collections after the second, which orphans
    payloads beside local-only ones, so compaction rewrites packfiles holding local-only
    content when the kill lands."""
    _seed_random()
    store = _DeduplicatedStore(new_lore_repo)
    kills = 0
    for round_index in range(20):
        gc_repo = (store.local, store.pushing, store.mirror)[round_index % 3]
        store.write_first(round_index)
        for repo in (store.pushing, store.mirror):
            _commit(repo, f"First {round_index}", no_gc=True)
            repo.push(max_connections=16, no_gc=True)
        start = time.monotonic()
        out = gc_repo.repository_gc(json=True)
        full_pass = time.monotonic() - start
        assert parse_jsonl(out, "compactionBegin"), "the full pass compacts"
        store.fetch(round_index)
        store.write_local(round_index)
        _commit(store.local, f"Local {round_index}", no_gc=True)
        store.write_second(round_index)
        _commit(store.pushing, f"Second {round_index}", no_gc=True)
        store.pushing.push(max_connections=16, no_gc=True)
        store.write_pending(round_index)
        _commit(store.pushing, f"Pending {round_index}", no_gc=True)

        for _ in range(4):
            kills += store.killed(
                gc_repo, ["repository", "gc"], max(full_pass, 0.5) * 1.5
            )
        store.assert_intact()

    assert kills >= 16, f"only {kills} of 80 passes were killed"
    store.local.repository_gc()
    store.assert_intact()


@pytest.mark.slow
@pytest.mark.runs_in_process("kills the processes carrying out commands")
def test_killed_commands_lose_no_local_content(new_lore_repo):
    """`commit` and `push` over a shared store, killed at any point, including the garbage
    collection they start, lose no local-only content and leave every index file whole, and the
    next run of the command completes their work. The caps are low enough that every command
    evicts and compacts, and the contents are deduplicated across partitions and contexts as
    `_DeduplicatedStore` describes."""
    _seed_random()
    store = _DeduplicatedStore(new_lore_repo)
    kills = 0
    for round_index in range(24):
        store.write_first(round_index)
        for repo in (store.pushing, store.mirror):
            kills += store.commit_killed(repo, f"First {round_index}")
            kills += store.push_killed(repo)
        store.fetch(round_index)
        store.write_local(round_index)
        kills += store.commit_killed(store.local, f"Local {round_index}")
        store.write_second(round_index)
        kills += store.commit_killed(store.pushing, f"Second {round_index}")
        kills += store.push_killed(store.pushing)
        store.write_pending(round_index)
        kills += store.commit_killed(store.pushing, f"Pending {round_index}")
        store.assert_intact()

    assert kills >= 48, f"only {kills} of 192 commands were killed"


def _store_files(repo: Lore) -> dict[str, bytes]:
    store = Path(repo.dot_path()) / "immutable"
    return {
        str(path.relative_to(store)): path.read_bytes()
        for path in store.rglob("*")
        if path.is_file()
    }


@pytest.mark.smoke
def test_repository_gc_dry_run_collects_nothing(new_lore_repo):
    """`repository gc --dry-run` leaves a store that is over its caps as it was, where
    `repository gc` evicts and compacts it."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo, count=64)
    repo.push(max_connections=16)
    _set_store_caps(repo, max_size="100", max_capacity="100")
    store = _store_files(repo)

    out = repo.repository_gc(dry_run=True)

    assert "Garbage collection skipped in a dry run" in out, out
    assert _store_files(repo) == store, "a dry run collected garbage"

    repo.repository_gc()
    assert _store_files(repo) != store


@pytest.mark.smoke
@pytest.mark.parametrize("dry_run", [True, False])
def test_repository_gc_failure_reports_no_result(new_lore_repo, dry_run):
    """`repository gc` that fails, here on an unreadable config, reports neither a skipped
    nor a completed collection."""
    repo: Lore = new_lore_repo()
    _seed_committed_data(repo)
    Path(repo.dot_path(), "config.toml").write_text("not = [valid", encoding="utf-8")

    out = repo.run(["repository", "gc"], dry_run=dry_run, check=False)

    assert "Garbage collection skipped" not in out, out
    assert "Garbage collection complete" not in out, out
