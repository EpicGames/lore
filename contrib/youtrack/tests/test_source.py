# SPDX-FileCopyrightText: 2026 RedRatInHat
# SPDX-License-Identifier: MIT
"""Contract and failure tests for the read-only Lore JSONL adapter."""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from dataclasses import replace
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

from lore_youtrack.source import (
    ZERO_HASH,
    LoreSource,
    Revision,
    SourceError,
    _bounded_run,
    _parse_events,
)

REPOSITORY = "12" * 16
BRANCH = "34" * 16
AUTHOR = "original-author"


def h(number: int) -> str:
    return f"{number:064x}"


def event(tag: str, **data: object) -> dict[str, object]:
    return {"tagName": tag, "data": data}


def output(*events: dict[str, object], complete: bool = True) -> bytes:
    entries = list(events)
    if complete:
        entries.append(event("complete", status=0))
    return ("\n".join(json.dumps(item) for item in entries) + "\n").encode()


def repository_output() -> bytes:
    return output(
        event("repositoryData", id=REPOSITORY, remoteUrl="lore://127.0.0.1:41337")
    )


def revision_output(
    revision: str = h(2),
    parents: tuple[str, str] = (h(1), ZERO_HASH),
    *,
    missing: str = "",
    created_by: object = AUTHOR,
    repository: str = REPOSITORY,
) -> bytes:
    # Shapes are from Lore 0.12's public event/metadata serializers, not the
    # human-readable CLI output. A native smoke verifies the same contract.
    metadata = {
        "created-by": ("string", created_by),
        "committed-by": ("string", "committer"),
        "message": ("string", "GAME-123 Исправить диалог\nSecond line"),
        "timestamp": ("numeric", 1793476800123),
    }
    return output(
        event(
            "revisionInfo",
            repository=repository,
            revision=revision,
            revisionNumber=2,
            parent=list(parents),
        ),
        *[
            event("metadata", key=key, value={"tagName": tag, "data": value})
            for key, (tag, value) in metadata.items()
            if key != missing
        ],
    )


def branches_output(*entries: dict[str, object], count: int | None = None) -> bytes:
    return output(
        event("branchListBegin", location="remote"),
        *entries,
        event(
            "branchListEnd",
            location="remote",
            count=len(entries) if count is None else count,
        ),
    )


def branch_entry(
    *, name: str = "main", branch_id: str = BRANCH, head: str = h(2)
) -> dict[str, object]:
    return event(
        "branchListEntry",
        location="remote",
        id=branch_id,
        name=name,
        latest=head,
        stack=[{"branch": "56" * 16, "revision": h(1)}],
    )


class SourceContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.source = LoreSource("lore", "/dedicated-bare-cache")

    def test_remote_arguments_are_passed_without_a_shell(self) -> None:
        with patch(
            "lore_youtrack.source._bounded_run", return_value=repository_output()
        ) as run:
            repository = self.source.repository()
        args = run.call_args.args[0]
        self.assertEqual(repository.id, REPOSITORY)
        self.assertEqual(repository.remote_url, "lore://127.0.0.1:41337")
        self.assertIn("--remote", args)
        self.assertIn("--json", args)
        self.assertIn("--non-interactive", args)
        self.assertEqual(args[-2:], ["repository", "info"])

    def test_complete_original_author_metadata_and_both_parents(self) -> None:
        with patch(
            "lore_youtrack.source._bounded_run",
            side_effect=[repository_output(), revision_output(parents=(h(1), h(3)))],
        ):
            revision = self.source.revision(h(2))
        self.assertEqual(revision.author, AUTHOR)
        self.assertEqual(revision.parents, (h(1), h(3)))
        self.assertEqual(revision.timestamp_ms, 1793476800123)
        self.assertEqual(revision.message, "GAME-123 Исправить диалог\nSecond line")

    def test_committer_fallback_does_not_use_push_identity(self) -> None:
        with patch(
            "lore_youtrack.source._bounded_run",
            side_effect=[repository_output(), revision_output(missing="created-by")],
        ):
            self.assertEqual(self.source.revision(h(2)).author, "committer")

    def test_invalid_original_author_cannot_silently_become_the_committer(self) -> None:
        with (
            patch(
                "lore_youtrack.source._bounded_run",
                side_effect=[repository_output(), revision_output(created_by=False)],
            ),
            self.assertRaises(SourceError),
        ):
            self.source.revision(h(2))

    def test_missing_metadata_is_failure_even_when_cli_completed(self) -> None:
        for missing in ("message", "timestamp"):
            with (
                self.subTest(missing=missing),
                patch(
                    "lore_youtrack.source._bounded_run",
                    side_effect=[repository_output(), revision_output(missing=missing)],
                ),
                self.assertRaises(SourceError),
            ):
                LoreSource("lore", "/bare").revision(h(2))

    def test_revision_and_repository_binding(self) -> None:
        for payload in (
            revision_output(revision=h(9)),
            revision_output(repository="ff" * 16),
        ):
            with (
                self.subTest(payload=payload),
                patch(
                    "lore_youtrack.source._bounded_run",
                    side_effect=[repository_output(), payload],
                ),
                self.assertRaises(SourceError),
            ):
                LoreSource("lore", "/bare").revision(h(2))

    def test_branch_heads_include_stack_and_remote_order_is_deterministic(self) -> None:
        payload = branches_output(
            branch_entry(name="z-feature", branch_id="78" * 16),
            branch_entry(),
        )
        with patch("lore_youtrack.source._bounded_run", return_value=payload):
            branches = self.source.branches()
        self.assertEqual([item.name for item in branches], ["main", "z-feature"])
        self.assertEqual(branches[0].heads, (h(2), h(1)))

    def test_remote_branch_list_must_be_complete_and_exclusively_remote(self) -> None:
        local = branch_entry()
        local["data"]["location"] = "local"
        for payload in (
            branches_output(branch_entry(), count=2),
            branches_output(local),
            branches_output(branch_entry(), branch_entry()),
            output(event("branchListBegin", location="remote"), branch_entry()),
            output(
                branch_entry(),
                event("branchListBegin", location="remote"),
                event("branchListEnd", location="remote", count=1),
            ),
        ):
            with (
                self.subTest(payload=payload),
                patch("lore_youtrack.source._bounded_run", return_value=payload),
                self.assertRaises(SourceError),
            ):
                self.source.branches()

    def test_empty_remote_repository_is_distinct_from_failed_listing(self) -> None:
        with patch("lore_youtrack.source._bounded_run", return_value=branches_output()):
            self.assertEqual(self.source.branches(), [])

    def test_output_requires_successful_terminal_complete(self) -> None:
        invalid_outputs = [
            b"",
            output(event("revisionInfo", revision=h(1)), complete=False),
            output(event("complete", status=1), complete=False),
            output(event("complete", status=True), complete=False),
            output(event("complete", status=0)),
            output(
                event("complete", status=0), event("metadata", key="x"), complete=False
            ),
            b'{"tagName":"complete","data":{"status":0,"status":1}}\n',
            b'{"tagName":"complete","data":{"status":NaN}}\n',
            b"not JSON and perhaps a token\n",
            b"\xff\n",
        ]
        for payload in invalid_outputs:
            with self.subTest(payload=payload), self.assertRaises(SourceError):
                _parse_events(payload)
        self.assertEqual(
            len(_parse_events(output(event("log", message="ignored log")))), 2
        )

    def test_native_revision_display_lookup_cannot_override_source_result(self) -> None:
        suffix = output(
            event("log", level="error", message="display lookup unavailable"),
            event("complete", status=9),
            complete=False,
        )
        with patch(
            "lore_youtrack.source._bounded_run",
            side_effect=[repository_output(), revision_output() + suffix],
        ):
            self.assertEqual(self.source.revision(h(2)).author, AUTHOR)
        for unexpected in (
            event(
                "metadata",
                key="created-by",
                value={"tagName": "string", "data": "other"},
            ),
            event("revisionInfo", revision=h(9)),
        ):
            with self.subTest(unexpected=unexpected), self.assertRaises(SourceError):
                _parse_events(
                    revision_output() + output(unexpected, complete=False),
                    allow_display_lookup=True,
                )
        with self.assertRaises(SourceError):
            _parse_events(
                output(event("complete", status=9), complete=False)
                + output(event("complete", status=0), complete=False),
                allow_display_lookup=True,
            )


