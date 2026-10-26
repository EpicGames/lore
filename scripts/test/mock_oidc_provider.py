# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""A stub OpenID Connect provider, for the OIDC login smoke tests.

Serves what a device-grant login touches: the discovery document, the device
authorization endpoint (RFC 8628 §3.1), the token endpoint for the device
code grant (§3.4) and the JWKS the lore server verifies tokens against. It
mints RS256 access tokens, as `MockAuthServer` does for the gRPC path.

The token endpoint also serves the token exchange grant (RFC 8693) that Tier 2
authorizes partitions with. It reads the partition from the `resource` or
`scope` the request carries, under the template the test sets, and answers
with a token whose `resources` claim grants what `grants` holds for that
partition, or with `invalid_target` for a partition `grants` does not name.
An exchange whose subject is an API key, under `API_KEY_TOKEN_TYPE` or its
short name `api-key` as UCS Auth accepts, logs in the user `api_keys` binds
the key to, answering as the device grant does, or answers `invalid_grant`
for a key `api_keys` does not name.

The client credentials grant (RFC 6749 §4.4) logs in the user `clients` binds
a registered client to. The client authenticates with HTTP Basic or in the
form, whichever `token_endpoint_auth_methods` lists; with no list the
discovery document leaves the field out and Basic is the RFC 8414 default.
Wrong credentials, or credentials in a place the list does not name, answer
`invalid_client` with HTTP 401.

A device grant that asked for `offline_access` also hands out a refresh token,
and the refresh token grant (RFC 6749 §6) rotates it: each one is good for one
refresh, and presenting one again revokes every refresh token of that login,
as UCS Auth does. `token_lifetime` sets the lifetime of the access tokens the
two grants and the partition exchange mint, negative for one that has expired
already.

With `revocation_endpoint` set, the discovery document advertises it. The
stub serves revocation (RFC 7009) at `/revoke`: a refresh token revoked there
revokes every refresh token of its login. Pointing `revocation_endpoint`
elsewhere, such as at a closed port, makes the provider unreachable for
revocation alone.

The token endpoint answers from a script the test sets: a list of RFC 8628
§3.5 error codes answered in order, then the token. An empty script hands out
the token on the first poll.

    provider.script_polls(["authorization_pending", "slow_down"])
    ... lore auth login ...
    assert [r.form["grant_type"] for r in provider.requests_to("/token")]
