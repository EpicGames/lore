// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// An example of a token exchange service for Lore's Tier 2, beside an OIDC provider that
// handles login. It takes the provider's access token and one RFC 8707 `resource` naming a
// repository, looks the caller up in the grants file, and returns a token for that
// repository signed with its own key.
//
// It isn't meant for real deployments. It shows what a token exchange service has to do for
// Lore, for local development and testing: its signing key is generated at startup and kept
// in memory, its grants come from a JSON file, and it has no administration, audit log,
// high availability or key rotation.

const crypto = require("node:crypto");
const fs = require("node:fs");
const http = require("node:http");

const HOST = process.env.HOST || "127.0.0.1";
const PORT = Number(process.env.PORT || 8190);
const ISSUER = process.env.ISSUER || `http://localhost:${PORT}`;
// Comma-separated, for a provider that gives each client an issuer of its own.
const list = (value) => (value ?? "").split(",").map((entry) => entry.trim()).filter(Boolean);
const LOGIN_ISSUERS = list(process.env.LOGIN_ISSUERS);
// An issuer names the host the client reaches the provider on, which inside Docker isn't
// the host this service reaches it on. Replaces the scheme, host and port of the provider's
// URLs.
const PROVIDER_URL = process.env.PROVIDER_URL;
// The `aud` of the tokens this service issues, which the Lore server accepts.
const AUDIENCE = process.env.AUDIENCE || "https://localhost";
// A login token is accepted when its `aud` names one of these.
const LOGIN_AUDIENCES = list(process.env.LOGIN_AUDIENCES || AUDIENCE);
// Match the Lore server's `[server.auth.oidc] resource_template` and
// `[server.auth] resource_id_template`.
const RESOURCE_PREFIX = process.env.RESOURCE_PREFIX || "https://localhost/partitions/";
const RESOURCE_ID_TEMPLATE = process.env.RESOURCE_ID_TEMPLATE || "urc-{id}";
const GRANTS_FILE = process.env.GRANTS_FILE;

const TOKEN_LIFETIME_SECONDS = 300;
const MAX_BODY_BYTES = 64 * 1024;
const FETCH_TIMEOUT_MS = 5_000;
// Bounds how often a caller sending unknown key ids makes this service fetch keys.
const MIN_REFETCH_MS = 10_000;
// A key the provider stops listing stops verifying tokens within this time, once a fetch
// succeeds.
const KEYS_MAX_AGE_MS = 10 * 60_000;

const TOKEN_EXCHANGE = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE = "urn:ietf:params:oauth:token-type:access_token";
const REPOSITORY_ID = /^[0-9a-f]{32}$/;
const IDENTITY_CLAIMS = ["identity", "https://lore.org/claims/identity"];
const LORE_ACTIONS = new Set([
  "read", "write", "owner", "admin", "migrate", "push-protected", "obliterate", "presign",
]);

for (const [name, value] of [["LOGIN_ISSUERS", LOGIN_ISSUERS.length], ["GRANTS_FILE", GRANTS_FILE]]) {
  if (!value) {
    console.error(`${name} must be set`);
    process.exit(1);
  }
}

const { publicKey, privateKey } = crypto.generateKeyPairSync("rsa", { modulusLength: 2048 });
const KID = crypto.randomUUID();
const PUBLIC_JWK = { ...publicKey.export({ format: "jwk" }), kid: KID, alg: "RS256", use: "sig" };

const base64url = (data) => Buffer.from(data).toString("base64url");
const isObject = (value) => typeof value === "object" && value !== null && !Array.isArray(value);
const log = (entry) => console.log(JSON.stringify(entry));

function sign(claims) {
  // RFC 9068's type, so a server with `jwt_typ = "at+jwt"` accepts these tokens.
  const header = base64url(JSON.stringify({ alg: "RS256", typ: "at+jwt", kid: KID }));
  const payload = base64url(JSON.stringify(claims));
  const signature = crypto.sign("sha256", Buffer.from(`${header}.${payload}`), privateKey);
  return `${header}.${payload}.${base64url(signature)}`;
}

function reachable(url) {
  if (!PROVIDER_URL) return url;
  const target = new URL(url);
  const provider = new URL(PROVIDER_URL);
  target.protocol = provider.protocol;
  target.host = provider.host;
  return target.toString();
}

async function fetchJson(url) {
  const response = await fetch(reachable(url), { signal: AbortSignal.timeout(FETCH_TIMEOUT_MS) });
  if (!response.ok) throw new Error(`${url} answered HTTP ${response.status}`);
  return response.json();
}