class GraphTests(unittest.TestCase):
    def setUp(self) -> None:
        self.source = LoreSource("lore", "/bare")
        self.graph = {
            h(1): Revision(h(1), (), "author", "root", 100),
            h(2): Revision(h(2), (h(1),), "author", "left", 5),
            h(3): Revision(h(3), (h(1),), "author", "right", 300),
            h(4): Revision(h(4), (h(2), h(3)), "author", "merge", 1),
        }

    def test_merge_graph_includes_second_parent_once_before_child(self) -> None:
        with patch.object(
            self.source, "revision", side_effect=self.graph.__getitem__
        ) as read:
            revisions = self.source.unseen_revisions(h(4), set())
        self.assertEqual([item.id for item in revisions], [h(1), h(2), h(3), h(4)])
        self.assertEqual(read.call_count, 4)

    def test_known_closed_subgraph_is_not_read_again(self) -> None:
        known = {h(1), h(2)}
        with patch.object(
            self.source, "revision", side_effect=self.graph.__getitem__
        ) as read:
            revisions = self.source.unseen_revisions([h(4), h(2)], known.__contains__)
        self.assertEqual([item.id for item in revisions], [h(3), h(4)])
        self.assertEqual(read.call_count, 2)
        self.assertEqual(known, {h(1), h(2)})

    def test_diamond_does_not_become_false_cycle(self) -> None:
        with patch.object(self.source, "revision", side_effect=self.graph.__getitem__):
            self.assertEqual(len(self.source.unseen_revisions([h(3), h(4)], set())), 4)

    def test_known_stack_roots_do_not_consume_the_new_revision_limit(self) -> None:
        with patch.object(
            self.source, "revision", side_effect=self.graph.__getitem__
        ) as read:
            revisions = self.source.unseen_revisions(
                [h(4), h(3), h(2)], {h(1), h(2), h(3)}, max_revisions=1
            )
        self.assertEqual([item.id for item in revisions], [h(4)])
        self.assertEqual(read.call_count, 1)

    def test_parent_cycle_fails_before_returning_partial_results(self) -> None:
        self.graph[h(1)] = replace(self.graph[h(1)], parents=(h(4),))
        with (
            patch.object(self.source, "revision", side_effect=self.graph.__getitem__),
            self.assertRaises(SourceError),
        ):
            self.source.unseen_revisions(h(4), set())

    def test_missing_parent_propagates_without_mutating_known(self) -> None:
        known: set[str] = set()
        with (
            patch.object(self.source, "revision", side_effect=SourceError("missing")),
            self.assertRaises(SourceError),
        ):
            self.source.unseen_revisions(h(4), known)
        self.assertEqual(known, set())

    def test_node_and_aggregate_metadata_limits(self) -> None:
        with patch.object(self.source, "revision", side_effect=self.graph.__getitem__):
            with self.assertRaises(SourceError):
                self.source.unseen_revisions(h(4), set(), max_revisions=3)
            with self.assertRaises(SourceError):
                self.source.unseen_revisions(h(4), set(), max_total_bytes=1)

    def test_deep_history_does_not_use_python_recursion(self) -> None:
        graph = {
            h(index): Revision(
                h(index), (h(index - 1),) if index > 1 else (), "a", "", 1
            )
            for index in range(1, 1501)
        }
        with patch.object(self.source, "revision", side_effect=graph.__getitem__):
            revisions = self.source.unseen_revisions(h(1500), set(), max_revisions=2000)
        self.assertEqual(len(revisions), 1500)
        self.assertEqual(revisions[0].id, h(1))
        self.assertEqual(revisions[-1].id, h(1500))

    def test_zero_head_never_calls_cli(self) -> None:
        with patch.object(self.source, "revision") as read:
            self.assertEqual(self.source.unseen_revisions(ZERO_HASH, set()), [])
        read.assert_not_called()


