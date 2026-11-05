# Copyright 2026 RedRatInHat
# SPDX-License-Identifier: MIT
"""Command line entry point for the optional YouTrack bridge."""

from __future__ import annotations

import argparse
import json
import signal
import sqlite3
import sys
import threading

from . import __version__
from .bridge import deliver, reconcile, scan
from .config import ConfigError, load_config
from .source import LoreSource, SourceError
from .store import Store, StoreError, process_lock, read_status
from .transport import YouTrackTransport


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Export Lore revisions to YouTrack Generic VCS"
    )
    parser.add_argument("--version", action="version", version=__version__)
    parser.add_argument(
        "--config",
        required=True,
        help="Administrator-owned TOML file outside the Lore repository",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    for name, help_text in (
        ("scan", "Persist reachable revisions without sending them"),
        (
            "baseline",
            "Record existing history without delivering it; requires an empty ledger",
        ),
        ("sync", "Scan and deliver one bounded batch"),
        ("run", "Continuously scan, reconcile, and deliver"),
        ("status", "Show local delivery counts and unresolved attempts"),
        ("reconcile", "Read YouTrack to resolve already attached UNKNOWN revisions"),
    ):
        commands.add_parser(name, help=help_text)
    retry = commands.add_parser(
        "retry-blocked",
        help="Retry an explicitly rejected request after correcting its configuration",
    )
    retry.add_argument("revision")
    args = parser.parse_args(argv)
    stop = threading.Event()
    if args.command == "run":
        signal.signal(signal.SIGTERM, lambda *_: stop.set())
        signal.signal(signal.SIGINT, lambda *_: stop.set())
    try:
        config = load_config(args.config)
        # Validate the destination before binding a new database. No connection is made here.
        transport = YouTrackTransport(
            config.receiver_url,
            config.signature.read()
            if args.command in {"sync", "run"}
            else "configuration-check",
            timeout=config.request_timeout,
            allow_http_loopback=config.allow_http_loopback,
        )
        if args.command == "status":
            print(
                json.dumps(
                    read_status(config.database, config.receiver_url), sort_keys=True
                )
            )
            return 0
        with process_lock(config.database):
            store = Store(config.database, config.receiver_url)
            try:
                if args.command == "retry-blocked":
                    store.retry_blocked(args.revision)
                    print(json.dumps(store.status(), sort_keys=True))
                    return 0
                if args.command == "reconcile":
                    if config.read_token is None:
                        raise ConfigError(
                            "Configure a read token and commit URL before reconciliation"
                        )
                    resolved = reconcile(config, store, transport)
                    print(
                        json.dumps(
                            {"reconciled": resolved, **store.status()}, sort_keys=True
                        )
                    )
                    return 2 if store.status()["problems"] else 0
                source = LoreSource(
                    config.lore_binary, config.repository, timeout=config.lore_timeout
                )
                while not stop.is_set():
                    try:
                        enqueued = scan(
                            config, source, store, baseline=args.command == "baseline"
                        )
                        resolved = (
                            reconcile(config, store, transport)
                            if args.command in {"sync", "run"}
                            else 0
                        )
                        accepted = (
                            deliver(config, store, transport)
                            if args.command in {"sync", "run"}
                            else 0
                        )
                        status = store.status()
                        print(
                            json.dumps(
                                {
                                    "discovered": enqueued,
                                    "accepted": accepted,
                                    "reconciled": resolved,
                                    **status,
                                },
                                sort_keys=True,
                            ),
                            flush=True,
                        )
                        if args.command != "run":
                            return (
                                2
                                if status["problems"]
                                or (
                                    args.command == "sync"
                                    and status["counts"].get("pending", 0)
                                )
                                else 0
                            )
                    except SourceError as error:
                        if args.command != "run":
                            raise
                        print(
                            json.dumps({"error": str(error), "phase": "source"}),
                            file=sys.stderr,
                            flush=True,
                        )
                    stop.wait(config.poll_seconds)
            finally:
                store.close()
    except (ConfigError, StoreError, SourceError, ValueError) as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        return 1
    except (OSError, sqlite3.Error):
        print(
            json.dumps(
                {
                    "error": "Local I/O failed; inspect permissions, available disk space, and database health"
                }
            ),
            file=sys.stderr,
        )
        return 1
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
