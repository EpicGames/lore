# Copyright 2026 RedRatInHat
# SPDX-License-Identifier: MIT

"""Bounded YouTrack HTTP requests with explicit uncertain-delivery outcomes."""

from __future__ import annotations

import http.client
import ipaddress
import json
import math
import re
import ssl
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
from typing import Any, Literal
from urllib.parse import quote, unquote, urlencode, urlsplit


@dataclass(frozen=True)
class DeliveryResult:
    """An accepted response confirms HTTP acceptance, not issue processing."""

    state: Literal["accepted", "retry", "blocked", "unknown"]
    http_status: int | None
    detail: str
    retry_after: int | None = None


@dataclass(frozen=True)
class ReconciliationResult:
    """Only ``found`` confirms attachment to every requested issue."""

    state: Literal["found", "absent", "blocked", "unknown"]
    http_status: int | None
    detail: str
    found_ids: tuple[str, ...] = ()


_RECEIVER_PATH = "/api/vcsHooksReceiver/generic/"
_ISSUE_ID = re.compile(r"(?:[A-Za-z][A-Za-z0-9_]*|[0-9]+)-[0-9]+\Z")
_MAX_REQUEST_BYTES = 4 * 1024 * 1024


def _validate_secret(value: str) -> None:
    if (
        not isinstance(value, str)
        or not value.strip()
        or any(ord(character) < 32 or ord(character) > 126 for character in value)
    ):
        raise ValueError("A nonempty printable ASCII secret is required")


def _retry_after_seconds(value: str | None) -> int | None:
    if not value:
        return None
    if value.isascii() and value.isdecimal():
        try:
            return max(1, int(value))
        except ValueError:
            return None
    try:
        date = parsedate_to_datetime(value)
        if date.tzinfo is None:
            return None
        return max(1, math.ceil((date - datetime.now(UTC)).total_seconds()))
    except (ValueError, TypeError, OverflowError):
        return None