class ProcessBoundaryTests(unittest.TestCase):
    def test_nonzero_exit_does_not_expose_stdout_or_stderr(self) -> None:
        script = (
            "import sys;print('sensitive-token');sys.stderr.write('secret');sys.exit(7)"
        )
        with self.assertRaises(SourceError) as failure:
            _bounded_run([sys.executable, "-c", script], 2, 4096)
        self.assertNotIn("sensitive-token", str(failure.exception))
        self.assertNotIn("secret", str(failure.exception))

    def test_both_output_streams_are_bounded(self) -> None:
        for stream in ("stdout", "stderr"):
            with self.subTest(stream=stream), self.assertRaises(SourceError):
                _bounded_run(
                    [
                        sys.executable,
                        "-c",
                        f"import sys;sys.{stream}.write('x'*1000000)",
                    ],
                    2,
                    4096,
                )

    def test_stuck_cli_is_terminated(self) -> None:
        with self.assertRaises(SourceError):
            _bounded_run(
                [sys.executable, "-c", "import time;time.sleep(10)"], 0.05, 4096
            )

    def test_argument_metacharacters_are_not_executed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory, "must-not-exist")
            value = f"$(touch {target})"
            result = _bounded_run(
                [sys.executable, "-c", "import sys;print(sys.argv[1])", value], 2, 4096
            )
            self.assertEqual(result.decode().strip(), value)
            self.assertFalse(target.exists())


