// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Sets up the Lore project, users and clients in a ZITADEL instance, through ZITADEL's
// API. ZITADEL has no import that keeps client IDs and secrets, so it generates the CLI
// client's ID and the CI client's secret, and this writes them where `loreserver` and the
// walkthrough read them: `local.toml`, which `loreserver --config .` loads after the
// configuration `--env` names, and `ci-secret`.

const fs = require("node:fs");
const path = require("node:path");

// The instance's own domain: ZITADEL finds the instance by the request's host.
const API = process.env.ZITADEL_URL || "http://localhost:8080";
const PAT_FILE = process.env.PAT_FILE || "/bootstrap/admin.pat";
const OUT_DIR = process.env.OUT_DIR || "/out";
const PROJECT = "lore-project";
const ACTIONS = ["admin", "push-protected", "obliterate"];
const USERS = [
  { id: "alice", name: "Alice", actions: ["admin"] },
  { id: "bob", name: "Bob", actions: ["obliterate"] },
];
const CI_USER = { id: "lore-ci", actions: ["push-protected"] };

// The access token claims Lore reads. ZITADEL's `aud` holds only project and client IDs, so
// the client finds no host in it to send the token to, and its roles claim is an object keyed
// by role, not a list. The identity is the user ID, which users can't change, unlike their
// user name. The grants are in the context only when the client requests the roles scope.
const CLAIMS_ACTION = `function loreClaims(ctx, api) {
  api.v1.claims.setClaim("root_domains", ["localhost"]);
  api.v1.claims.setClaim("identity", ctx.v1.getUser().id);
  var actions = [];
  var grants = ctx.v1.user.grants;
  ((grants && grants.grants) || []).forEach(function (grant) {
    if (grant.projectId === "${PROJECT}") actions = actions.concat(grant.roles);
  });
  api.v1.claims.setClaim("lore_actions", actions);
}`;
// ZITADEL's ids for the complement token flow and its pre access token creation trigger.
const COMPLEMENT_TOKEN_FLOW = 2;
const PRE_ACCESS_TOKEN_TRIGGER = 5;
const FETCH_TIMEOUT_MS = 10_000;

let pat;

