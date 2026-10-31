# Copyright 2026 RedRatInHat
# SPDX-License-Identifier: MIT
"""Behavioral tests for crash recovery, identity, ancestry, and delivery order."""

import subprocess
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path

from lore_youtrack.bridge import deliver, reconcile, scan
from lore_youtrack.config import Config, ConfigError, Secret
from lore_youtrack.source import Branch, LoreSource, Repository, Revision, SourceError
from lore_youtrack.store import Store, StoreError, process_lock, read_status
from lore_youtrack.transport import DeliveryResult, ReconciliationResult


def revision(number, parents=(), author="alice", message=None):
    return Revision(
        f"{number:064x}",
        tuple(f"{p:064x}" for p in parents),
        author,
        message or f"GAME-{number} change",
        1730130046519 + number,
    )


class MemorySource:
    unseen_revisions = LoreSource.unseen_revisions

    def __init__(self, revisions, branches=None, repository_id="repository-a"):
        self.items = {r.id: r for r in revisions}
        last = revisions[-1].id
        self.branch_items = branches or [Branch("branch-a", "main", last, (last,))]
        self.repository_id = repository_id

    def repository(self):
        return Repository(self.repository_id, "lores://lore.invalid/repository-a")

    def branches(self):
        return self.branch_items

    def revision(self, signature):
        return self.items[signature]


class Receiver:
    def __init__(self, result=None):
        self.posts = []
        self.lookups = []
        self.result = result or DeliveryResult("accepted", 204, "HTTP accepted")
        self.reconciliation = ReconciliationResult("absent", 200, "Not found")

    def deliver(self, payload):
        self.posts.append(payload)
        return self.result

    def reconcile(self, issues, **options):
        self.lookups.append((issues, options))
        return self.reconciliation


class BridgeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.database = self.path / "outbox.sqlite3"
        self.config = Config(
            self.path / "cache",
            self.database,
            "https://yt.invalid/api/vcsHooksReceiver/generic/1-1",
            Secret(env="TEST_SIGNATURE"),
            {"alice": {"userName": "alice", "email": "alice@example.invalid"}},
            poll_seconds=1,
        )
        self.lock = process_lock(self.database)
        self.lock.__enter__()
        self.addCleanup(self.lock.__exit__, None, None, None)
        self.store = Store(self.database, self.config.receiver_url)
        self.addCleanup(lambda: self.store.close())

    def restart(self):
        self.store.close()
        self.store = Store(self.database, self.config.receiver_url)

    def test_multi_revision_push_and_merge_keep_original_data_and_dag_order(self):
        source = MemorySource(
            [revision(1), revision(2, (1,)), revision(3, (1,)), revision(4, (2, 3))]
        )
        self.assertEqual(scan(self.config, source, self.store), 4)
        receiver = Receiver()
        self.assertEqual(deliver(self.config, self.store, receiver), 4)
        versions = [p["commits"][0]["version"] for p in receiver.posts]
        self.assertLess(versions.index(revision(2).id), versions.index(revision(4).id))
        self.assertLess(versions.index(revision(3).id), versions.index(revision(4).id))
        self.assertEqual(
            receiver.posts[0],
            {
                "branch": "refs/heads/main",
                "commits": [
                    {
                        "version": revision(1).id,
                        "date": revision(1).timestamp_ms,
                        "user": {"userName": "alice", "email": "alice@example.invalid"},
                        "comment": "GAME-1 change",
                    }
                ],
            },
        )
        self.restart()
        self.assertEqual(scan(self.config, source, self.store), 0)
        self.assertEqual(deliver(self.config, self.store, receiver), 0)
        self.assertEqual(len(receiver.posts), 4)

    def test_shared_ancestors_and_new_branch_do_not_reexecute_commands(self):
        one, two = revision(1), revision(2, (1,))
        branches = [
            Branch("z", "main", two.id, (two.id,)),
            Branch("a", "feature", one.id, (one.id,)),
        ]
        source = MemorySource([one, two], branches)
        config = replace(self.config, branches=("*",))
        self.assertEqual(scan(config, source, self.store), 2)
        receiver = Receiver()
        self.assertEqual(deliver(config, self.store, receiver), 2)
        self.assertEqual(receiver.posts[0]["branch"], "refs/heads/feature")
        self.assertEqual(receiver.posts[1]["branch"], "refs/heads/main")
        source.branch_items.append(Branch("another", "release", two.id, (two.id,)))
        self.assertEqual(scan(config, source, self.store), 0)

    def test_crash_after_claim_is_unknown_and_holds_children_across_restarts(self):
        scan(self.config, MemorySource([revision(1), revision(2, (1,))]), self.store)
        item = self.store.claim()
        self.assertEqual(item["revision"], revision(1).id)
        self.restart()
        self.assertEqual(self.store.status()["counts"], {"unknown": 1, "pending": 1})
        receiver = Receiver()
        self.assertEqual(deliver(self.config, self.store, receiver), 0)
        self.restart()
        self.assertEqual(deliver(self.config, self.store, receiver), 0)
        self.assertEqual(receiver.posts, [])
        with self.assertRaises(StoreError):
            self.store.retry_blocked(revision(1).id)

    def test_unknown_response_is_never_retried(self):
        scan(self.config, MemorySource([revision(1)]), self.store)
        receiver = Receiver(DeliveryResult("unknown", 503, "HTTP outcome uncertain"))
        deliver(self.config, self.store, receiver)
        self.restart()
        deliver(self.config, self.store, receiver)
        self.assertEqual(len(receiver.posts), 1)
        self.assertEqual(self.store.status()["counts"], {"unknown": 1})

    def test_connect_failure_remains_pending_with_backoff(self):
        scan(self.config, MemorySource([revision(1)]), self.store)
        receiver = Receiver(DeliveryResult("retry", None, "No request sent"))
        deliver(self.config, self.store, receiver)
        self.assertEqual(self.store.status()["counts"], {"pending": 1})
        self.assertIsNone(self.store.claim(now=0))
        self.assertIsNotNone(self.store.claim(now=10**15))

    def test_rejected_request_requires_explicit_retry(self):
        scan(self.config, MemorySource([revision(1)]), self.store)
        receiver = Receiver(DeliveryResult("blocked", 401, "Authorization rejected"))
        deliver(self.config, self.store, receiver)
        self.restart()
        deliver(self.config, self.store, receiver)
        self.assertEqual(len(receiver.posts), 1)
        self.store.retry_blocked(revision(1).id)
        receiver.result = DeliveryResult("accepted", 204, "Accepted")
        deliver(self.config, self.store, receiver)
        self.assertEqual(self.store.status()["counts"], {"accepted": 1})

    def test_positive_reconciliation_uses_all_issue_ids_and_never_posts(self):
        secret_file = self.path / "token"
        secret_file.write_text("read-only-token", encoding="utf-8")
        config = replace(
            self.config,
            read_token=Secret(file=secret_file),
            commit_url_template="https://viewer.invalid/repository-a/$hash$",
            project_keys=("GAME",),
        )
        scan(
            config,
            MemorySource(
                [revision(1, message="GAME-1 GAME-2 unrelated-3 #GAME-1 Fixed")]
            ),
            self.store,
        )
        self.store.claim()
        self.restart()
        receiver = Receiver()
        self.assertEqual(reconcile(config, self.store, receiver), 0)
        self.assertEqual(self.store.status()["counts"], {"unknown": 1})
        self.assertEqual(receiver.lookups[0][0], ["GAME-1", "GAME-2"])
        self.assertEqual(
            receiver.lookups[0][1]["expected_url"],
            "https://viewer.invalid/repository-a/" + revision(1).id,
        )
        receiver.reconciliation = ReconciliationResult(
            "found", 200, "Both associations found"
        )
        self.assertEqual(reconcile(config, self.store, receiver), 1)
        self.assertEqual(deliver(config, self.store, receiver), 0)
        self.assertEqual(receiver.posts, [])
        self.assertEqual(self.store.status()["counts"], {"reconciled": 1})

    def test_unmapped_author_or_incomplete_discovery_leaves_no_checkpoint(self):
        source = MemorySource([revision(1), revision(2, (1,), author="unknown-user")])
        with self.assertRaises(ConfigError):
            scan(self.config, source, self.store)
        self.assertFalse(self.store.knows(revision(1).id))
        self.assertEqual(self.store.status()["counts"], {})
        with self.assertRaises(SourceError):
            scan(
                replace(self.config, scan_limit=1),
                MemorySource([revision(1), revision(2, (1,))]),
                self.store,
            )
        self.assertEqual(self.store.status()["counts"], {})

    def test_missing_parent_rolls_back_the_whole_enqueue_transaction(self):
        records = [
            (revision(1), "refs/heads/main", {}),
            (revision(3, (2,)), "refs/heads/main", {}),
        ]
        with self.assertRaises(StoreError):
            self.store.enqueue(records)
        self.assertFalse(self.store.knows(revision(1).id))
        self.assertEqual(self.store.status()["counts"], {})

    def test_baseline_skips_existing_history_and_new_commits_are_delivered(self):
        config = replace(self.config, authors={})
        source = MemorySource([revision(1)])
        self.assertEqual(scan(config, source, self.store, baseline=True), 1)
        self.assertEqual(self.store.status()["counts"], {"skipped": 1})
        with self.assertRaises(StoreError):
            scan(config, source, self.store, baseline=True)
        self.assertEqual(
            scan(
                self.config, MemorySource([revision(1), revision(2, (1,))]), self.store
            ),
            1,
        )
        receiver = Receiver()
        self.assertEqual(deliver(self.config, self.store, receiver), 1)
        self.assertEqual(receiver.posts[0]["commits"][0]["version"], revision(2).id)

    def test_binding_prevents_reusing_ledger_for_different_source_or_receiver(self):
        scan(self.config, MemorySource([revision(1)]), self.store)
        with self.assertRaises(StoreError):
            scan(
                self.config,
                MemorySource([revision(1)], repository_id="other"),
                self.store,
            )
        with self.assertRaises(StoreError):
            Store(
                self.database, "https://other.invalid/api/vcsHooksReceiver/generic/1-1"
            )
        self.assertEqual(self.store.status()["counts"], {"pending": 1})

    def test_a_second_process_cannot_recover_an_active_attempt(self):
        scan(self.config, MemorySource([revision(1)]), self.store)
        self.store.claim()
        code = "from pathlib import Path; from lore_youtrack.store import process_lock; import sys\nwith process_lock(Path(sys.argv[1])): print('incorrectly acquired')"
        child = subprocess.run(
            [sys.executable, "-c", code, str(self.database)],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        self.assertNotEqual(child.returncode, 0)
        self.assertNotIn("incorrectly acquired", child.stdout)
        self.assertEqual(self.store.status()["counts"], {"in_flight": 1})

    def test_read_only_status_during_a_live_attempt_never_recovers_it(self):
        scan(self.config, MemorySource([revision(1)]), self.store)
        self.store.claim()
        snapshot = read_status(self.database, self.config.receiver_url)
        self.assertEqual(snapshot["counts"], {"in_flight": 1})
        self.assertEqual(self.store.status()["counts"], {"in_flight": 1})
        with self.assertRaises(StoreError):
            read_status(
                self.database, "https://other.invalid/api/vcsHooksReceiver/generic/1-1"
            )

    def test_credential_is_not_persisted_in_outbox(self):
        scan(self.config, MemorySource([revision(1)]), self.store)
        receiver = Receiver()
        deliver(self.config, self.store, receiver)
        rows = list(self.store.connection.iterdump())
        self.assertNotIn("TEST_SIGNATURE", "\n".join(rows))
        self.assertNotIn("https://yt.invalid", "\n".join(rows))


if __name__ == "__main__":
    unittest.main()