@unittest.skipUnless(
    os.environ.get("LORE_TEST_BINARY") and os.environ.get("LORE_TEST_SERVER_BINARY"),
    "set LORE_TEST_BINARY and LORE_TEST_SERVER_BINARY to run the native smoke",
)
class NativeSourceTests(unittest.TestCase):
    """Exercise the parser against a real server, pushes and a merge graph."""

    def test_remote_merge_history_and_durable_delivery_from_a_bare_clone(self) -> None:
        binary = str(Path(os.environ["LORE_TEST_BINARY"]).resolve())
        server_binary = str(Path(os.environ["LORE_TEST_SERVER_BINARY"]).resolve())
        with tempfile.TemporaryDirectory(prefix="lore-youtrack-source-") as directory:
            root = Path(directory)
            config = root / "config"
            work = root / "work"
            config.mkdir()
            work.mkdir()
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            (config / "local.toml").write_text(
                "[server]\nconnection_close_timeout_seconds = 1\n"
                "runtime_shutdown_timeout_seconds = 1\n"
                f'[server.quic]\nhost = "127.0.0.1"\nport = {port}\n'
                f'[server.grpc]\nhost = "127.0.0.1"\nport = {port}\n'
                "[server.http]\nenabled = false\n"
                "[immutable_store.local]\n"
                f"path = {json.dumps(str(root / 'immutable'))}\n"
                "[mutable_store.local]\n"
                f"path = {json.dumps(str(root / 'mutable'))}\n",
                encoding="utf-8",
            )
            isolated = dict(os.environ)
            for key in (
                "LORE_TOKEN",
                "LORE_IDENTITY_TOKEN",
                "LORE_ACCESS_TOKEN",
                "LORE_USE_SERVICE",
                "LORE_SERVICE_EXECUTABLE",
            ):
                isolated.pop(key, None)
            isolated.update(
                LORE_GLOBAL_PATH=str(root / "global"),
                LORE_AUTH_PATH=str(root / "auth"),
                RUST_LOG="error",
            )
            with (
                patch.dict(os.environ, isolated, clear=True),
                (root / "server.log").open("wb") as log,
            ):
                server = subprocess.Popen(
                    [server_binary, "--config", str(config)],
                    stdout=log,
                    stderr=subprocess.STDOUT,
                )
                try:
                    deadline = time.monotonic() + 15
                    while True:
                        self.assertIsNone(server.poll(), "native Lore server exited")
                        if time.monotonic() >= deadline:
                            self.fail("native Lore server did not become ready")
                        try:
                            with socket.create_connection(
                                ("127.0.0.1", port), timeout=0.1
                            ):
                                break
                        except OSError:
                            time.sleep(0.01)

                    def run(*command: str) -> list[dict[str, object]]:
                        result = subprocess.run(
                            [
                                binary,
                                "--json",
                                "--non-interactive",
                                "--no-pager",
                                "--repository",
                                str(work),
                                *command,
                            ],
                            cwd=work,
                            capture_output=True,
                            timeout=30,
                            check=False,
                        )
                        self.assertEqual(
                            result.returncode, 0, "native fixture command failed"
                        )
                        events = [
                            json.loads(line)
                            for line in result.stdout.splitlines()
                            if line
                        ]
                        completions = [
                            e["data"]["status"]
                            for e in events
                            if e["tagName"] == "complete"
                        ]
                        self.assertTrue(completions)
                        self.assertEqual(completions[0], 0)
                        return events

                    def commit(path: str, message: str, author: str) -> str:
                        (work / path).write_text(message + "\n", encoding="utf-8")
                        run("file", "stage", path)
                        events = run(
                            "--identity", author, "revision", "commit", message
                        )
                        return next(
                            e["data"]["revision"]
                            for e in events
                            if e["tagName"] == "revisionCommitRevision"
                        )

                    url = f"lore://127.0.0.1:{port}/source-smoke"
                    run("repository", "create", url)
                    base = commit("story.txt", "GAME-101 base", "base-author")
                    run("branch", "push")
                    run("--identity", "feature-author", "branch", "create", "feature")
                    feature = commit(
                        "feature.txt", "GAME-102 feature", "feature-author"
                    )
                    run("branch", "push")
                    run("branch", "switch", "main")
                    main = commit("main.txt", "GAME-103 main", "main-author")
                    run("branch", "push")
                    run(
                        "--identity",
                        "integrator",
                        "branch",
                        "merge",
                        "feature",
                        "--message",
                        "GAME-104 merge",
                    )
                    run("branch", "push")
                    bare = root / "reader"
                    run("repository", "clone", url, str(bare), "--bare")

                    source = LoreSource(binary, bare)
                    repository = source.repository()
                    self.assertTrue(repository.id)
                    self.assertEqual(repository.remote_url, f"lore://127.0.0.1:{port}")
                    branches = source.branches()
                    main_branch = next(
                        branch for branch in branches if branch.name == "main"
                    )
                    revisions = source.unseen_revisions(main_branch.heads, set())
                    by_id = {revision.id: revision for revision in revisions}
                    self.assertTrue({base, feature, main}.issubset(by_id))
                    self.assertEqual(by_id[feature].author, "feature-author")
                    self.assertEqual(by_id[main].author, "main-author")
                    self.assertEqual(by_id[main_branch.head].message, "GAME-104 merge")
                    self.assertEqual(
                        set(by_id[main_branch.head].parents), {feature, main}
                    )
                    positions = {revision.id: i for i, revision in enumerate(revisions)}
                    for revision in revisions:
                        self.assertGreater(revision.timestamp_ms, 1_000_000_000_000)
                        for parent in revision.parents:
                            self.assertLess(positions[parent], positions[revision.id])
                    self.assertFalse((bare / "story.txt").exists())
                    self.assertFalse((bare / "feature.txt").exists())
                    self.assertFalse((bare / "main.txt").exists())

                    unpushed = commit(
                        "private.txt", "GAME-105 unpushed", "local-author"
                    )
                    work_source = LoreSource(binary, work)
                    work_source.repository()
                    remote_main = next(
                        b for b in work_source.branches() if b.name == "main"
                    )
                    self.assertEqual(remote_main.head, main_branch.head)
                    self.assertNotIn(unpushed, by_id)
                    self.assertEqual(
                        source.unseen_revisions(main_branch.heads, set(by_id)), []
                    )
                    self._assert_bridge_delivery(
                        root, binary, bare, revisions, run, unpushed
                    )
                finally:
                    server.terminate()
                    try:
                        server.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()

    def _assert_bridge_delivery(
        self, root, binary, bare, revisions, run, unpushed
    ) -> None:
        received: list[tuple[str, str | None, dict]] = []
        lose_response = False

        class Receiver(BaseHTTPRequestHandler):
            def do_POST(self):
                payload = json.loads(
                    self.rfile.read(int(self.headers["Content-Length"]))
                )
                received.append(
                    (self.path, self.headers.get("X-YT-Simple-Signature"), payload)
                )
                if lose_response:
                    self.close_connection = True
                    return
                self.send_response(204)
                self.end_headers()

            def log_message(self, *_args):
                pass

        receiver = ThreadingHTTPServer(("127.0.0.1", 0), Receiver)
        worker = threading.Thread(target=receiver.serve_forever, daemon=True)
        worker.start()
        try:
            config = root / "bridge.toml"
            config.write_text(
                "[bridge]\n"
                f"repository = {json.dumps(str(bare))}\n"
                f"database = {json.dumps(str(root / 'outbox.sqlite3'))}\n"
                f"lore_binary = {json.dumps(binary)}\n"
                'branches = ["main"]\n'
                "[youtrack]\n"
                f'receiver_url = "http://127.0.0.1:{receiver.server_port}/api/vcsHooksReceiver/generic/1-1"\n'
                'signature_env = "LORE_YOUTRACK_NATIVE_SIGNATURE"\n'
                "allow_http_loopback = true\n"
                + "\n".join(
                    f'[authors."{author}"]\nuser_name = "yt-{author}"\n'
                    f'email = "{author}@example.invalid"\n'
                    for author in (
                        "base-author",
                        "feature-author",
                        "main-author",
                        "integrator",
                        "local-author",
                    )
                ),
                encoding="utf-8",
            )
            environment = dict(os.environ)
            environment["LORE_YOUTRACK_NATIVE_SIGNATURE"] = "native-test-signature"
            environment["PYTHONPATH"] = str(Path(__file__).resolve().parents[1])

            def sync(expected_exit=0):
                result = subprocess.run(
                    [
                        sys.executable,
                        "-m",
                        "lore_youtrack",
                        "--config",
                        str(config),
                        "sync",
                    ],
                    cwd=root,
                    env=environment,
                    capture_output=True,
                    timeout=30,
                    check=False,
                )
                self.assertEqual(
                    result.returncode, expected_exit, "native bridge command failed"
                )
                return json.loads(result.stdout)

            first = sync()
            self.assertEqual(first["accepted"], len(revisions))
            self.assertEqual(len(received), len(revisions))
            for (path, signature, payload), revision in zip(received, revisions):
                self.assertEqual(path, "/api/vcsHooksReceiver/generic/1-1")
                self.assertEqual(signature, "native-test-signature")
                self.assertEqual(
                    payload,
                    {
                        "branch": "refs/heads/main",
                        "commits": [
                            {
                                "version": revision.id,
                                "date": revision.timestamp_ms,
                                "user": {
                                    "userName": f"yt-{revision.author}",
                                    "email": f"{revision.author}@example.invalid",
                                },
                                "comment": revision.message,
                            }
                        ],
                    },
                )
            second = sync()
            self.assertEqual(second["discovered"], 0)
            self.assertEqual(second["accepted"], 0)
            self.assertEqual(len(received), len(revisions))

            # The receiver consumes a real newly pushed revision, then closes
            # without an ACK. A fresh bridge process must not replay its POST.
            lose_response = True
            run("branch", "push")
            uncertain = sync(expected_exit=2)
            self.assertEqual(uncertain["counts"].get("unknown"), 1)
            self.assertEqual(len(received), len(revisions) + 1)
            self.assertEqual(received[-1][2]["commits"][0]["version"], unpushed)
            reopened = sync(expected_exit=2)
            self.assertEqual(reopened["counts"].get("unknown"), 1)
            self.assertEqual(reopened["accepted"], 0)
            self.assertEqual(len(received), len(revisions) + 1)
        finally:
            receiver.shutdown()
            receiver.server_close()
            worker.join(timeout=5)


if __name__ == "__main__":
    unittest.main()