/** The signing keys `issuer` lists, by key id. Throws when they can't be fetched. */
async function fetchLoginKeys(issuer) {
  const discovery = await fetchJson(`${issuer.replace(/\/$/, "")}/.well-known/openid-configuration`);
  // A provider that builds its issuer from the request's host, such as authentik, names
  // itself by the PROVIDER_URL host here, so with PROVIDER_URL set only the issuer's path is
  // compared. That can't admit a token with another `iss`: verifySubjectToken matches the
  // token's `iss` against LOGIN_ISSUERS exactly, and keys are cached per issuer.
  if (typeof discovery.issuer !== "string" || reachable(discovery.issuer) !== reachable(issuer)) {
    throw new Error(`the discovery document of ${issuer} names the issuer ${discovery.issuer}`);
  }
  const jwks = await fetchJson(discovery.jwks_uri);
  const keys = new Map();
  for (const jwk of jwks.keys ?? []) {
    if (jwk.kty !== "RSA" || (jwk.use ?? "sig") !== "sig" || typeof jwk.kid !== "string") continue;
    try {
      keys.set(jwk.kid, crypto.createPublicKey({ key: jwk, format: "jwk" }));
    } catch (error) {
      log({ skipped: "unreadable login key", issuer, kid: jwk.kid, error: String(error) });
    }
  }
  return keys;
}

/**
 * Per login issuer: its signing keys by key id, when they were last fetched, a fetch in
 * flight, and whether the last fetch failed.
 */
const loginKeys = new Map();

/** `issuer`'s key named `kid`, or undefined when it lists none. Throws when the keys can't be fetched. */
async function loginKey(issuer, kid) {
  let cached = loginKeys.get(issuer);
  if (!cached) {
    cached = { keys: new Map(), fetchedAt: 0, fetching: null, failed: false };
    loginKeys.set(issuer, cached);
  }
  // A client exchanges for several connections at once, so requests that arrive during a
  // fetch wait for it rather than finding no key. A failed fetch is decided below.
  if (cached.fetching) await cached.fetching.catch(() => {});
  const age = Date.now() - cached.fetchedAt;
  if ((!cached.keys.has(kid) && age > MIN_REFETCH_MS) || age > KEYS_MAX_AGE_MS) {
    cached.fetchedAt = Date.now();
    cached.fetching = fetchLoginKeys(issuer)
      .then((keys) => {
        cached.keys = keys;
        cached.failed = false;
      })
      .catch((error) => {
        cached.failed = true;
        log({ failed: "login key fetch", issuer, error: String(error) });
      })
      .finally(() => {
        cached.fetching = null;
      });
    await cached.fetching;
  }
  if (cached.keys.has(kid)) return cached.keys.get(kid);
  // Until a fetch succeeds, a key missing from the cache is the provider's outage, not a bad
  // login token.
  if (cached.failed) throw new Error(`the keys of ${issuer} couldn't be fetched`);
  return undefined;
}

/** The JSON object a base64url token part holds, or null. */
function decodePart(part) {
  try {
    const value = JSON.parse(Buffer.from(part, "base64url"));
    return isObject(value) ? value : null;
  } catch {
    return null;
  }
}

/**
 * The identity the Lore server reads from `claims`, by the server's rule: the first of the
 * identity claims the token carries, or else `sub`. Null when the claim that decides isn't a
 * non-empty string, which the server refuses.
 */
function loreIdentity(claims) {
  const claim = IDENTITY_CLAIMS.find((name) => name in claims);
  const identity = claim ? claims[claim] : claims.sub;
  return typeof identity === "string" && identity !== "" ? identity : null;
}

/** The claims of a login token, or null when it isn't one this service accepts. */
async function verifySubjectToken(token) {
  const parts = token.split(".");
  if (parts.length !== 3) return null;
  const [header, payload, signature] = parts;
  const head = decodePart(header);
  const claims = decodePart(payload);
  if (!head || !claims || head.alg !== "RS256" || typeof head.kid !== "string") return null;
  // The unverified `iss` chooses the keys to check the signature against, and only an
  // issuer in LOGIN_ISSUERS has keys here.
  if (!LOGIN_ISSUERS.includes(claims.iss)) {
    log({ refused: "issuer", iss: claims.iss });
    return null;
  }
  const key = await loginKey(claims.iss, head.kid);
  if (!key) {
    log({ refused: "unknown login key", kid: head.kid });
    return null;
  }
  const valid = crypto.verify(
    "sha256",
    Buffer.from(`${header}.${payload}`),
    key,
    Buffer.from(signature, "base64url"),
  );
  if (!valid) {
    log({ refused: "bad signature", kid: head.kid });
    return null;
  }
  const audiences = [claims.aud].flat();
  if (!audiences.some((aud) => LOGIN_AUDIENCES.includes(aud))) {
    log({ refused: "audience", aud: claims.aud });
    return null;
  }
  const now = Date.now() / 1000;
  if (typeof claims.exp !== "number" || claims.exp <= now) {
    log({ refused: "expired", exp: claims.exp });
    return null;
  }
  if (typeof claims.nbf === "number" && claims.nbf > now) {
    log({ refused: "not yet valid", nbf: claims.nbf });
    return null;
  }
  if (typeof claims.sub !== "string" || claims.sub === "" || !loreIdentity(claims)) {
    log({ refused: "no identity" });
    return null;
  }
  return claims;
}

