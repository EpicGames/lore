# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Declarative stub of the UCS auth service, for authenticated-path smoke tests.

Serves the `epic_urc.UrcAuthApi` and `ucs.auth.RebacApi` gRPC services
(lore-proto/proto/auth_api.proto, rebac_api.proto) plus an HTTP endpoint with
the JWKS document the lore server validates tokens against. The stub holds
**no behavior of its own**: each test registers the request/response pairs its
scenario needs with `on(...)`, and anything unregistered is denied. A test
therefore reads as the full API conversation it expects.

    token = mock.mint_token(USER1)
    mock.on("GetAuthSession", session_code="code-1").respond(
        user_token_response(USER1, token)
    )
    mock.on("CheckUserPermission", bearer=token, resource_id="urc-abc").respond(
        check_user_permission_response("urc-abc", ("read", "write"))
    )

Two things cannot be hardcoded into tests and stay as real code here. Tokens
must carry valid RS256 signatures that verify against the served JWKS, and
fresh `iat`/`exp` against the test run's clock, so `mint_token` mints them on
demand. And the messages are protobuf, which has no legible literal form, so
requests are decoded into plain dicts for matching (`_REQUEST_FIELDS`) and
responses are assembled by the `*_response` builder functions.

Matching is by equality on decoded request fields, newest registration first,
so a test can override an earlier rule mid-test (e.g. re-register a permission
check as `.deny()` to revoke access). The pseudo-field `bearer` matches the
request's credential: the `authorization` header's token, or the embedded
`TargetUser` token where the RPC carries one. Unmatched requests are refused
with a per-method default status (`_DEFAULT_DENIALS`) and logged, never
answered permissively.

