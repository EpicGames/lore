# Copyright 2026 RedRatInHat
# SPDX-License-Identifier: MIT

"""Exercise transport behavior against isolated real HTTP peers."""

from __future__ import annotations

import json
import socket
import ssl
import threading
import unittest
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs, urlsplit

from lore_youtrack.transport import YouTrackTransport

Request = tuple[str, str, dict[str, str], bytes]
Reply = tuple[int, dict[str, str], bytes] | None


@contextmanager
def http_peer(
    reply: Callable[[Request], Reply],
) -> Iterator[tuple[str, list[Request]]]:
    requests: list[Request] = []

    class Handler(BaseHTTPRequestHandler):
        def _respond(self) -> None:
            length = int(self.headers.get("Content-Length", "0"))
            request = (
                self.command,
                self.path,
                dict(self.headers),
                self.rfile.read(length),
            )
            requests.append(request)
            response = reply(request)
            if response is None:
                self.connection.shutdown(socket.SHUT_RDWR)
                self.close_connection = True
                return
            status, headers, body = response
            self.send_response(status)
            if "Content-Length" not in headers:
                self.send_header("Content-Length", str(len(body)))
            for key, value in headers.items():
                self.send_header(key, value)
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

        do_POST = _respond
        do_GET = _respond

        def log_message(self, format: str, *args: Any) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(
        target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True
    )
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


