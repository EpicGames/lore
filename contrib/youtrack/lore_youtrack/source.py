# SPDX-FileCopyrightText: 2026 RedRatInHat
# SPDX-License-Identifier: MIT
"""Read remote Lore history through the 0.12 JSONL CLI contract.

Only repository/branch metadata and revision headers are requested. The caller
owns a dedicated bare clone; this module never switches branches or checks out
file contents. Unknown output schemas fail before any delivery is produced.
"""

from __future__ import annotations

import json
import math
import os
import re
import signal
import subprocess
import threading
from collections.abc import Callable, Collection, Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO

ZERO_HASH = "0" * 64
_MAX_ROOTS = 100000
_HASH = re.compile(r"[0-9a-fA-F]{64}\Z")
_ID = re.compile(r"[0-9a-fA-F]{32}\Z")


class SourceError(Exception):
    """An incomplete or invalid source read; messages never include CLI output."""


@dataclass(frozen=True)
class Repository:
    id: str
    remote_url: str


@dataclass(frozen=True)
class Branch:
    id: str
    name: str
    head: str
    heads: tuple[str, ...]


@dataclass(frozen=True)
class Revision:
    id: str
    parents: tuple[str, ...]
    author: str
    message: str
    timestamp_ms: int


def _hash(value: Any) -> str:
    if not isinstance(value, str) or not _HASH.fullmatch(value):
        raise SourceError("Lore returned an invalid revision hash")
    return value.lower()


def _id(value: Any) -> str:
    if not isinstance(value, str) or not _ID.fullmatch(value) or int(value, 16) == 0:
        raise SourceError("Lore returned an invalid repository or branch identifier")
    return value.lower()


def _text(value: Any, *, empty: bool = False) -> str:
    if not isinstance(value, str) or (not empty and not value.strip()):
        raise SourceError("Lore returned a missing or invalid text field")
    if "\x00" in value:
        raise SourceError("Lore returned a text field containing NUL")
    return value


def _integer(value: Any) -> int:
    if type(value) is not int or value < 0:
        raise SourceError("Lore returned a missing or invalid integer field")
    return value


def _object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise SourceError("Lore returned JSON with duplicate fields")
        result[key] = value
    return result


def _invalid_constant(_value: str) -> None:
    raise SourceError("Lore returned invalid JSON")


def _parse_events(
    output: bytes, *, allow_display_lookup: bool = False
) -> list[dict[str, Any]]:
    try:
        text = output.decode("utf-8")
    except UnicodeDecodeError:
        raise SourceError("Lore returned non-UTF-8 output") from None
    events: list[dict[str, Any]] = []
    completed = False
    display_completed = False
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            event = json.loads(
                line, object_pairs_hook=_object, parse_constant=_invalid_constant
            )
        except (ValueError, RecursionError):
            raise SourceError("Lore returned invalid JSONL") from None
        if (
            not isinstance(event, dict)
            or not isinstance(event.get("tagName"), str)
            or not isinstance(event.get("data"), dict)
        ):
            raise SourceError("Lore returned an unsupported event schema")
        tag = event["tagName"]
        if completed:
            if tag == "log":
                continue
            # 0.12's handle_revision_info runs resolve_revision_user_ids after
            # its successful command, even with --json. That display lookup
            # has its own completion and may fail on an unauthenticated server.
            # Only the already-completed primary operation supplies source data.
            if allow_display_lookup and not display_completed:
                if tag == "authUserInfo":
                    continue
                if tag == "complete" and type(event["data"].get("status")) is int:
                    display_completed = True
                    continue
            raise SourceError("Lore returned unexpected events after completion")
        if tag == "complete":
            status = event["data"].get("status")
            if type(status) is not int or status != 0:
                raise SourceError("Lore reported a failed operation")
            completed = True
        elif tag == "error":
            raise SourceError("Lore reported a source error")
        events.append(event)
    if not completed:
        raise SourceError("Lore output ended without successful completion")
    return events


def _only(events: list[dict[str, Any]], tag: str) -> dict[str, Any]:
    values = [event["data"] for event in events if event["tagName"] == tag]
    if len(values) != 1:
        raise SourceError("Lore returned a missing or repeated result event")
    return values[0]


def _kill(process: subprocess.Popen[bytes]) -> None:
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
    except ProcessLookupError:
        pass