class YouTrackTransport:
    """Send once to one validated receiver, and optionally inspect its REST API.

    No request follows redirects or retries automatically. The ``retry`` outcome
    is reserved for a failed connection before any HTTP request was attempted.
    ``timeout`` is a socket timeout, not an end-to-end deadline.
    """

    def __init__(
        self,
        receiver_url: str,
        signature: str,
        *,
        timeout: float = 15.0,
        allow_http_loopback: bool = False,
        max_response_bytes: int = 65536,
    ) -> None:
        _validate_secret(signature)
        if not isinstance(receiver_url, str) or len(receiver_url) > 4096:
            raise ValueError("Invalid YouTrack receiver URL")
        if any(
            character.isspace() or ord(character) < 32 for character in receiver_url
        ):
            raise ValueError("Invalid YouTrack receiver URL")
        try:
            parsed = urlsplit(receiver_url)
            port = parsed.port
        except ValueError:
            raise ValueError("Invalid YouTrack receiver URL") from None
        if (
            not parsed.hostname
            or parsed.username is not None
            or parsed.password is not None
            or parsed.query
            or parsed.fragment
            or "\\" in receiver_url
            or (port is not None and not 1 <= port <= 65535)
        ):
            raise ValueError("Invalid YouTrack receiver URL")
        try:
            loopback = ipaddress.ip_address(parsed.hostname).is_loopback
        except ValueError:
            loopback = parsed.hostname.lower() in ("localhost", "localhost.")
        if parsed.scheme != "https" and not (
            parsed.scheme == "http" and allow_http_loopback and loopback
        ):
            raise ValueError("HTTPS is required except for explicitly allowed loopback")
        prefix, separator, receiver_id = parsed.path.partition(_RECEIVER_PATH)
        decoded_path = unquote(parsed.path)
        if (
            not separator
            or not re.fullmatch(r"[A-Za-z0-9_-]{1,256}", receiver_id)
            or any(part in (".", "..") for part in decoded_path.split("/"))
            or "\\" in decoded_path
            or "//" in decoded_path
        ):
            raise ValueError("A Generic VCS receiver resource URL is required")
        if (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or not math.isfinite(timeout)
            or timeout <= 0
        ):
            raise ValueError("Timeout must be a finite positive number")
        if (
            isinstance(max_response_bytes, bool)
            or not isinstance(max_response_bytes, int)
            or max_response_bytes < 1
        ):
            raise ValueError("Response size limit must be a positive integer")
        self._scheme = parsed.scheme
        self._host = parsed.hostname
        self._port = port
        self._receiver_path = parsed.path
        self._api_path = prefix + "/api"
        self._signature = signature
        self._timeout = timeout
        self._max_response_bytes = max_response_bytes
        self._tls_context = ssl.create_default_context()

    def _connection(self) -> http.client.HTTPConnection:
        if self._scheme == "https":
            return http.client.HTTPSConnection(
                self._host,
                self._port,
                timeout=self._timeout,
                context=self._tls_context,
            )
        return http.client.HTTPConnection(self._host, self._port, timeout=self._timeout)

    def deliver(self, payload: Mapping[str, Any]) -> DeliveryResult:
        """POST a payload once; a caller must persist an attempt before calling."""
        try:
            body = json.dumps(
                payload, ensure_ascii=False, allow_nan=False, separators=(",", ":")
            ).encode("utf-8")
        except (TypeError, ValueError, UnicodeError, RecursionError):
            return DeliveryResult("blocked", None, "Payload is not valid JSON")
        if len(body) > _MAX_REQUEST_BYTES:
            return DeliveryResult("blocked", None, "Payload exceeds the request limit")

        connection = self._connection()
        try:
            try:
                connection.connect()
            except (OSError, http.client.HTTPException, ValueError):
                return DeliveryResult(
                    "retry", None, "Connection failed before sending an HTTP request"
                )
            try:
                connection.request(
                    "POST",
                    self._receiver_path,
                    body=body,
                    headers={
                        "Content-Type": "application/json",
                        "Accept": "application/json",
                        "X-YT-Simple-Signature": self._signature,
                    },
                )
                response = connection.getresponse()
                status = response.status
                retry_after = _retry_after_seconds(response.getheader("Retry-After"))
                data = response.read(self._max_response_bytes + 1)
                if len(data) > self._max_response_bytes:
                    return DeliveryResult(
                        "unknown",
                        status,
                        "Response exceeds the size limit",
                        retry_after,
                    )
                if response.length not in (None, 0):
                    return DeliveryResult(
                        "unknown", status, "Response ended before its declared length"
                    )
            except (OSError, http.client.HTTPException, ValueError):
                return DeliveryResult(
                    "unknown",
                    None,
                    "Request outcome is unknown; reconcile before action",
                )
        finally:
            connection.close()

        if 200 <= status < 300:
            return DeliveryResult(
                "accepted", status, "HTTP accepted; issue processing is not confirmed"
            )
        if status >= 500 or status in (408, 425, 429):
            return DeliveryResult(
                "unknown",
                status,
                "Receiver outcome requires reconciliation",
                retry_after,
            )
        return DeliveryResult(
            "blocked", status, "Receiver rejected the request; no automatic replay"
        )

    def reconcile(
        self,
        issue_ids: Sequence[str],
        *,
        revision: str,
        expected_url: str,
        token: str,
        page_size: int = 100,
        max_pages: int = 100,
    ) -> ReconciliationResult:
        """Inspect attachment on every issue; absence never authorizes a resend.

        Presence proves a repository/revision association, not command effects
        or which receiver created an association shared by several integrations.
        ``max_pages`` bounds the total number of GET requests across all issues.
        """
        if (
            isinstance(issue_ids, (str, bytes))
            or any(
                not isinstance(issue, str) or not _ISSUE_ID.fullmatch(issue)
                for issue in issue_ids
            )
            or not isinstance(revision, str)
            or not revision
            or not isinstance(expected_url, str)
            or not expected_url
        ):
            raise ValueError("Valid issue IDs, revision and expected URL are required")
        if (
            isinstance(page_size, bool)
            or not isinstance(page_size, int)
            or not 1 <= page_size <= 1000
            or isinstance(max_pages, bool)
            or not isinstance(max_pages, int)
            or not 1 <= max_pages <= 1000
        ):
            raise ValueError("Pagination limits must be between 1 and 1000")
        issues = tuple(dict.fromkeys(issue_ids))
        if not issues:
            return ReconciliationResult("absent", None, "No issue IDs to inspect")
        _validate_secret(token)

        found: list[str] = []
        remaining_pages = max_pages
        for issue_id in issues:
            skip = 0
            while remaining_pages:
                remaining_pages -= 1
                path = (
                    self._api_path
                    + "/issues/"
                    + quote(issue_id, safe="")
                    + "/vcsChanges"
                )
                query = urlencode(
                    {
                        "fields": "$type,version,urls,state",
                        "$top": page_size,
                        "$skip": skip,
                    }
                )
                result, page = self._read_page(path + "?" + query, token)
                if result is not None:
                    return ReconciliationResult(
                        result.state, result.http_status, result.detail, tuple(found)
                    )
                if not page:
                    break
                if any(
                    change.get("$type") == "VcsChange"
                    and change.get("version") == revision
                    and isinstance(change.get("urls"), list)
                    and expected_url in change["urls"]
                    and type(change.get("state")) is int
                    and change["state"] in (0, 1, 3)
                    for change in page
                ):
                    found.append(issue_id)
                    break
                skip += len(page)
            else:
                return ReconciliationResult(
                    "unknown", 200, "Reconciliation page limit reached", tuple(found)
                )

        if len(found) == len(issues):
            return ReconciliationResult(
                "found",
                200,
                "Attachment is visible on every requested issue",
                tuple(found),
            )
        return ReconciliationResult(
            "absent",
            200,
            "Not all attachments are visible; resending remains blocked",
            tuple(found),
        )

    def _read_page(
        self, path: str, token: str
    ) -> tuple[ReconciliationResult | None, list[dict[str, Any]]]:
        connection = self._connection()
        try:
            connection.connect()
            connection.request(
                "GET",
                path,
                headers={
                    "Authorization": "Bearer " + token,
                    "Accept": "application/json",
                },
            )
            response = connection.getresponse()
            status = response.status
            if not 200 <= status < 300:
                state = (
                    "unknown"
                    if status >= 500 or status in (408, 425, 429)
                    else "blocked"
                )
                return ReconciliationResult(
                    state, status, "REST inspection did not succeed"
                ), []
            data = response.read(self._max_response_bytes + 1)
            if len(data) > self._max_response_bytes:
                return ReconciliationResult(
                    "unknown", status, "REST response exceeds the size limit"
                ), []
            if response.length not in (None, 0):
                return ReconciliationResult(
                    "unknown", status, "REST response ended early"
                ), []
            page = json.loads(data)
            if not isinstance(page, list) or any(
                not isinstance(item, dict) for item in page
            ):
                return ReconciliationResult(
                    "unknown", status, "REST response is not a list of changes"
                ), []
            return None, page
        except (OSError, http.client.HTTPException, ValueError, RecursionError):
            return ReconciliationResult(
                "unknown", None, "REST inspection could not be completed"
            ), []
        finally:
            connection.close()
