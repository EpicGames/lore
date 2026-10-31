# Copyright 2026 RedRatInHat
# SPDX-License-Identifier: MIT
"""A single-writer durable outbox with conservative crash recovery."""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import time
from collections.abc import Iterable, Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any


class StoreError(RuntimeError):
    """A local state or ownership problem."""


def _status(connection: sqlite3.Connection) -> dict:
    counts = dict(
        connection.execute("SELECT state,COUNT(*) FROM deliveries GROUP BY state")
    )
    problems = [
        dict(row)
        for row in connection.execute(
            "SELECT sequence,revision,branch,state,attempts,http_status,detail FROM deliveries "
            "WHERE state IN ('unknown','blocked','in_flight') ORDER BY sequence LIMIT 100"
        )
    ]
    return {"counts": counts, "problems": problems}


def read_status(database: Path, receiver_url: str) -> dict:
    """Read a consistent snapshot while the daemon runs, without crash recovery."""
    if not database.exists():
        raise StoreError("The database does not exist; scan or baseline first")
    connection = sqlite3.connect(
        database.resolve().as_uri() + "?mode=ro", uri=True, timeout=5
    )
    connection.row_factory = sqlite3.Row
    try:
        connection.execute("BEGIN")
        if connection.execute("PRAGMA user_version").fetchone()[0] != 1:
            raise StoreError("Unsupported bridge database version")
        receiver = connection.execute(
            "SELECT value FROM metadata WHERE key='receiver'"
        ).fetchone()
        if (
            receiver is None
            or receiver[0] != hashlib.sha256(receiver_url.encode()).hexdigest()
        ):
            raise StoreError(
                "Database receiver binding does not match this configuration"
            )
        return _status(connection)
    finally:
        connection.close()


@contextmanager
def process_lock(database: Path) -> Iterator[None]:
    """Keep one process in charge; an OS-released lock also covers recovery."""
    database.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor = os.open(str(database) + ".lock", os.O_RDWR | os.O_CREAT, 0o600)
    with os.fdopen(descriptor, "r+b") as stream:
        if os.name == "nt":
            import msvcrt

            if os.fstat(stream.fileno()).st_size == 0:
                stream.write(b"\0")
                stream.flush()
            stream.seek(0)
            try:
                msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
            except OSError as error:
                raise StoreError("Another bridge process owns this database") from error
        else:
            import fcntl

            try:
                fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError as error:
                raise StoreError("Another bridge process owns this database") from error
        try:
            yield
        finally:
            if os.name == "nt":
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(stream.fileno(), fcntl.LOCK_UN)