def _bounded_run(args: list[str], timeout: float, max_bytes: int) -> bytes:
    """Bound both output pipes, including a noisy stderr, before decoding."""
    try:
        process = subprocess.Popen(
            args,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            shell=False,
            start_new_session=(os.name == "posix"),
        )
    except (OSError, ValueError):
        raise SourceError("Could not start Lore CLI") from None

    chunks: list[bytes] = []
    total = 0
    lock = threading.Lock()
    exceeded = threading.Event()
    read_failed = threading.Event()

    def read(pipe: BinaryIO, keep: bool) -> None:
        nonlocal total
        try:
            with pipe:
                while chunk := pipe.read(8192):
                    with lock:
                        total += len(chunk)
                        if total > max_bytes:
                            exceeded.set()
                        elif keep:
                            chunks.append(chunk)
                    if exceeded.is_set():
                        _kill(process)
                        return
        except OSError:
            read_failed.set()
            _kill(process)

    # Popen supplies both handles because both were requested as PIPE.
    if process.stdout is None or process.stderr is None:
        _kill(process)
        process.wait()
        raise SourceError("Could not capture Lore CLI output")
    readers = [
        threading.Thread(target=read, args=(process.stdout, True), daemon=True),
        threading.Thread(target=read, args=(process.stderr, False), daemon=True),
    ]
    for reader in readers:
        reader.start()
    timed_out = False
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        _kill(process)
        process.wait()
    finally:
        for reader in readers:
            reader.join(timeout=2)
    if timed_out:
        raise SourceError("Lore CLI exceeded its read timeout")
    if exceeded.is_set():
        raise SourceError("Lore CLI exceeded its output limit")
    if read_failed.is_set() or any(reader.is_alive() for reader in readers):
        _kill(process)
        raise SourceError("Could not finish reading Lore CLI output")
    if process.returncode != 0:
        raise SourceError("Lore CLI exited unsuccessfully")
    return b"".join(chunks)


