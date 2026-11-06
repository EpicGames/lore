// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Tests of the example token exchange service. Each test starts server.js as a process
// against a stub OIDC provider on a local port, and talks to it over HTTP, as the Lore client
// does. Run with `node --test` in this directory. CI doesn't run them.

const assert = require("node:assert/strict");
const { spawn } = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const http = require("node:http");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");

const TOKEN_EXCHANGE = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE = "urn:ietf:params:oauth:token-type:access_token";
const AUDIENCE = "https://localhost";
const REPOSITORY = "0123456789abcdef0123456789abcdef";
const OTHER_REPOSITORY = "fedcba9876543210fedcba9876543210";
const RESOURCE = `https://localhost/partitions/${REPOSITORY}`;

const base64url = (data) => Buffer.from(data).toString("base64url");
const decode = (token) => JSON.parse(Buffer.from(token.split(".")[1], "base64url"));
const now = () => Math.floor(Date.now() / 1000);

function rsaKey(kid) {
  const { publicKey, privateKey } = crypto.generateKeyPairSync("rsa", { modulusLength: 2048 });
  return { kid, privateKey, jwk: { ...publicKey.export({ format: "jwk" }), kid, alg: "RS256", use: "sig" } };
}

function sign(key, claims, header = {}) {
  const head = base64url(JSON.stringify({ alg: "RS256", kid: key.kid, ...header }));
  const payload = base64url(JSON.stringify(claims));
  const signature = crypto.sign("sha256", Buffer.from(`${head}.${payload}`), key.privateKey);
  return `${head}.${payload}.${base64url(signature)}`;
}