/** The actions `user` holds on `repository`: its grants on `*` and on the repository. */
function grantedActions(user, repository) {
  const grants = JSON.parse(fs.readFileSync(GRANTS_FILE, "utf8"));
  const userGrants = isObject(grants) && Object.hasOwn(grants, user) ? grants[user] : {};
  const actions = new Set();
  for (const scope of ["*", repository]) {
    const listed = isObject(userGrants) && Object.hasOwn(userGrants, scope) ? userGrants[scope] : [];
    if (!Array.isArray(listed)) {
      log({ ignored: "grant that isn't a list", user, scope });
      continue;
    }
    for (const action of listed) {
      if (LORE_ACTIONS.has(action)) actions.add(action);
      else log({ ignored: "unknown action", user, scope, action });
    }
  }
  return [...actions];
}

async function exchange(form) {
  if (form.get("grant_type") !== TOKEN_EXCHANGE) {
    return [400, { error: "unsupported_grant_type" }];
  }
  if (form.get("subject_token_type") !== ACCESS_TOKEN_TYPE) {
    return [400, { error: "invalid_request", error_description: "subject_token_type" }];
  }
  const subject = await verifySubjectToken(form.get("subject_token") ?? "");
  if (!subject) {
    return [400, { error: "invalid_grant" }];
  }
  const resources = form.getAll("resource");
  const repository = resources.length === 1 && resources[0].startsWith(RESOURCE_PREFIX)
    ? resources[0].slice(RESOURCE_PREFIX.length)
    : "";
  if (!REPOSITORY_ID.test(repository)) {
    log({ refused: "not one repository resource", resources });
    return [400, { error: "invalid_target", error_description: "one repository resource" }];
  }

  // Grants name the user by the identity the Lore server records, and the exchanged token
  // keeps it, so the caller is the same user to the server in both tokens.
  const identity = loreIdentity(subject);
  const actions = grantedActions(identity, repository);
  log({ identity, repository, actions });
  if (actions.length === 0) {
    return [400, { error: "invalid_target", error_description: "no grant on the repository" }];
  }

  const now = Math.floor(Date.now() / 1000);
  // The Lore server reads `exp` as a whole number of seconds.
  const expires = Math.min(Math.floor(subject.exp), now + TOKEN_LIFETIME_SECONDS);
  const token = sign({
    iss: ISSUER,
    sub: subject.sub,
    identity,
    aud: [AUDIENCE],
    iat: now,
    exp: expires,
    preferred_username: subject.preferred_username,
    resources: [{ resource_id: RESOURCE_ID_TEMPLATE.replace("{id}", repository), permission: actions }],
  });
  return [200, {
    access_token: token,
    issued_token_type: ACCESS_TOKEN_TYPE,
    token_type: "Bearer",
    expires_in: expires - now,
  }];
}

function respond(response, status, body) {
  response.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  response.end(JSON.stringify(body));
}

/** The request body, or null when it's over MAX_BODY_BYTES. */
async function readBody(request) {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.byteLength;
    if (size > MAX_BODY_BYTES) return null;
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}

http
  .createServer(async (request, response) => {
    try {
      if (request.method === "GET" && request.url === "/.well-known/openid-configuration") {
        return respond(response, 200, {
          issuer: ISSUER,
          token_endpoint: `${ISSUER}/token`,
          jwks_uri: `${ISSUER}/jwks`,
          grant_types_supported: [TOKEN_EXCHANGE],
          token_endpoint_auth_methods_supported: ["none"],
        });
      }
      if (request.method === "GET" && request.url === "/jwks") {
        return respond(response, 200, { keys: [PUBLIC_JWK] });
      }
      if (request.method === "POST" && request.url === "/token") {
        const body = await readBody(request);
        if (body === null) return respond(response, 413, { error: "invalid_request" });
        const [status, answer] = await exchange(new URLSearchParams(body));
        return respond(response, status, answer);
      }
      respond(response, 404, { error: "not_found" });
    } catch (error) {
      console.error(error);
      respond(response, 500, { error: "server_error" });
    }
  })
  .listen(PORT, HOST, () =>
    console.log(`Token exchange service ${ISSUER}, login issuers ${LOGIN_ISSUERS.join(", ")}`),
  );