class LoreSource:
    """Read the remote view of a dedicated Lore bare clone."""

    def __init__(
        self,
        binary: str | os.PathLike[str],
        repository_path: str | os.PathLike[str],
        timeout: float = 30,
        max_output_bytes: int = 8 * 1024 * 1024,
    ) -> None:
        if (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or not math.isfinite(timeout)
            or timeout <= 0
        ):
            raise SourceError("Lore CLI timeout must be positive and finite")
        if type(max_output_bytes) is not int or max_output_bytes <= 0:
            raise SourceError("Lore CLI output limit must be a positive integer")
        self.binary = os.fspath(binary)
        self.repository_path = str(Path(repository_path).resolve())
        self.timeout = timeout
        self.max_output_bytes = max_output_bytes
        self._repository_id: str | None = None

    def _events(self, *command: str) -> list[dict[str, Any]]:
        output = _bounded_run(
            [
                self.binary,
                "--json",
                "--non-interactive",
                "--no-pager",
                "--remote",
                "--repository",
                self.repository_path,
                *command,
            ],
            self.timeout,
            self.max_output_bytes,
        )
        return _parse_events(
            output, allow_display_lookup=command[:2] == ("revision", "info")
        )

    def repository(self) -> Repository:
        """Return and bind the actual remote repository identity."""
        data = _only(self._events("repository", "info"), "repositoryData")
        result = Repository(_id(data.get("id")), _text(data.get("remoteUrl")))
        if self._repository_id is not None and self._repository_id != result.id:
            raise SourceError("Lore repository identity changed during the scan")
        self._repository_id = result.id
        return result

    def branches(self) -> list[Branch]:
        """List current remote branches, with complete-list validation."""
        events = self._events("branch", "list")
        begin = _only(events, "branchListBegin")
        end = _only(events, "branchListEnd")
        if begin.get("location") != "remote" or end.get("location") != "remote":
            raise SourceError("Lore did not return a remote branch listing")
        branches: list[Branch] = []
        ids: set[str] = set()
        names: set[str] = set()
        begun = False
        ended = False
        for event in events:
            tag, data = event["tagName"], event["data"]
            if tag == "branchListBegin":
                begun = True
            elif tag == "branchListEnd":
                if not begun:
                    raise SourceError("Lore returned a branch listing out of order")
                ended = True
            elif tag == "branchListEntry":
                if not begun or ended or data.get("location") != "remote":
                    raise SourceError(
                        "Lore returned an incomplete remote branch listing"
                    )
                branch_id = _id(data.get("id"))
                name = _text(data.get("name"))
                head = _hash(data.get("latest"))
                stack = data.get("stack")
                if not isinstance(stack, list):
                    raise SourceError("Lore returned an unsupported branch stack")
                heads = [head]
                for point in stack:
                    if not isinstance(point, dict):
                        raise SourceError("Lore returned an unsupported branch point")
                    _id(point.get("branch"))
                    heads.append(_hash(point.get("revision")))
                if branch_id in ids or name in names:
                    raise SourceError("Lore returned duplicate remote branches")
                ids.add(branch_id)
                names.add(name)
                branches.append(
                    Branch(
                        branch_id,
                        name,
                        head,
                        tuple(dict.fromkeys(h for h in heads if h != ZERO_HASH)),
                    )
                )
        if _integer(end.get("count")) != len(branches):
            raise SourceError("Lore returned an incomplete remote branch listing")
        return sorted(branches, key=lambda item: (item.name, item.id))

    def revision(self, revision_hash: str) -> Revision:
        """Read complete original-author metadata for an exact revision."""
        revision_hash = _hash(revision_hash)
        if revision_hash == ZERO_HASH:
            raise SourceError("The zero hash does not identify a Lore revision")
        if self._repository_id is None:
            self.repository()
        events = self._events("revision", "info", revision_hash)
        data = _only(events, "revisionInfo")
        if (
            _hash(data.get("revision")) != revision_hash
            or _id(data.get("repository")) != self._repository_id
        ):
            raise SourceError("Lore returned a different revision or repository")
        parents = data.get("parent")
        if not isinstance(parents, list) or len(parents) != 2:
            raise SourceError("Lore returned an unsupported revision parent schema")
        parent_hashes = tuple(
            dict.fromkeys(_hash(parent) for parent in parents if parent != ZERO_HASH)
        )
        metadata: dict[str, Any] = {}
        for event in events:
            if event["tagName"] != "metadata":
                continue
            entry = event["data"]
            key = entry.get("key")
            if not isinstance(key, str):
                raise SourceError("Lore returned an invalid metadata key")
            if key not in {"message", "timestamp", "created-by", "committed-by"}:
                continue
            value = entry.get("value")
            if key in metadata or not isinstance(value, dict):
                raise SourceError("Lore returned missing or repeated revision metadata")
            expected = "numeric" if key == "timestamp" else "string"
            if value.get("tagName") != expected or "data" not in value:
                raise SourceError("Lore returned an unsupported metadata schema")
            metadata[key] = (
                _integer(value["data"])
                if key == "timestamp"
                else _text(value["data"], empty=True)
            )
        message = _text(metadata.get("message"), empty=True)
        timestamp = _integer(metadata.get("timestamp"))
        if timestamp > 2**63 - 1:
            raise SourceError(
                "Lore revision timestamp exceeds the YouTrack integer range"
            )
        author = _text(metadata.get("created-by") or metadata.get("committed-by"))
        return Revision(revision_hash, parent_hashes, author, message, timestamp)

    def unseen_revisions(
        self,
        head: str | Iterable[str],
        known: Collection[str] | Callable[[str], bool],
        max_revisions: int = 10000,
        max_total_bytes: int = 32 * 1024 * 1024,
    ) -> list[Revision]:
        """Walk both parents in postorder; return nothing partial on failure.

        A known hash must imply its ancestors were already discovered. The
        caller persists discovery atomically before marking any hash known.
        Root and parent order is retained, giving deterministic first-seen
        attribution when the caller scans selected branches in sorted order.
        """
        if type(max_revisions) is not int or max_revisions <= 0:
            raise SourceError("The revision scan limit must be a positive integer")
        if type(max_total_bytes) is not int or max_total_bytes <= 0:
            raise SourceError("The metadata scan limit must be a positive integer")
        is_known = known if callable(known) else known.__contains__
        roots: list[str] = []
        for root in [head] if isinstance(head, str) else head:
            roots.append(_hash(root))
            if len(roots) > _MAX_ROOTS:
                raise SourceError("Lore history exceeded the root scan limit")
        work = [(root, False) for root in reversed(roots) if root != ZERO_HASH]
        state: dict[str, int] = {}
        revisions: dict[str, Revision] = {}
        result: list[Revision] = []
        total_bytes = 0
        while work:
            revision_hash, exiting = work.pop()
            if exiting:
                state[revision_hash] = 2
                result.append(revisions[revision_hash])
                continue
            if state.get(revision_hash) == 1:
                raise SourceError("Lore history contains a parent cycle")
            if state.get(revision_hash) == 2 or is_known(revision_hash):
                continue
            if len(revisions) >= max_revisions:
                raise SourceError("Lore history exceeded the revision scan limit")
            revision = self.revision(revision_hash)
            total_bytes += (
                len(revision.author.encode("utf-8"))
                + len(revision.message.encode("utf-8"))
                + 64 * (len(revision.parents) + 1)
                + 64
            )
            if total_bytes > max_total_bytes:
                raise SourceError("Lore history exceeded the metadata scan limit")
            revisions[revision_hash] = revision
            state[revision_hash] = 1
            work.append((revision_hash, True))
            work.extend((parent, False) for parent in reversed(revision.parents))
        return result