async function freePort() {
  const server = net.createServer().listen(0, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  const { port } = server.address();
  await new Promise((resolve) => server.close(resolve));
  return port;
}

/**
 * A provider serving a discovery document and a key set. `keys` is the set it serves, `up`
 * whether it answers, `delayMs` how long each answer takes, and `jwksRequests` how often its
 * key set was fetched. `issuer` is the issuer its tokens carry, and `url` where it's reached,
 * which its discovery document names, like a provider that names its issuer by the host a
 * request reaches it on.
 */
async function startProvider(t) {
  const provider = { keys: [rsaKey("k1")], up: true, delayMs: 0, jwksRequests: 0 };
  const server = http.createServer(async (request, response) => {
    await new Promise((resolve) => setTimeout(resolve, provider.delayMs));
    if (!provider.up) {
      response.writeHead(503).end();
      return;
    }
    response.setHeader("content-type", "application/json");
    if (request.url === "/.well-known/openid-configuration") {
      response.end(JSON.stringify({ issuer: provider.url, jwks_uri: `${provider.url}/jwks` }));
    } else if (request.url === "/jwks") {
      provider.jwksRequests += 1;
      response.end(JSON.stringify({ keys: provider.keys.map((key) => key.jwk) }));
    } else {
      response.writeHead(404).end();
    }
  });
  server.listen(0, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  provider.url = `http://127.0.0.1:${server.address().port}`;
  provider.issuer = provider.url;
  t.after(() => new Promise((resolve) => server.close(resolve)));
  return provider;
}

/** A login token from `provider` for `claims`, signed with its first key. */
function loginToken(provider, claims = {}, key = provider.keys[0]) {
  return sign(key, { iss: provider.issuer, sub: "alice", aud: AUDIENCE, exp: now() + 300, ...claims });
}

/** Starts server.js with `env` and the grants `grants`, and stops it after the test. */
async function startService(t, provider, grants, env = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "lore-exchange-test-"));
  const grantsFile = path.join(dir, "grants.json");
  fs.writeFileSync(grantsFile, JSON.stringify(grants));
  const port = await freePort();
  const child = spawn(process.execPath, [path.join(__dirname, "server.js")], {
    env: {
      PATH: process.env.PATH,
      PORT: String(port),
      ISSUER: "http://localhost:8190",
      LOGIN_ISSUERS: provider.issuer,
      GRANTS_FILE: grantsFile,
      ...env,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  t.after(() => {
    child.kill();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  await new Promise((resolve, reject) => {
    child.stdout.on("data", (data) => String(data).includes("Token exchange service") && resolve());
    child.once("exit", (code) => reject(new Error(`server.js exited with ${code}`)));
  });
  return { url: `http://127.0.0.1:${port}`, grantsFile };
}

async function exchange(service, subjectToken, resource = RESOURCE) {
  const form = new URLSearchParams({
    grant_type: TOKEN_EXCHANGE,
    subject_token_type: ACCESS_TOKEN_TYPE,
    subject_token: subjectToken,
    resource,
  });
  const response = await fetch(`${service.url}/token`, { method: "POST", body: form });
  return { status: response.status, body: await response.json() };
}

const aliceWrites = { alice: { "*": ["write"] } };

test("a valid login token gets a repository-scoped token signed with the service's key", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, aliceWrites);

  const { status, body } = await exchange(service, loginToken(provider));
  assert.equal(status, 200);
  assert.equal(body.token_type, "Bearer");
  assert.equal(body.issued_token_type, ACCESS_TOKEN_TYPE);

  const [head] = body.access_token.split(".");
  const header = JSON.parse(Buffer.from(head, "base64url"));
  assert.equal(header.typ, "at+jwt");
  const jwks = await (await fetch(`${service.url}/jwks`)).json();
  const key = crypto.createPublicKey({ key: jwks.keys[0], format: "jwk" });
  const [h, p, s] = body.access_token.split(".");
  assert.ok(crypto.verify("sha256", Buffer.from(`${h}.${p}`), key, Buffer.from(s, "base64url")));

  const claims = decode(body.access_token);
  assert.equal(claims.iss, "http://localhost:8190");
  assert.deepEqual(claims.aud, [AUDIENCE]);
  assert.equal(claims.sub, "alice");
  assert.equal(claims.identity, "alice");
  assert.deepEqual(claims.resources, [{ resource_id: `urc-${REPOSITORY}`, permission: ["write"] }]);
});

test("the discovery document names the service's own endpoints", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, aliceWrites);
  const document = await (await fetch(`${service.url}/.well-known/openid-configuration`)).json();
  assert.equal(document.issuer, "http://localhost:8190");
  assert.equal(document.token_endpoint, "http://localhost:8190/token");
  assert.equal(document.jwks_uri, "http://localhost:8190/jwks");
});

test("a login token the service can't verify is refused with invalid_grant", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, aliceWrites);
  const stranger = rsaKey("k1");
  const valid = loginToken(provider);
  const [h, p] = valid.split(".");
  const cases = {
    "not a JWT": "x",
    "two parts": `${h}.${p}`,
    "four parts": `${valid}.x`,
    "header not JSON": `${base64url("{")}.${p}.c2ln`,
    "claims null": `${h}.${base64url("null")}.c2ln`,
    "another algorithm": loginToken(provider).replace(h, base64url(JSON.stringify({ alg: "HS256", kid: "k1" }))),
    "bad signature": loginToken(provider, {}, stranger),
    "another issuer": loginToken(provider, { iss: "http://127.0.0.1:1" }),
    "another audience": loginToken(provider, { aud: "https://elsewhere.example.com" }),
    expired: loginToken(provider, { exp: now() - 10 }),
    "not yet valid": loginToken(provider, { nbf: now() + 300 }),
    "no sub": loginToken(provider, { sub: "" }),
    "empty identity": loginToken(provider, { identity: "" }),
    "non-string identity": loginToken(provider, { identity: 7 }),
  };
  for (const [name, token] of Object.entries(cases)) {
    const { status, body } = await exchange(service, token);
    assert.equal(status, 400, name);
    assert.equal(body.error, "invalid_grant", name);
  }
});

test("a repository the caller has no grant on is refused with invalid_target", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, { alice: { [OTHER_REPOSITORY]: ["write"] } });
  const token = loginToken(provider);

  assert.equal((await exchange(service, token)).body.error, "invalid_target");
  for (const resource of [`https://localhost/partitions/${REPOSITORY.toUpperCase()}`, "https://localhost/other/x"]) {
    assert.equal((await exchange(service, token, resource)).body.error, "invalid_target", resource);
  }
  assert.equal((await exchange(service, token, `https://localhost/partitions/${OTHER_REPOSITORY}`)).status, 200);
});

test("grants on * and on the repository merge, and unknown actions are ignored", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, {
    alice: { "*": ["write", "wirte"], [REPOSITORY]: ["admin", "write"] },
    bob: { "*": "write" },
  });

  const alice = decode((await exchange(service, loginToken(provider))).body.access_token);
  assert.deepEqual(alice.resources[0].permission.sort(), ["admin", "write"]);
  assert.equal((await exchange(service, loginToken(provider, { sub: "bob" }))).body.error, "invalid_target");
});

