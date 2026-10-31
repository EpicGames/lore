# Copyright 2026 RedRatInHat
# SPDX-License-Identifier: MIT
"""Discovery, delivery, and read-only reconciliation of Lore revisions."""

from __future__ import annotations

import fnmatch
import json
import re

from .config import Config, ConfigError
from .source import LoreSource
from .store import Store
from .transport import YouTrackTransport

ISSUE_ID = re.compile(r"\b([A-Za-z][A-Za-z0-9_]*)-\d+\b")


def scan(
    config: Config, source: LoreSource, store: Store, *, baseline: bool = False
) -> int:
    """Read a fixed remote snapshot and enqueue each revision once, in DAG order."""
    repository = source.repository()
    store.bind("repository", repository.id)
    branches = sorted(
        (
            branch
            for branch in source.branches()
            if any(fnmatch.fnmatchcase(branch.name, p) for p in config.branches)
        ),
        key=lambda branch: (branch.name, branch.id),
    )
    if not branches:
        raise ConfigError("No remote branch matches the configured branch patterns")
    discovered: set[str] = set()
    records: list[tuple] = []
    total_bytes = 0
    for branch in branches:
        branch_name = "refs/heads/" + branch.name
        if any(ord(c) < 32 for c in branch_name):
            raise ConfigError("A remote branch name contains control characters")
        remaining = config.scan_limit - len(discovered)
        if remaining <= 0 and any(
            not (h in discovered or store.knows(h)) for h in branch.heads
        ):
            raise ConfigError("Discovery exceeds scan_limit; nothing was enqueued")
        revisions = source.unseen_revisions(
            branch.heads,
            lambda revision: revision in discovered or store.knows(revision),
            max_revisions=max(1, remaining),
        )
        for revision in revisions:
            identity = config.authors.get(revision.author)
            if identity is None and not baseline:
                raise ConfigError(
                    f"No YouTrack author mapping for Lore identity {revision.author!r}"
                )
            payload = {
                "branch": branch_name,
                "commits": [
                    {
                        "version": revision.id,
                        "date": revision.timestamp_ms,
                        "user": identity or {},
                        "comment": revision.message,
                    }
                ],
            }
            size = len(json.dumps(payload, ensure_ascii=False).encode("utf-8"))
            total_bytes += size
            if size > 1024 * 1024 or total_bytes > 32 * 1024 * 1024:
                raise ConfigError(
                    "Discovery payload limit exceeded; nothing was enqueued"
                )
            records.append((revision, branch_name, payload))
            discovered.add(revision.id)
    return store.enqueue(records, baseline=baseline)


def deliver(config: Config, store: Store, transport: YouTrackTransport) -> int:
    """Send persisted records; stop at the first unconfirmed result."""
    accepted = 0
    for _ in range(config.send_limit):
        item = store.claim()
        if item is None:
            break
        result = transport.deliver(item["payload"])
        store.finish(item["sequence"], result)
        if result.state != "accepted":
            break
        accepted += 1
    return accepted


def reconcile(config: Config, store: Store, transport: YouTrackTransport) -> int:
    """Confirm existing associations, without sending a second POST."""
    if config.read_token is None or config.commit_url_template is None:
        return 0
    token = config.read_token.read()
    resolved = 0
    for item in store.unknown(config.send_limit):
        comment = item["payload"]["commits"][0]["comment"]
        issues = sorted(
            {
                m.group(0)
                for m in ISSUE_ID.finditer(comment)
                if not config.project_keys or m.group(1) in config.project_keys
            }
        )
        if not issues:
            continue
        result = transport.reconcile(
            issues,
            revision=item["revision"],
            expected_url=config.commit_url_template.replace("$hash$", item["revision"]),
            token=token,
        )
        if result.state == "found":
            store.reconciled(
                item["sequence"],
                "REST confirmed revision and repository URL in every referenced issue; commands were not replayed",
            )
            resolved += 1
    return resolved