class TransportTests(unittest.TestCase):
    def client(self, origin: str, **kwargs: Any) -> YouTrackTransport:
        return YouTrackTransport(
            origin + "/youtrack/api/vcsHooksReceiver/generic/519-6",
            "test-secret",
            allow_http_loopback=True,
            **kwargs,
        )

    def test_actual_post_uses_generic_contract_and_one_request(self) -> None:
        payload = {
            "branch": "main",
            "commits": [
                {
                    "version": "a" * 64,
                    "date": 1730130046519,
                    "user": {"userName": "developer", "email": "dev@example.test"},
                    "comment": "GAME-12 исправление",
                }
            ],
        }
        with http_peer(lambda _: (202, {}, b"{}")) as (origin, requests):
            result = YouTrackTransport(
                origin + "/youtrack/api/vcsHooksReceiver/generic/519-6",
                "test secret with spaces",
                allow_http_loopback=True,
            ).deliver(payload)
        self.assertEqual(result.state, "accepted")
        self.assertEqual(result.http_status, 202)
        self.assertEqual(len(requests), 1)
        method, path, headers, body = requests[0]
        self.assertEqual(method, "POST")
        self.assertEqual(path, "/youtrack/api/vcsHooksReceiver/generic/519-6")
        self.assertEqual(headers["X-YT-Simple-Signature"], "test secret with spaces")
        self.assertEqual(headers["Content-Type"], "application/json")
        self.assertNotIn("Authorization", headers)
        self.assertNotIn("Idempotency-Key", headers)
        self.assertEqual(json.loads(body), payload)

    def test_connection_refusal_is_retry_before_any_request(self) -> None:
        # A bound socket without listen cannot accept a request or be reused by a peer.
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            origin = f"http://127.0.0.1:{reserved.getsockname()[1]}"
            result = self.client(origin, timeout=0.2).deliver({"commits": []})
        self.assertEqual(result.state, "retry")
        self.assertIsNone(result.http_status)
        self.assertNotIn(origin, result.detail)

    def test_lost_reply_preserves_effect_and_never_replays(self) -> None:
        effects: list[dict[str, Any]] = []

        def commit_then_disconnect(request: Request) -> None:
            effects.append(json.loads(request[3]))

        with http_peer(commit_then_disconnect) as (origin, requests):
            result = self.client(origin).deliver({"commits": [{"version": "abc"}]})
        self.assertEqual(result.state, "unknown")
        self.assertEqual(len(requests), 1)
        self.assertEqual(effects, [{"commits": [{"version": "abc"}]}])

    def test_server_errors_and_rate_limits_are_unknown_without_retry(self) -> None:
        for status in (408, 425, 429, 500, 502, 503):
            with self.subTest(status=status):
                with http_peer(
                    lambda _, status=status: (status, {"Retry-After": "0"}, b"private")
                ) as (origin, requests):
                    result = self.client(origin).deliver({})
                self.assertEqual(result.state, "unknown")
                self.assertEqual(result.http_status, status)
                self.assertEqual(result.retry_after, 1)
                self.assertEqual(len(requests), 1)
                self.assertNotIn("private", result.detail)

    def test_rejections_are_blocked_without_retry(self) -> None:
        for status in (400, 401, 403, 404, 409, 422):
            with self.subTest(status=status):
                with http_peer(
                    lambda _, status=status: (status, {}, b"test-secret")
                ) as (origin, requests):
                    result = self.client(origin).deliver({})
                self.assertEqual(result.state, "blocked")
                self.assertEqual(result.http_status, status)
                self.assertEqual(len(requests), 1)
                self.assertNotIn("test-secret", result.detail)

    def test_redirect_never_calls_another_host(self) -> None:
        with (
            http_peer(lambda _: (200, {}, b"")) as (other, leaked),
            http_peer(lambda _: (307, {"Location": other + "/capture"}, b"")) as (
                origin,
                requests,
            ),
        ):
            result = self.client(origin).deliver({})
        self.assertEqual(result.state, "blocked")
        self.assertEqual(result.http_status, 307)
        self.assertEqual(len(requests), 1)
        self.assertEqual(leaked, [])

    def test_timeout_after_post_is_unknown(self) -> None:
        release = threading.Event()

        def delayed_reply(_: Request) -> Reply:
            release.wait(timeout=2)
            return 200, {}, b""

        with http_peer(delayed_reply) as (origin, requests):
            try:
                result = self.client(origin, timeout=0.05).deliver({})
            finally:
                release.set()
        self.assertEqual(result.state, "unknown")
        self.assertEqual(len(requests), 1)

    def test_response_limit_and_invalid_payload_are_enforced(self) -> None:
        with http_peer(lambda _: (200, {}, b"x" * 129)) as (origin, requests):
            client = self.client(origin, max_response_bytes=128)
            invalid = client.deliver({"bad": float("nan")})
            result = client.deliver({})
        self.assertEqual(invalid.state, "blocked")
        self.assertEqual(result.state, "unknown")
        self.assertEqual(len(requests), 1)

    def test_truncated_response_is_unknown_even_after_success_headers(self) -> None:
        with http_peer(lambda _: (200, {"Content-Length": "100"}, b"{}")) as (
            origin,
            requests,
        ):
            result = self.client(origin).deliver({})
        self.assertEqual(result.state, "unknown")
        self.assertEqual(result.http_status, 200)
        self.assertEqual(len(requests), 1)

    def test_untrusted_url_and_secret_values_fail_before_network(self) -> None:
        for url in (
            "http://example.test/api/vcsHooksReceiver/generic/519-6",
            "ftp://example.test/api/vcsHooksReceiver/generic/519-6",
            "https://user:pass@example.test/api/vcsHooksReceiver/generic/519-6",
            "https://example.test/api/vcsHooksReceiver/generic/519-6?secret=x",
            "https://example.test/api/vcsHooksReceiver/generic/519-6#x",
            "https://example.test/../api/vcsHooksReceiver/generic/519-6",
            "https://example.test/%2e%2e/api/vcsHooksReceiver/generic/519-6",
            "https://example.test/api/commands",
        ):
            with self.subTest(url=url), self.assertRaises(ValueError):
                YouTrackTransport(url, "secret", allow_http_loopback=True)
        url = "https://example.test/api/vcsHooksReceiver/generic/519-6"
        for secret in ("", " ", "x\r\nInjected:yes", "\tx", "секрет"):
            with self.subTest(secret=secret), self.assertRaises(ValueError):
                YouTrackTransport(url, secret)
        with self.assertRaises(ValueError):
            self.client("http://127.0.0.1", timeout=float("nan"))
        with self.assertRaises(ValueError):
            YouTrackTransport(
                "http://127.0.0.1/api/vcsHooksReceiver/generic/519-6", "secret"
            )

    def test_https_uses_verified_tls(self) -> None:
        client = YouTrackTransport(
            "https://example.test/api/vcsHooksReceiver/generic/519-6", "secret"
        )
        connection = client._connection()
        self.assertTrue(connection._context.check_hostname)
        self.assertEqual(connection._context.verify_mode, ssl.CERT_REQUIRED)

    def test_reconcile_paginates_and_requires_every_issue_and_exact_url(self) -> None:
        revision = "abc123"
        expected_url = "https://lore.example.test/repos/game/revision/abc123"
        calls: list[tuple[str, int]] = []

        def page(request: Request) -> Reply:
            method, path, headers, _ = request
            self.assertEqual(method, "GET")
            self.assertEqual(headers.get("Authorization"), "Bearer rest-secret")
            self.assertNotIn("X-YT-Simple-Signature", headers)
            parsed = urlsplit(path)
            self.assertTrue(parsed.path.startswith("/youtrack/api/issues/"))
            issue = parsed.path.split("/")[-2]
            query = parse_qs(parsed.query)
            self.assertEqual(query["fields"], ["$type,version,urls,state"])
            skip = int(query["$skip"][0])
            calls.append((issue, skip))
            change = {
                "$type": "VcsChange",
                "version": revision,
                "urls": [expected_url],
                "state": 0,
            }
            if skip == 0:
                change["urls"] = ["https://another-repo.example.test/revision/abc123"]
            return 200, {}, json.dumps([change]).encode()

        with http_peer(page) as (origin, requests):
            result = self.client(origin).reconcile(
                ["GAME-1", "GAME-2", "GAME-1"],
                revision=revision,
                expected_url=expected_url,
                token="rest-secret",
            )
        self.assertEqual(result.state, "found")
        self.assertEqual(result.found_ids, ("GAME-1", "GAME-2"))
        self.assertEqual(
            calls, [("GAME-1", 0), ("GAME-1", 1), ("GAME-2", 0), ("GAME-2", 1)]
        )
        self.assertEqual(len(requests), 4)

    def test_reconcile_partial_absence_does_not_report_all_found(self) -> None:
        def page(request: Request) -> Reply:
            changes = []
            if "/GAME-1/" in request[1]:
                changes = [
                    {
                        "$type": "VcsChange",
                        "version": "abc",
                        "urls": ["https://lore.test/abc"],
                        "state": 0,
                    }
                ]
            return 200, {}, json.dumps(changes).encode()

        with http_peer(page) as (origin, _):
            result = self.client(origin).reconcile(
                ["GAME-1", "GAME-2"],
                revision="abc",
                expected_url="https://lore.test/abc",
                token="secret",
            )
        self.assertEqual(result.state, "absent")
        self.assertEqual(result.found_ids, ("GAME-1",))

    def test_reconcile_ignores_detached_wrong_revision_and_pull_request(self) -> None:
        changes = [
            {
                "$type": "VcsChange",
                "version": "wrong",
                "urls": ["https://lore.test/abc"],
                "state": 0,
            },
            {
                "$type": "VcsChange",
                "version": "abc",
                "urls": ["https://lore.test/abc"],
                "state": 2,
            },
            {
                "$type": "PullRequest",
                "version": "abc",
                "urls": ["https://lore.test/abc"],
                "state": 0,
            },
        ]

        def page(request: Request) -> Reply:
            skip = int(parse_qs(urlsplit(request[1]).query)["$skip"][0])
            return 200, {}, json.dumps(changes if skip == 0 else []).encode()

        with http_peer(page) as (origin, _):
            result = self.client(origin).reconcile(
                ["GAME-1"],
                revision="abc",
                expected_url="https://lore.test/abc",
                token="secret",
            )
        self.assertEqual(result.state, "absent")
        self.assertEqual(result.found_ids, ())

    def test_reconcile_redirect_keeps_bearer_on_configured_origin(self) -> None:
        with (
            http_peer(lambda _: (200, {}, b"[]")) as (other, leaked),
            http_peer(lambda _: (302, {"Location": other + "/capture"}, b"")) as (
                origin,
                requests,
            ),
        ):
            result = self.client(origin).reconcile(
                ["GAME-1"],
                revision="abc",
                expected_url="https://lore.test/abc",
                token="secret",
            )
        self.assertEqual(result.state, "blocked")
        self.assertEqual(len(requests), 1)
        self.assertEqual(leaked, [])

    def test_reconcile_limits_malformed_errors_and_empty_input(self) -> None:
        for status, body, state in (
            (403, b"token", "blocked"),
            (503, b"token", "unknown"),
            (200, b"not JSON token", "unknown"),
            (200, b"{}", "unknown"),
            (200, b"[1]", "unknown"),
            (200, b" " * 130, "unknown"),
        ):
            with self.subTest(status=status, body=body):
                with http_peer(
                    lambda _, status=status, body=body: (status, {}, body)
                ) as (origin, requests):
                    client = self.client(origin, max_response_bytes=128)
                    empty = client.reconcile(
                        [],
                        revision="abc",
                        expected_url="https://lore.test/abc",
                        token="",
                    )
                    result = client.reconcile(
                        ["GAME-1"],
                        revision="abc",
                        expected_url="https://lore.test/abc",
                        token="secret",
                    )
                self.assertEqual(empty.state, "absent")
                self.assertEqual(result.state, state)
                self.assertEqual(len(requests), 1)
                self.assertNotIn("token", result.detail)
        with http_peer(lambda _: (200, {}, b'[{"version":"other"}]')) as (
            origin,
            requests,
        ):
            result = self.client(origin).reconcile(
                ["GAME-1"],
                revision="abc",
                expected_url="https://lore.test/abc",
                token="secret",
                max_pages=2,
            )
        self.assertEqual(result.state, "unknown")
        self.assertEqual(len(requests), 2)


if __name__ == "__main__":
    unittest.main()