test("grants name the caller by the identity the Lore server reads", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, {
    "by-identity": { "*": ["write"] },
    "by-alias": { "*": ["write"] },
    "by-sub": { "*": ["write"] },
  });
  const cases = [
    [{ sub: "by-sub", identity: "by-identity", "https://lore.org/claims/identity": "by-alias" }, "by-identity"],
    [{ sub: "by-sub", "https://lore.org/claims/identity": "by-alias" }, "by-alias"],
    [{ sub: "by-sub" }, "by-sub"],
    [{ sub: "by-sub", email: "by-identity" }, "by-sub"],
  ];
  for (const [claims, identity] of cases) {
    const { status, body } = await exchange(service, loginToken(provider, claims));
    assert.equal(status, 200, identity);
    assert.equal(decode(body.access_token).identity, identity);
  }

  // An email address users may be able to change never names the caller to the grants.
  const byEmail = await exchange(service, loginToken(provider, { sub: "nobody", email: "by-sub" }));
  assert.equal(byEmail.body.error, "invalid_target");
});

test("the issued token expires within five minutes, never past the login token, in whole seconds", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, aliceWrites);

  const long = decode((await exchange(service, loginToken(provider, { exp: now() + 3600 }))).body.access_token);
  assert.ok(long.exp - long.iat <= 300);
  const short = await exchange(service, loginToken(provider, { exp: now() + 100.5 }));
  const claims = decode(short.body.access_token);
  assert.ok(Number.isInteger(claims.exp));
  assert.ok(claims.exp <= now() + 101);
  assert.ok(Number.isInteger(short.body.expires_in));
});

test("requests that arrive during the first key fetch wait for it", async (t) => {
  const provider = await startProvider(t);
  provider.delayMs = 300;
  const service = await startService(t, provider, aliceWrites);

  const results = await Promise.all(Array.from({ length: 5 }, () => exchange(service, loginToken(provider))));
  assert.deepEqual(results.map((result) => result.status), [200, 200, 200, 200, 200]);
  assert.equal(provider.jwksRequests, 1);
});

test("a provider outage is server_error, and a cached key keeps verifying through it", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, aliceWrites);
  const token = loginToken(provider);
  assert.equal((await exchange(service, token)).status, 200);

  provider.up = false;
  assert.equal((await exchange(service, token)).status, 200);

  const down = await startProvider(t);
  down.up = false;
  const unreachable = await startService(t, down, aliceWrites);
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const { status, body } = await exchange(unreachable, loginToken(down));
    assert.equal(status, 500);
    assert.equal(body.error, "server_error");
  }
});

test("a key the provider adds is fetched when a token names it", async (t) => {
  const provider = await startProvider(t);
  const service = await startService(t, provider, aliceWrites);
  assert.equal((await exchange(service, loginToken(provider))).status, 200);

  const rotated = rsaKey("k2");
  provider.keys.push(rotated);
  // Unknown key ids are refetched at most every ten seconds, which the first exchange started.
  await new Promise((resolve) => setTimeout(resolve, 10_500));
  assert.equal((await exchange(service, loginToken(provider, {}, rotated))).status, 200);
});

test("an issuer is compared through PROVIDER_URL, and the token's iss still has to match", async (t) => {
  const provider = await startProvider(t);
  const port = new URL(provider.url).port;
  provider.issuer = `http://localhost:${port}`;
  const service = await startService(t, provider, aliceWrites, { PROVIDER_URL: provider.url });

  assert.equal((await exchange(service, loginToken(provider))).status, 200);
  const other = loginToken(provider, { iss: provider.url });
  assert.equal((await exchange(service, other)).body.error, "invalid_grant");
});

test("a discovery document naming another issuer is refused without PROVIDER_URL", async (t) => {
  const provider = await startProvider(t);
  provider.issuer = `http://localhost:${new URL(provider.url).port}`;
  const service = await startService(t, provider, aliceWrites);
  const { status, body } = await exchange(service, loginToken(provider));
  assert.equal(status, 500);
  assert.equal(body.error, "server_error");
});

test("the service doesn't start without its required settings", async () => {
  for (const missing of ["LOGIN_ISSUERS", "GRANTS_FILE"]) {
    const env = { PATH: process.env.PATH, PORT: String(await freePort()), LOGIN_ISSUERS: "http://127.0.0.1:1", GRANTS_FILE: "/dev/null" };
    delete env[missing];
    const child = spawn(process.execPath, [path.join(__dirname, "server.js")], { env, stdio: "ignore" });
    const code = await new Promise((resolve) => child.once("exit", resolve));
    assert.equal(code, 1, missing);
  }
});