class Store:
    """Use only while holding process_lock for the entire command lifetime."""

    def __init__(self, database: Path, receiver_url: str):
        if not database.exists():
            descriptor = os.open(database, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            os.close(descriptor)
        self.connection = sqlite3.connect(database, timeout=5)
        self.connection.row_factory = sqlite3.Row
        try:
            self.connection.execute("PRAGMA journal_mode=WAL")
            self.connection.execute("PRAGMA synchronous=FULL")
            self.connection.execute("PRAGMA foreign_keys=ON")
            version = self.connection.execute("PRAGMA user_version").fetchone()[0]
            if version not in (0, 1):
                raise StoreError("Unsupported bridge database version")
            self.connection.executescript("""
                CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS revisions (
                    id TEXT PRIMARY KEY,
                    parents TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS deliveries (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    revision TEXT NOT NULL UNIQUE REFERENCES revisions(id),
                    branch TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN (
                        'pending','in_flight','accepted','unknown','blocked','reconciled','skipped'
                    )),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    next_attempt_at REAL NOT NULL DEFAULT 0,
                    http_status INTEGER,
                    detail TEXT NOT NULL DEFAULT '',
                    updated_at REAL NOT NULL
                );
                PRAGMA user_version=1;
            """)
            self.bind("receiver", hashlib.sha256(receiver_url.encode()).hexdigest())
            with self.connection:
                self.connection.execute(
                    "UPDATE deliveries SET state='unknown', detail=?, updated_at=? WHERE state='in_flight'",
                    (
                        "Process stopped during an attempt; delivery outcome is unknown",
                        time.time(),
                    ),
                )
        except Exception:
            self.connection.close()
            raise

    def close(self) -> None:
        self.connection.close()

    def bind(self, key: str, value: str) -> None:
        """Prevent a different source or receiver from reusing this ledger."""
        with self.connection:
            previous = self.connection.execute(
                "SELECT value FROM metadata WHERE key=?", (key,)
            ).fetchone()
            if previous is not None and previous[0] != value:
                raise StoreError(
                    f"Database {key} binding does not match this configuration"
                )
            self.connection.execute(
                "INSERT OR IGNORE INTO metadata VALUES (?, ?)", (key, value)
            )

    def knows(self, revision: str) -> bool:
        return (
            self.connection.execute(
                "SELECT 1 FROM revisions WHERE id=?", (revision,)
            ).fetchone()
            is not None
        )

    def enqueue(
        self, records: Iterable[tuple[Any, str, dict]], *, baseline: bool = False
    ) -> int:
        """Commit the complete discovered ancestry and immutable payloads together."""
        count = 0
        with self.connection:
            if (
                baseline
                and self.connection.execute(
                    "SELECT 1 FROM revisions LIMIT 1"
                ).fetchone()
            ):
                raise StoreError("Baseline is allowed only on a new, empty ledger")
            for revision, branch, payload in records:
                if self.knows(revision.id):
                    continue
                if any(not self.knows(parent) for parent in revision.parents):
                    raise StoreError(
                        "Discovery must include every parent before its child"
                    )
                self.connection.execute(
                    "INSERT INTO revisions VALUES (?, ?)",
                    (revision.id, json.dumps(revision.parents)),
                )
                self.connection.execute(
                    "INSERT INTO deliveries (revision,branch,payload,state,updated_at) VALUES (?,?,?,?,?)",
                    (
                        revision.id,
                        branch,
                        json.dumps(payload, ensure_ascii=False, separators=(",", ":")),
                        "skipped" if baseline else "pending",
                        time.time(),
                    ),
                )
                count += 1
        return count

    def claim(self, now: float | None = None) -> dict | None:
        """Persist in_flight before HTTP; unresolved earlier records hold the queue."""
        now = time.time() if now is None else now
        with self.connection:
            row = self.connection.execute(
                "SELECT * FROM deliveries WHERE state NOT IN ('accepted','reconciled','skipped') ORDER BY sequence LIMIT 1"
            ).fetchone()
            if row is None or row["state"] != "pending" or row["next_attempt_at"] > now:
                return None
            self.connection.execute(
                "UPDATE deliveries SET state='in_flight',attempts=attempts+1,updated_at=? WHERE sequence=? AND state='pending'",
                (now, row["sequence"]),
            )
        result = dict(row)
        result["payload"] = json.loads(result["payload"])
        result["attempts"] += 1
        return result

    def finish(self, sequence: int, result: Any) -> None:
        state = {
            "accepted": "accepted",
            "retry": "pending",
            "blocked": "blocked",
            "unknown": "unknown",
        }.get(result.state)
        if state is None:
            raise StoreError("Unrecognized delivery outcome")
        with self.connection:
            row = self.connection.execute(
                "SELECT state,attempts FROM deliveries WHERE sequence=?", (sequence,)
            ).fetchone()
            if row is None or row["state"] != "in_flight":
                raise StoreError("Only the current in-flight attempt can be completed")
            delay = (
                min(300, 2 ** min(row["attempts"], 9)) if result.state == "retry" else 0
            )
            self.connection.execute(
                "UPDATE deliveries SET state=?,http_status=?,detail=?,next_attempt_at=?,updated_at=? WHERE sequence=?",
                (
                    state,
                    result.http_status,
                    result.detail,
                    time.time() + delay,
                    time.time(),
                    sequence,
                ),
            )

    def unknown(self, limit: int = 100) -> list[dict]:
        rows = self.connection.execute(
            "SELECT * FROM deliveries WHERE state='unknown' ORDER BY sequence LIMIT ?",
            (limit,),
        )
        return [{**dict(row), "payload": json.loads(row["payload"])} for row in rows]

    def reconciled(self, sequence: int, detail: str) -> None:
        with self.connection:
            cursor = self.connection.execute(
                "UPDATE deliveries SET state='reconciled',detail=?,updated_at=? WHERE sequence=? AND state='unknown'",
                (detail, time.time(), sequence),
            )
            if cursor.rowcount != 1:
                raise StoreError("Only an unknown delivery can be reconciled")

    def retry_blocked(self, revision: str) -> None:
        """An explicit operator action; unknown attempts have no retry operation."""
        with self.connection:
            cursor = self.connection.execute(
                "UPDATE deliveries SET state='pending',next_attempt_at=0,detail=?,updated_at=? "
                "WHERE revision=? AND state='blocked' AND http_status IN (400,401,403,404,405,413,415,422)",
                (
                    "Operator requested retry after correcting a rejected request",
                    time.time(),
                    revision,
                ),
            )
            if cursor.rowcount != 1:
                raise StoreError(
                    "Only an explicitly rejected delivery can be retried; UNKNOWN cannot"
                )

    def status(self) -> dict:
        return _status(self.connection)