async function call(method, route, body, { allow = [] } = {}) {
  const response = await fetch(`${API}/${route}`, {
    method,
    headers: { Authorization: `Bearer ${pat}`, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
  });
  const text = await response.text();
  if (!response.ok && !allow.includes(response.status)) {
    throw new Error(`${method} /${route} answered HTTP ${response.status}: ${text}`);
  }
  return { status: response.status, body: text ? JSON.parse(text) : {} };
}

/** The setup user's organization, once ZITADEL has written the user's token and accepts it. */
async function connect() {
  for (let attempt = 0; attempt < 60; attempt += 1) {
    let me;
    try {
      pat = fs.readFileSync(PAT_FILE, "utf8").trim();
      // Until ZITADEL has written the token and accepts it, reading it, connecting or the
      // request can fail.
      me = await call("GET", "auth/v1/users/me", undefined, { allow: [401, 404] });
    } catch (error) {
      if (error.code !== "ENOENT" && error.name !== "TimeoutError" && error.message !== "fetch failed") {
        throw error;
      }
    }
    if (me?.status === 200) return me.body.user.details.resourceOwner;
    await new Promise((resolve) => setTimeout(resolve, 2_000));
  }
  throw new Error(`ZITADEL didn't accept the token in ${PAT_FILE}`);
}

/** Creates something unless it exists. True when it was created. */
async function create(route, body) {
  const { status, body: answer } = await call("POST", route, body, { allow: [400, 409] });
  // Some ZITADEL APIs report an existing object as 409, others as 400 with an
  // AlreadyExist* error.
  if (status === 409 || (status === 400 && /AlreadyExist/.test(answer.message))) return false;
  if (status === 400) throw new Error(`POST /${route} answered HTTP 400: ${JSON.stringify(answer)}`);
  return true;
}

/** Updates something. ZITADEL refuses an update that changes nothing, in several wordings. */
async function change(method, route, body) {
  const { status, body: answer } = await call(method, route, body, { allow: [400] });
  if (status === 400 && !/NotChanged|No changes/i.test(answer.message)) {
    throw new Error(`${method} /${route} answered HTTP 400: ${JSON.stringify(answer)}`);
  }
}

/**
 * Creates what's missing, so a run that failed part way is completed by the next. True when
 * the CI user was created, whose secret then has to be generated.
 */
async function createAll(org) {
  await create("zitadel.project.v2.ProjectService/CreateProject", {
    organizationId: org,
    projectId: PROJECT,
    name: "Lore",
  });
  for (const action of ACTIONS) {
    await create(`management/v1/projects/${PROJECT}/roles`, { roleKey: action, displayName: action });
  }
  await create("zitadel.app.v2beta.AppService/CreateApplication", {
    projectId: PROJECT,
    id: "lore-cli",
    name: "lore-cli",
    oidcRequest: {
      appType: "OIDC_APP_TYPE_NATIVE",
      authMethodType: "OIDC_AUTH_METHOD_TYPE_NONE",
      grantTypes: ["OIDC_GRANT_TYPE_DEVICE_CODE", "OIDC_GRANT_TYPE_REFRESH_TOKEN"],
      responseTypes: ["OIDC_RESPONSE_TYPE_CODE"],
      accessTokenType: "OIDC_TOKEN_TYPE_JWT",
    },
  });
  for (const user of USERS) {
    await create("v2/users/human", {
      organization: { orgId: org },
      userId: user.id,
      username: user.id,
      profile: { givenName: user.name, familyName: "Example" },
      email: { email: `${user.id}@example.com`, isVerified: true },
      password: { password: user.id, changeRequired: false },
    });
  }
  const ciCreated = await create("v2/users/new", {
    organizationId: org,
    userId: CI_USER.id,
    username: CI_USER.id,
    machine: { name: "Lore CI" },
  });
  // Machine users get opaque access tokens unless told otherwise.
  await change("PUT", `management/v1/users/${CI_USER.id}/machine`, {
    name: "Lore CI",
    accessTokenType: "ACCESS_TOKEN_TYPE_JWT",
  });
  for (const user of [...USERS, CI_USER]) {
    await create(`management/v1/users/${user.id}/grants`, { projectId: PROJECT, roleKeys: user.actions });
  }
  const { body: found } = await call("POST", "management/v1/actions/_search", {
    queries: [{ actionNameQuery: { name: "loreClaims", method: "TEXT_QUERY_METHOD_EQUALS" } }],
  });
  const action = { name: "loreClaims", script: CLAIMS_ACTION, timeout: "10s", allowedToFail: false };
  let actionId = found.result?.[0]?.id;
  if (actionId) await change("PUT", `management/v1/actions/${actionId}`, action);
  else actionId = (await call("POST", "management/v1/actions", action)).body.id;
  await change("POST", `management/v1/flows/${COMPLEMENT_TOKEN_FLOW}/trigger/${PRE_ACCESS_TOKEN_TRIGGER}`, {
    actionIds: [actionId],
  });
  return ciCreated;
}

/** Writes a file owned by whoever owns OUT_DIR, the user who checked out the repository. */
function writeOutput(name, content, mode) {
  const file = path.join(OUT_DIR, name);
  const { uid, gid } = fs.statSync(OUT_DIR);
  fs.writeFileSync(file, content, { mode });
  fs.chownSync(file, uid, gid);
}

async function writeClientId() {
  const { body } = await call("GET", `management/v1/projects/${PROJECT}/apps/lore-cli`);
  writeOutput(
    "local.toml",
    "# Written by setup.js: the client ID ZITADEL generated for lore-cli.\n" +
      `[server.auth.oidc]\nclient_id = "${body.app.oidcConfig.clientId}"\n`,
    0o644,
  );
}

/** Whether ZITADEL accepts the secret in `ci-secret`, which may be from an earlier instance. */
async function ciSecretWorks() {
  let secret;
  try {
    secret = fs.readFileSync(path.join(OUT_DIR, "ci-secret"), "utf8").trim();
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
  const response = await fetch(`${API}/oauth/v2/token`, {
    method: "POST",
    headers: { Authorization: `Basic ${Buffer.from(`${CI_USER.id}:${secret}`).toString("base64")}` },
    // ZITADEL refuses a token request with no scope.
    body: new URLSearchParams({ grant_type: "client_credentials", scope: "openid" }),
    signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
  });
  return response.ok;
}

async function writeCiSecret() {
  const { body } = await call("PUT", `management/v1/users/${CI_USER.id}/secret`, {});
  writeOutput("ci-secret", `${body.clientSecret}\n`, 0o600);
}

async function main() {
  // A failed setup mustn't leave the client ID of an earlier instance for the server.
  fs.rmSync(path.join(OUT_DIR, "local.toml"), { force: true });
  const org = await connect();
  const ciCreated = await createAll(org);
  await writeClientId();
  // ZITADEL shows a secret only when it generates one, so a new one replaces a lost or
  // stale file.
  if (ciCreated || !(await ciSecretWorks())) await writeCiSecret();
  console.log(`ZITADEL is set up. lore-cli's client ID and lore-ci's secret are in ${OUT_DIR}.`);
}

main().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