"""

import base64
import json
import logging
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import cast
from urllib.parse import parse_qsl, unquote_plus

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa

logger = logging.getLogger(__name__)

DEVICE_CODE_GRANT = "urn:ietf:params:oauth:grant-type:device_code"
CLIENT_CREDENTIALS_GRANT = "client_credentials"
REFRESH_TOKEN_GRANT = "refresh_token"
TOKEN_EXCHANGE_GRANT = "urn:ietf:params:oauth:grant-type:token-exchange"
ACCESS_TOKEN_TYPE = "urn:ietf:params:oauth:token-type:access_token"
API_KEY_TOKEN_TYPE = "urn:lore:params:oauth:token-type:api-key"
DEFAULT_TOKEN_LIFETIME_SECONDS = 3600


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


@dataclass
class OidcUser:
    user_id: str
    display_name: str
    preferred_username: str
    # The value minted into the `identity` claim, which names the user when
    # `sub` does not: a durable id beside a `sub` that differs per client, as
    # Entra's `oid` is. `None` mints no such claim.
    identity: str | None = None


USER = OidcUser("oidc-user-1", "OIDC User One", "oidcuser1")
KEY_USER = OidcUser("oidc-key-user", "OIDC Key User", "oidckeyuser")
SERVICE_ACCOUNT = OidcUser("service-account-ci", "", "service-account-ci-bot")
PAIRWISE_USER = OidcUser(
    "pairwise-sub-1", "Pairwise User", "pairwiseuser", identity="stable-oid-1"
)


@dataclass
class Received:
    """A request the provider saw: its path and the form it carried."""

    method: str
    path: str
    headers: dict[str, str]
    form: dict[str, str] = field(default_factory=dict)
    # Every form field in the order sent, a repeated one once per value.
    fields: list[tuple[str, str]] = field(default_factory=list)


class _Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        provider = cast(_Server, self.server).provider
        self._record({})
        if self.path == "/.well-known/openid-configuration":
            self._json(200, provider.discovery_document())
        elif self.path == "/jwks":
            self._json(200, provider.jwks())
        elif self.path.startswith("/activate"):
            self._respond(200, b"<html><body>Approve the device here.</body></html>")
        else:
            self._respond(404, b"not found")

    def do_POST(self):
        provider = cast(_Server, self.server).provider
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode("utf-8")
        fields = parse_qsl(body)
        form = dict(fields)
        self._record(form, fields)
        if self.path == "/device":
            self._json(*provider.device_authorization(form))
        elif self.path == "/token":
            self._json(*provider.token(form, self.headers.get("Authorization")))
        elif self.path == "/revoke":
            status, payload = provider.revoke(form)
            if payload is None:
                self._respond(status, b"")
            else:
                self._json(status, payload)
        else:
            self._respond(404, b"not found")

    def _record(
        self, form: dict[str, str], fields: list[tuple[str, str]] | None = None
    ) -> None:
        provider = cast(_Server, self.server).provider
        headers = {name.lower(): value for name, value in self.headers.items()}
        with provider._lock:
            provider.requests.append(
                Received(
                    self.command, self.path.split("?")[0], headers, form, fields or []
                )
            )

    def _json(self, status: int, payload: dict) -> None:
        self._respond(status, json.dumps(payload).encode("utf-8"), "application/json")

    def _respond(self, status: int, body: bytes, content_type: str = "text/html"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        logger.debug("mock oidc provider: " + format, *args)


def _claims(token: str) -> dict:
    payload = token.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))


class _Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, provider: "MockOidcProvider"):
        super().__init__(("127.0.0.1", 0), _Handler)
        self.provider = provider


class MockOidcProvider:
    """The stub. `start()` binds a loopback port; the issuer is its base URL."""

    def __init__(
        self,
        client_id: str = "lore-cli",
        audience: tuple[str, ...] = ("lore-tests", "127.0.0.1"),
        kid: str = "mock-oidc-key-1",
        interval: int = 1,
    ):
        self.client_id = client_id
        # The lore server accepts any listed value as `aud`, and the CLI needs
        # the remote's domain among the token's acceptable domains.
        self.audience = list(audience)
        self.kid = kid
        self.interval = interval
        self.device_code = "mock-device-code"
        self.user_code = "WDJB-MJHT"
        self.user = USER

        self._private_key = rsa.generate_private_key(
            public_exponent=65537, key_size=2048
        )
        self._public_key = self._private_key.public_key()
        self._lock = threading.Lock()
        self._poll_script: list[str] = []
        self.requests: list[Received] = []
        # The template the exchange reads the partition from, and the request
        # parameter carrying it: "resource" or "scope".
        self.exchange_template = ""
        self.exchange_carrier = "resource"
        # Partition id to the actions an exchange for it grants.
        self.grants: dict[str, list[str]] = {}
        # API key to the user an exchange of it logs in.
        self.api_keys: dict[str, OidcUser] = {}
        # Registered client id to its secret and the user it logs in as.
        self.clients: dict[str, tuple[str, OidcUser]] = {}
        # `token_endpoint_auth_methods_supported`; `None` leaves it out.
        self.token_endpoint_auth_methods: list[str] | None = None
        # The lifetime of the access tokens the device and refresh grants and the
        # partition exchange mint.
        self.token_lifetime = DEFAULT_TOKEN_LIFETIME_SECONDS
        # The scope the last device authorization asked for.
        self._device_scope = ""
        # Refresh tokens in the order issued, each to its login and whether it
        # is still good.
        self.refresh_tokens: dict[str, tuple[int, bool]] = {}
        # Whether the refresh grant consumes the token and then answers HTTP 500, as a
        # provider whose answer is lost on the way back.
        self.lose_refresh_answers = False
        # The advertised `revocation_endpoint`; `None` leaves it out.
        self.revocation_endpoint: str | None = None
        self._logins = 0
        self._minted: set[str] = set()
        self._server: _Server | None = None
        self._thread: threading.Thread | None = None
        self.port: int | None = None

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self) -> "MockOidcProvider":
        self._server = _Server(self)
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        logger.info("Mock OIDC provider on %d", self.port)
        return self

    def stop(self) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
            self._server = None

    def reset(self) -> None:
        with self._lock:
            self._poll_script.clear()
            self.requests.clear()
            self.grants.clear()
            self.api_keys.clear()
            self.clients.clear()
            self.token_endpoint_auth_methods = None
            self.user = USER
            self.token_lifetime = DEFAULT_TOKEN_LIFETIME_SECONDS
            self._device_scope = ""
            self.refresh_tokens.clear()
            self.lose_refresh_answers = False
            self.revocation_endpoint = None

    def reset_requests(self) -> None:
        with self._lock:
            self.requests.clear()

    # ------------------------------------------------------------------
    # Scripting surface
    # ------------------------------------------------------------------

    def script_polls(self, errors: list[str]) -> None:
        """Answer the next polls with these RFC 8628 §3.5 errors in order, then
        the token. `access_denied` and `expired_token` end the script: every
        poll after them answers the same."""
        with self._lock:
            self._poll_script = list(errors)

    def requests_to(self, path: str) -> list[Received]:
        with self._lock:
            return [r for r in self.requests if r.path == path]

    def valid_refresh_tokens(self) -> list[str]:
        with self._lock:
            return [token for token, (_, valid) in self.refresh_tokens.items() if valid]

    def revoke_refresh_tokens(self) -> None:
        with self._lock:
            for token, (login, _) in self.refresh_tokens.items():
                self.refresh_tokens[token] = (login, False)

    # ------------------------------------------------------------------
    # Endpoints
    # ------------------------------------------------------------------

    @property
    def issuer(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    @property
    def verification_uri_complete(self) -> str:
        return f"{self.issuer}/activate?user_code={self.user_code}"

    def discovery_document(self) -> dict:
        document = {
            "issuer": self.issuer,
            "token_endpoint": f"{self.issuer}/token",
            "jwks_uri": f"{self.issuer}/jwks",
            "device_authorization_endpoint": f"{self.issuer}/device",
            "scopes_supported": ["openid", "offline_access"],
        }
        if self.token_endpoint_auth_methods is not None:
            document["token_endpoint_auth_methods_supported"] = (
                self.token_endpoint_auth_methods
            )
        if self.revocation_endpoint is not None:
            document["revocation_endpoint"] = self.revocation_endpoint
        return document

    def device_authorization(self, form: dict[str, str]) -> tuple[int, dict]:
        if form.get("client_id") != self.client_id:
            return 400, {"error": "invalid_client"}
        with self._lock:
            self._device_scope = form.get("scope", "")
        return 200, {
            "device_code": self.device_code,
            "user_code": self.user_code,
            "verification_uri": f"{self.issuer}/activate",
            "verification_uri_complete": self.verification_uri_complete,
            "expires_in": 300,
            "interval": self.interval,
        }

    def token(
        self, form: dict[str, str], authorization: str | None
    ) -> tuple[int, dict]:
        if form.get("grant_type") == TOKEN_EXCHANGE_GRANT:
            return self.token_exchange(form)
        if form.get("grant_type") == CLIENT_CREDENTIALS_GRANT:
            return self.client_credentials(form, authorization)
        if form.get("grant_type") == REFRESH_TOKEN_GRANT:
            return self.refresh(form)
        if form.get("grant_type") != DEVICE_CODE_GRANT:
            return 400, {"error": "unsupported_grant_type"}
        if form.get("client_id") != self.client_id:
            return 400, {"error": "invalid_client"}
        if form.get("device_code") != self.device_code:
            return 400, {"error": "invalid_grant"}
        with self._lock:
            if self._poll_script:
                error = self._poll_script[0]
                if error not in ("access_denied", "expired_token"):
                    self._poll_script.pop(0)
                return 400, {"error": error}
            scope = self._device_scope
        response = {
            "access_token": self.mint_token(self.user, self.token_lifetime),
            "token_type": "Bearer",
            "expires_in": max(self.token_lifetime, 0),
            "scope": scope,
            "id_token": self.mint_id_token(self.user),
        }
        if "offline_access" in scope.split():
            with self._lock:
                self._logins += 1
                response["refresh_token"] = self._issue_refresh_token(self._logins)
        return 200, response

    def refresh(self, form: dict[str, str]) -> tuple[int, dict]:
        """RFC 6749 §6 with rotation: the presented token is consumed, and a
        consumed one presented again revokes its whole login."""
        if form.get("client_id") != self.client_id:
            return 400, {"error": "invalid_client"}
        with self._lock:
            presented = self.refresh_tokens.get(form.get("refresh_token", ""))
            if presented is None:
                return 400, {"error": "invalid_grant"}
            login, valid = presented
            if not valid:
                for token, (other, _) in self.refresh_tokens.items():
                    if other == login:
                        self.refresh_tokens[token] = (login, False)
                return 400, {"error": "invalid_grant"}
            self.refresh_tokens[form["refresh_token"]] = (login, False)
            refresh_token = self._issue_refresh_token(login)
            if self.lose_refresh_answers:
                return 500, {"error": "server_error"}
        return 200, {
            "access_token": self.mint_token(self.user, self.token_lifetime),
            "token_type": "Bearer",
            "expires_in": max(self.token_lifetime, 0),
            "refresh_token": refresh_token,
        }

    def revoke(self, form: dict[str, str]) -> tuple[int, dict | None]:
        """RFC 7009 §2.2: a token this stub does not know is answered as one
        revoked, with HTTP 200 and no body."""
        if form.get("client_id") != self.client_id:
            return 401, {"error": "invalid_client"}
        token = form.get("token", "")
        with self._lock:
            revoked = self.refresh_tokens.get(token)
            if revoked is not None:
                login, _ = revoked
                for other, (other_login, _) in self.refresh_tokens.items():
                    if other_login == login:
                        self.refresh_tokens[other] = (login, False)
        return 200, None

    def _issue_refresh_token(self, login: int) -> str:
        """A new refresh token for `login`. The caller holds the lock."""
        token = f"mock-refresh-{login}-{len(self.refresh_tokens) + 1}"
        self.refresh_tokens[token] = (login, True)
        return token

    def token_exchange(self, form: dict[str, str]) -> tuple[int, dict]:
        """RFC 8693 §2.1: the subject token must be an unexpired one this stub
        minted, and the partition is read from the configured carrier."""
        if form.get("client_id") != self.client_id:
            return 400, {"error": "invalid_client"}
        if form.get("subject_token_type") in (API_KEY_TOKEN_TYPE, "api-key"):
            return self.api_key_exchange(form)
        with self._lock:
            minted = form.get("subject_token") in self._minted
        if not minted or form.get("subject_token_type") != ACCESS_TOKEN_TYPE:
            return 400, {"error": "invalid_grant"}
        if _claims(form["subject_token"])["exp"] <= time.time():
            return 400, {"error": "invalid_grant", "error_description": "expired"}
        prefix, _, suffix = self.exchange_template.partition("{id}")
        named = form.get(self.exchange_carrier, "")
        partition = named.removeprefix(prefix).removesuffix(suffix)
        if not named.startswith(prefix) or partition not in self.grants:
            return 400, {"error": "invalid_target"}
        resources = [
            {"resource_id": f"urc-{partition}", "permission": self.grants[partition]}
        ]
        return 200, {
            "access_token": self.mint_token(
                self.user, self.token_lifetime, resources=resources
            ),
            "issued_token_type": ACCESS_TOKEN_TYPE,
            "token_type": "Bearer",
            "expires_in": max(self.token_lifetime, 0),
        }

    def client_credentials(
        self, form: dict[str, str], authorization: str | None
    ) -> tuple[int, dict]:
        """RFC 6749 §4.4.2, with the client authenticated as §2.3.1 allows
        and the discovery document names."""
        methods = self.token_endpoint_auth_methods or ["client_secret_basic"]
        if authorization is not None and authorization.startswith("Basic "):
            if "client_secret_basic" not in methods:
                return 401, {"error": "invalid_client"}
            decoded = base64.b64decode(authorization.removeprefix("Basic ")).decode()
            client_id, _, client_secret = decoded.partition(":")
            client_id, client_secret = (
                unquote_plus(client_id),
                unquote_plus(client_secret),
            )
        elif "client_secret" in form:
            if "client_secret_post" not in methods:
                return 401, {"error": "invalid_client"}
            client_id, client_secret = form.get("client_id", ""), form["client_secret"]
        else:
            return 401, {"error": "invalid_client"}
        with self._lock:
            registered = self.clients.get(client_id)
        if registered is None or registered[0] != client_secret:
            return 401, {"error": "invalid_client"}
        return 200, {
            "access_token": self.mint_token(registered[1]),
            "token_type": "Bearer",
            "expires_in": DEFAULT_TOKEN_LIFETIME_SECONDS,
        }

    def api_key_exchange(self, form: dict[str, str]) -> tuple[int, dict]:
        """A foreign credential exchange: the key alone names the user, and
        the answer is the device grant's."""
        if "resource" in form or "audience" in form:
            return 400, {"error": "invalid_target"}
        with self._lock:
            user = self.api_keys.get(form.get("subject_token", ""))
        if user is None:
            return 400, {"error": "invalid_grant"}
        return 200, {
            "access_token": self.mint_token(user),
            "issued_token_type": ACCESS_TOKEN_TYPE,
            "token_type": "Bearer",
            "expires_in": DEFAULT_TOKEN_LIFETIME_SECONDS,
        }

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

    def _sign(self, claims: dict) -> str:
        header = {"alg": "RS256", "typ": "JWT", "kid": self.kid}
        signing_input = (
            _b64url(json.dumps(header, separators=(",", ":")).encode())
            + "."
            + _b64url(json.dumps(claims, separators=(",", ":")).encode())
        )
        signature = self._private_key.sign(
            signing_input.encode("ascii"), padding.PKCS1v15(), hashes.SHA256()
        )
        return f"{signing_input}.{_b64url(signature)}"

    def mint_token(
        self,
        user: OidcUser,
        lifetime_seconds: int = DEFAULT_TOKEN_LIFETIME_SECONDS,
        resources: list[dict] | None = None,
    ) -> str:
        """An access token for `user`, carrying only the claims a stock
        provider emits: no display name, so the name must come from the ID
        token. With `resources`, a partition-scoped token carrying that
        claim, as an exchange mints."""
        now = int(time.time())
        claims = {
            "iss": self.issuer,
            "sub": user.user_id,
            "aud": self.audience,
            "iat": now,
            "exp": now + lifetime_seconds,
            "client_id": self.client_id,
            "scope": "openid offline_access",
        }
        if user.identity is not None:
            claims["identity"] = user.identity
        if resources is not None:
            claims["resources"] = resources
        token = self._sign(claims)
        with self._lock:
            self._minted.add(token)
        return token

    def mint_id_token(self, user: OidcUser) -> str:
        now = int(time.time())
        return self._sign(
            {
                "iss": self.issuer,
                "sub": user.user_id,
                "aud": self.client_id,
                "iat": now,
                "exp": now + 300,
                "name": user.display_name,
                "preferred_username": user.preferred_username,
            }
        )