As elsewhere in this suite, protobuf messages are encoded by hand via
`protobuf_wire` — the environment ships `grpcio` but no generated stubs.
"""

import base64
import json
import logging
import threading
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import cast

import grpc
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from protobuf_wire import (
    encode_bytes_field,
    encode_string_field,
    encode_varint_field,
    field_bytes,
    field_string,
    field_strings,
    parse_fields,
)

logger = logging.getLogger(__name__)

_AUTH_SERVICE = "epic_urc.UrcAuthApi"
_REBAC_SERVICE = "ucs.auth.RebacApi"


def _identity_bytes(raw: bytes) -> bytes:
    """(De)serializer for generic handlers: messages stay raw bytes."""
    return raw


class _AbortError(Exception):
    """A request refused with a gRPC status.

    grpcio's `context.abort` raises a bare `Exception`, which the catch-all in
    the handler cannot tell apart from a genuine bug, so refusals travel as
    this exception and are translated to `context.abort` in one place.
    """

    def __init__(self, code: grpc.StatusCode, details: str):
        super().__init__(details)
        self.code = code
        self.details = details


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _b64url_decode(data: str) -> bytes:
    return base64.urlsafe_b64decode(data + "=" * (-len(data) % 4))


def tamper_token(token: str) -> str:
    """`token` with its subject altered after signing: still a well-formed JWT
    with plausible claims, but the signature no longer covers the payload."""
    header, payload, signature = token.split(".")
    claims = json.loads(_b64url_decode(payload))
    claims["sub"] = claims.get("sub", "") + "-tampered"
    forged_payload = _b64url(json.dumps(claims, separators=(",", ":")).encode())
    return f"{header}.{forged_payload}.{signature}"


class MockUser:
    def __init__(self, user_id: str, display_name: str, preferred_username: str):
        self.user_id = user_id
        self.display_name = display_name
        self.preferred_username = preferred_username


# USER1 logs in through the interactive device-grant flow in the tests, USER2
# through an API key, so scenarios have two identities to combine.
USER1 = MockUser("mock-user-1", "Mock User One", "mockuser1")
USER2 = MockUser("mock-user-2", "Mock User Two", "mockuser2")
USER2_API_KEY = "mock-api-key-user-2"

# Token lifetime the response builders and `mint_token` default to.
DEFAULT_TOKEN_LIFETIME_SECONDS = 3600


# ---------------------------------------------------------------------------
# Response builders: the protobuf response payloads a test registers.
# ---------------------------------------------------------------------------


def user_token_response(
    user: MockUser,
    token: str,
    lifetime_seconds: int = DEFAULT_TOKEN_LIFETIME_SECONDS,
) -> bytes:
    """A `UserToken` response, the shape `GetAuthSession`, both external-token
    exchanges and `ExchangeUserTokenForMultiresourceToken` all answer with."""
    expires_at_ms = (int(time.time()) + lifetime_seconds) * 1000
    message = (
        encode_string_field(1, token)
        + encode_varint_field(2, expires_at_ms)
        + encode_string_field(3, user.user_id)
        + encode_string_field(4, user.display_name)
    )
    return encode_bytes_field(1, message)


def start_auth_session_response(session_code: str, login_url: str) -> bytes:
    return encode_string_field(1, session_code) + encode_string_field(2, login_url)


def check_user_permission_response(resource_id: str, permissions) -> bytes:
    """A `CheckUserPermission` answer allowing `permissions` on `resource_id`."""
    entry = encode_string_field(1, resource_id)
    for permission in permissions:
        entry += encode_string_field(2, permission)
    return encode_bytes_field(1, entry)


def lookup_user_permissions_response(
    *resource_ids: str, next_page_token: str = ""
) -> bytes:
    """A `LookupUserPermissions` answer granting `read` on each resource,
    with a continuation token when the listing has another page."""
    response = b""
    for resource_id in resource_ids:
        response += encode_bytes_field(
            1, encode_string_field(1, resource_id) + encode_string_field(2, "read")
        )
    if next_page_token:
        response += encode_string_field(2, next_page_token)
    return response


def user_info_response(*users: MockUser) -> bytes:
    """A `GetUserInfo` answer resolving the given users."""
    response = b""
    for user in users:
        response += encode_bytes_field(
            1,
            encode_string_field(1, user.user_id)
            + encode_string_field(2, user.display_name),
        )
    return response


def empty_response() -> bytes:
    """An empty message: `CreateResource`/`DeleteResource` responses, or a
    `GetAuthSession`/`RefreshAuthSession` answer carrying no token yet."""
    return b""


# ---------------------------------------------------------------------------
# Request decoding: protobuf field maps per RPC, for matching and logging.
# ---------------------------------------------------------------------------

_STRING = "string"
_STRINGS = "repeated string"
_TARGET_USER = "target_user"

# method -> {field name: (field number, kind)}
_REQUEST_FIELDS: dict[str, dict[str, tuple[int, str]]] = {
    "HealthCheck": {},
    "StartAuthSession": {"client_state": (1, _STRING)},
    "GetAuthSession": {"session_code": (1, _STRING), "client_state": (2, _STRING)},
    "RefreshAuthSession": {},
    "VerifyUser": {"target_user": (1, _TARGET_USER)},
    "ExchangeExternalTokenForUserToken": {
        "external_token": (1, _STRING),
        "token_type": (2, _STRING),
    },
    "ExchangeAPIKeyForUserToken": {"api_key": (1, _STRING)},
    "ExchangeUserTokenForMultiresourceToken": {"resource_id": (1, _STRINGS)},
    "CheckUserPermission": {
        "resource_id": (1, _STRINGS),
        "target_user": (2, _TARGET_USER),
    },
    "LookupUserPermissions": {
        "resource_filter": (1, _STRING),
        "page_token": (4, _STRING),
    },
    "GetUserInfo": {"resource_id": (1, _STRING), "user_id": (2, _STRINGS)},
    "GetUserId": {"resource_id": (1, _STRING), "user_display_name": (2, _STRING)},
    "GetProviderUserId": {"user_id": (1, _STRING)},
    "CreateResource": {"resource_id": (1, _STRING), "resource_name": (2, _STRING)},
    "DeleteResource": {"resource_id": (1, _STRING)},
}

_REBAC_METHODS = ("CreateResource", "DeleteResource")

# The status an unmatched request is refused with. Chosen to mirror how the
# real service refuses the corresponding case: a token endpoint answers "who
# are you" (UNAUTHENTICATED), a permission endpoint "you may not"
# (PERMISSION_DENIED), a session poll for an unknown code NOT_FOUND.
_DEFAULT_DENIALS: dict[str, grpc.StatusCode] = {
    "GetAuthSession": grpc.StatusCode.NOT_FOUND,
    "ExchangeExternalTokenForUserToken": grpc.StatusCode.UNAUTHENTICATED,
    "ExchangeAPIKeyForUserToken": grpc.StatusCode.UNAUTHENTICATED,
    "GetUserInfo": grpc.StatusCode.NOT_FOUND,
    "GetUserId": grpc.StatusCode.NOT_FOUND,
    "GetProviderUserId": grpc.StatusCode.NOT_FOUND,
}
_FALLBACK_DENIAL = grpc.StatusCode.PERMISSION_DENIED


def _decode_request(method: str, request: bytes) -> dict:
    fields = parse_fields(request)
    decoded: dict = {}
    for name, (number, kind) in _REQUEST_FIELDS[method].items():
        if kind == _STRING:
            decoded[name] = field_string(fields, number)
        elif kind == _STRINGS:
            decoded[name] = tuple(field_strings(fields, number))
        elif kind == _TARGET_USER:
            target_user = field_bytes(fields, number)
            decoded[name] = (
                field_string(parse_fields(target_user), 1) if target_user else ""
            )
    return decoded


class _Rule:
    """One registered request → response pair."""

    def __init__(self, method: str, matchers: dict):
        self.method = method
        self.matchers = matchers
        self.response: bytes | None = None
        self.denial: tuple[grpc.StatusCode, str] | None = None

    def respond(self, payload: bytes) -> "_Rule":
        self.response = payload
        return self

    def deny(
        self,
        code: grpc.StatusCode = grpc.StatusCode.PERMISSION_DENIED,
        details: str = "denied by test rule",
    ) -> "_Rule":
        self.denial = (code, details)
        return self

    def matches(self, request: dict) -> bool:
        for name, expected in self.matchers.items():
            value = request.get(name)
            if isinstance(value, tuple):  # repeated field: expected ∈ values
                if expected not in value:
                    return False
            elif value != expected:
                return False
        return True

    def describe(self) -> str:
        matchers = ", ".join(
            f"{name}={_excerpt(value)}" for name, value in self.matchers.items()
        )
        return f"{self.method}({matchers})"


def _excerpt(value) -> str:
    """Values for the log: tokens are long, so the middle is elided."""
    text = str(value)
    return text if len(text) <= 24 else f"{text[:10]}…{text[-10:]}"


class _JwksHttpHandler(BaseHTTPRequestHandler):
    """Serves the JWKS document and the fake browser-login page."""

    def do_GET(self):
        mock = cast(_JwksHttpServer, self.server).mock
        if self.path == "/v1/auth/jwks":
            mock.calls["jwks_fetch"] += 1
            body = json.dumps(mock.jwks()).encode("utf-8")
            self._respond(200, body, "application/json")
        elif self.path.startswith("/login/"):
            # The page exists so the login URL a test registers is real.
            self._respond(200, b"<html><body>Mock login complete.</body></html>")
        else:
            self._respond(404, b"not found")

    def _respond(self, status: int, body: bytes, content_type: str = "text/html"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        logger.debug("mock auth http: " + format, *args)


class _JwksHttpServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, mock: "MockAuthServer"):
        super().__init__(("127.0.0.1", 0), _JwksHttpHandler)
        self.mock = mock


def _is_access_token(bearer: str) -> bool:
    """Whether the bearer is an exchanged access token: one whose payload
    carries a `resources` claim. Decoded without verification -- this asks
    what kind of token it is, not whether it is genuine."""
    try:
        payload = bearer.split(".")[1]
        claims = json.loads(
            base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4))
        )
    except (IndexError, ValueError):
        return False
    return isinstance(claims, dict) and "resources" in claims


class MockAuthServer:
    """The declarative stub. `start()` binds OS-assigned loopback ports.

    Tests script it with `on(method, **matchers).respond(payload)` /
    `.deny(...)` and read `calls` / `requests` back for assertions. `reset()`
    clears the rules and records between tests; the signing key survives, so
    the lore server's cached JWKS stays valid across a module."""

    def __init__(
        self,
        issuer: str = "urc-mock-auth",
        audience: tuple[str, ...] = ("urc-tests", "127.0.0.1"),
        kid: str = "mock-auth-key-1",
    ):
        self.issuer = issuer
        # The audience does double duty: the lore server accepts any listed
        # value as `aud`, and the CLI requires the remote's domain (127.0.0.1
        # for these tests) to appear among the token's acceptable domains.
        self.audience = list(audience)
        self.kid = kid

        self._private_key = rsa.generate_private_key(
            public_exponent=65537, key_size=2048
        )
        self._public_key = self._private_key.public_key()

        self._lock = threading.Lock()
        self._rules: list[_Rule] = []
        # RPC name -> number of calls, and every decoded request, in order.
        self.calls: Counter[str] = Counter()
        self.requests: list[tuple[str, dict]] = []

        self._grpc_server: grpc.Server | None = None
        self._http_server: _JwksHttpServer | None = None
        self._http_thread: threading.Thread | None = None
        self.grpc_port: int | None = None
        self.http_port: int | None = None

    # ------------------------------------------------------------------
    # Scripting surface
    # ------------------------------------------------------------------

    def on(self, method: str, **matchers) -> _Rule:
        """Register a rule: requests to `method` whose decoded fields equal
        `matchers` get the rule's response. Field names are the proto field
        names (see `_REQUEST_FIELDS`), plus `bearer` for the request's
        credential. Newest registration wins, so re-registering overrides."""
        if method not in _REQUEST_FIELDS:
            raise ValueError(f"unknown RPC method {method!r}")
        unknown = set(matchers) - set(_REQUEST_FIELDS[method]) - {"bearer"}
        if unknown:
            raise ValueError(f"{method} has no fields {sorted(unknown)}")
        rule = _Rule(method, matchers)
        with self._lock:
            self._rules.insert(0, rule)
        return rule

    def reset(self) -> None:
        """Drop every rule and record. Health checks answer unconditionally,
        registered here so even they are visible as a rule."""
        with self._lock:
            self._rules.clear()
            self.calls.clear()
            self.requests.clear()
        self.on("HealthCheck").respond(encode_string_field(1, "ok"))

    def requests_for(self, method: str) -> list[dict]:
        with self._lock:
            return [fields for name, fields in self.requests if name == method]

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self) -> "MockAuthServer":
        self.reset()

        self._http_server = _JwksHttpServer(self)
        self.http_port = self._http_server.server_address[1]
        self._http_thread = threading.Thread(
            target=self._http_server.serve_forever, daemon=True
        )
        self._http_thread.start()

        self._grpc_server = grpc.server(ThreadPoolExecutor(max_workers=8))
        auth_methods = {
            name: self._unary(name)
            for name in _REQUEST_FIELDS
            if name not in _REBAC_METHODS
        }
        rebac_methods = {name: self._unary(name) for name in _REBAC_METHODS}
        self._grpc_server.add_generic_rpc_handlers(
            (
                grpc.method_handlers_generic_handler(_AUTH_SERVICE, auth_methods),
                grpc.method_handlers_generic_handler(_REBAC_SERVICE, rebac_methods),
            )
        )
        self.grpc_port = self._grpc_server.add_insecure_port("127.0.0.1:0")
        self._grpc_server.start()
        logger.info(
            "Mock auth stub: grpc on %d, jwks on %d", self.grpc_port, self.http_port
        )
        return self

    def stop(self):
        if self._grpc_server is not None:
            self._grpc_server.stop(grace=None)
            self._grpc_server = None
        if self._http_server is not None:
            self._http_server.shutdown()
            self._http_server.server_close()
            self._http_server = None

    @property
    def auth_url(self) -> str:
        """The auth endpoint URL, as advertised to clients and used by the
        server's own auth/rebac clients. Plain http: gRPC without TLS."""
        return f"http://127.0.0.1:{self.grpc_port}"

    @property
    def jwks_url(self) -> str:
        return f"http://127.0.0.1:{self.http_port}/v1/auth/jwks"

    def login_page_url(self, session_code: str) -> str:
        """The URL a registered `StartAuthSession` response should point at."""
        return f"http://127.0.0.1:{self.http_port}/login/{session_code}"

    # ------------------------------------------------------------------
    # Token minting: real signatures, fresh timestamps
    # ------------------------------------------------------------------

    def jwks(self) -> dict:
        numbers = self._public_key.public_numbers()

        def uint_b64(value: int) -> str:
            return _b64url(value.to_bytes((value.bit_length() + 7) // 8, "big"))

        return {
            "keys": [
                {
                    "kty": "RSA",
                    "n": uint_b64(numbers.n),
                    "e": uint_b64(numbers.e),
                    "kid": self.kid,
                    "alg": "RS256",
                    "use": "sig",
                }
            ]
        }

    def mint_token(
        self,
        user: MockUser,
        resources: list[dict] | None = None,
        lifetime_seconds: int = DEFAULT_TOKEN_LIFETIME_SECONDS,
    ) -> str:
        """A signed JWT for `user`. With `resources`, an authorization token
        (the multiresource-exchange shape); without, an authentication token.

        A negative `lifetime_seconds` mints an already-expired token, for
        expiry-handling tests."""
        now = int(time.time())
        claims = {
            "iss": self.issuer,
            "sub": user.user_id,
            "aud": self.audience,
            # `iat` stays in the past for a pre-expired token, so the pair is
            # internally consistent: issued then, expired since.
            "iat": min(now, now + lifetime_seconds - 1),
            "exp": now + lifetime_seconds,
            "name": user.display_name,
            "preferred_username": user.preferred_username,
            "is_service_account": False,
            "root_domains": ["127.0.0.1"],
        }
        if resources is not None:
            claims["resources"] = resources
        header = {"alg": "RS256", "typ": "JWT", "kid": self.kid}
        signing_input = (
            _b64url(json.dumps(header, separators=(",", ":")).encode())
            + "."
            + _b64url(json.dumps(claims, separators=(",", ":")).encode())
        ).encode("ascii")
        signature = self._private_key.sign(
            signing_input, padding.PKCS1v15(), hashes.SHA256()
        )
        return signing_input.decode("ascii") + "." + _b64url(signature)

    def verify_token(self, token: str) -> dict:
        """Claims of a token this stub minted. Raises ValueError otherwise.
        Matching never verifies tokens — this exists for test assertions."""
        try:
            header_b64, claims_b64, signature_b64 = token.split(".")
            self._public_key.verify(
                _b64url_decode(signature_b64),
                f"{header_b64}.{claims_b64}".encode("ascii"),
                padding.PKCS1v15(),
                hashes.SHA256(),
            )
            claims = json.loads(_b64url_decode(claims_b64))
        except ValueError:
            raise
        except Exception as e:
            raise ValueError(f"invalid token: {e}") from e
        if claims.get("exp", 0) < time.time():
            raise ValueError("token expired")
        return claims

    # ------------------------------------------------------------------
    # Request handling: decode, record, match, answer
    # ------------------------------------------------------------------

    def _unary(self, method: str):
        def handler(request: bytes, context: grpc.ServicerContext) -> bytes:
            try:
                return self._serve(method, request, context)
            except _AbortError as refusal:
                context.abort(refusal.code, refusal.details)
                raise  # unreachable; context.abort raises
            except Exception:
                logger.exception("mock auth: %s failed", method)
                context.abort(grpc.StatusCode.INTERNAL, "mock auth internal error")
                raise  # unreachable; context.abort raises

        return grpc.unary_unary_rpc_method_handler(
            handler,
            request_deserializer=_identity_bytes,
            response_serializer=_identity_bytes,
        )

    def _serve(
        self, method: str, request: bytes, context: grpc.ServicerContext
    ) -> bytes:
        decoded = _decode_request(method, request)
        decoded["bearer"] = self._credential(decoded, context)
        with self._lock:
            self.calls[method] += 1
            self.requests.append((method, decoded))
            rules = [rule for rule in self._rules if rule.method == method]

        # The real auth service refuses exchanged access tokens as
        # CheckUserPermission credentials (UNAUTHENTICATED, INVALID_FORMAT on
        # the authorization field). only identity tokens may ask. Enforced
        # before rule matching so no test can script the unreal case.
        if method == "CheckUserPermission" and _is_access_token(decoded["bearer"]):
            logger.info(
                "mock auth: %s refused: an access token is not a credential", method
            )
            raise _AbortError(
                grpc.StatusCode.UNAUTHENTICATED,
                "invalid or expired authentication token",
            )

        for rule in rules:
            if not rule.matches(decoded):
                continue
            if rule.denial is not None:
                logger.info("mock auth: %s denied by rule %s", method, rule.describe())
                raise _AbortError(*rule.denial)
            logger.info("mock auth: %s answered by rule %s", method, rule.describe())
            assert rule.response is not None, f"rule {rule.describe()} has no response"
            return rule.response

        logger.info(
            "mock auth: %s unmatched, refusing: %s",
            method,
            {name: _excerpt(value) for name, value in decoded.items()},
        )
        raise _AbortError(
            _DEFAULT_DENIALS.get(method, _FALLBACK_DENIAL),
            f"no test rule matches this {method} request",
        )

    @staticmethod
    def _credential(decoded: dict, context: grpc.ServicerContext) -> str:
        """The request's credential: an embedded TargetUser token where the
        RPC carries one, else the authorization header's bearer token."""
        if decoded.get("target_user"):
            return decoded["target_user"]
        for key, value in context.invocation_metadata():
            if key.lower() == "authorization" and isinstance(value, str):
                if value.startswith("Bearer "):
                    return value[len("Bearer ") :]
                break
        return ""
