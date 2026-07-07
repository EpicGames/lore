// lore-web HTTP server. Built on Node's stdlib http only (no web framework) so
// the whole tool runs on any machine with Node + the vendored SDK, no build and
// no extra native deps. Bound to 127.0.0.1: it exposes full repo write access
// and must never be reachable off-host.

import { createServer } from "node:http";
import { readFile, stat, readdir } from "node:fs/promises";
import { existsSync, readFileSync, rmSync, renameSync } from "node:fs";
import { join, extname, normalize as normalizePath, dirname, parse as parsePath, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";

import { log } from "./log.mjs";
import { collect, stream, configureSdk, shutdownSdk } from "./sdk.mjs";
import * as store from "./store.mjs";
import * as xform from "./transforms.mjs";
import { addClient, broadcastRefresh } from "./events.mjs";
import { watchRepo, unwatchRepo } from "./watcher.mjs";
import { isLoggedIn, runCli } from "./cli.mjs";
import { setupLoreignore, appendIgnorePattern, hasLoreignore, hasGitignore, hasP4ignore } from "./loreignore.mjs";
import { discoverServers } from "./discovery.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB_DIR = join(HERE, "..", "web");
const HOST = process.env.LORE_WEB_HOST ?? "127.0.0.1";
const PORT = Number(process.env.LORE_WEB_PORT ?? process.env.PORT ?? 7420);

/** A path is a Lore working copy if it holds a .lore (or legacy .urc) dir. */
function isRepo(path) {
  return existsSync(join(path, ".lore")) || existsSync(join(path, ".urc"));
}

/**
 * The remote server base to assign when initializing a brand-new repository, so
 * the user never types one. Reads from configured default remote, falls back to
 * an already-tracked repo's remote, or the default local Lore server. The repo
 * name is appended later; repositoryCreate mints the per-repo UUID that
 * distinguishes repos on a server.
 * @returns {string} a remote base URL with no trailing repo-name path component
 */
function defaultRemoteBase() {
  const configured = store.getDefaultRemote();
  if (configured) return configured;

  for (const r of store.listRepos()) {
    for (const name of ["config.toml", "config"]) {
      const cfg = join(r.path, ".lore", name);
      if (!existsSync(cfg)) continue;
      try {
        const m = readFileSync(cfg, "utf8").match(/^\s*remote_url\s*=\s*"([^"]+)"/m);
        if (m && m[1]) return m[1];
      } catch {
        // unreadable config — keep looking
      }
    }
  }
  return "lore://127.0.0.1:41337";
}

/** The remote_url recorded in a repo's .lore config, or null if none/unreadable. */
function readRepoRemote(repoPath) {
  for (const name of ["config.toml", "config"]) {
    const cfg = join(repoPath, ".lore", name);
    if (!existsSync(cfg)) continue;
    try {
      const m = readFileSync(cfg, "utf8").match(/^\s*remote_url\s*=\s*"([^"]+)"/m);
      if (m && m[1]) return m[1];
    } catch {
      // unreadable — fall through
    }
  }
  return null;
}

/** The scheme://authority prefix of a URL, with any path/repo component dropped. */
function remoteBase(url) {
  const m = url.match(/^([a-z][a-z0-9+.-]*:\/\/[^/]+)/i);
  return m ? m[1] : url.replace(/\/+$/, "");
}

/**
 * The repository URL suggested when initializing a folder named `label`, of the
 * form <server-base>/<label>. This is what the Add flow shows for review.
 * @param {string} label the repo name (usually the folder's last path segment)
 * @returns {string} a full repository URL
 */
function suggestInitUrl(label) {
  return `${defaultRemoteBase().replace(/\/+$/, "")}/${label}`;
}

/**
 * Forward-slash form of a path — the native lib drops Windows backslashes.
 * @param {string} p a filesystem path, possibly using backslash separators
 * @returns {string} the same path with every backslash replaced by a slash
 */
function toUnixPath(p) {
  return p.replace(/\\/g, "/");
}

function sendJson(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(text);
}

/** Translate a thrown error into a typed JSON error response (never crash). */
function sendError(res, err) {
  const message = err instanceof Error ? err.message : String(err);
  const status = err && typeof err === "object" && "httpStatus" in err ? err.httpStatus : 500;
  log.warn("request failed", { message });
  sendJson(res, status, { error: message });
}

async function readBody(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  if (chunks.length === 0) return {};
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    return {};
  }
}

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
  ".json": "application/json",
  ".ico": "image/x-icon",
};

/** Serve a static asset from web/, defaulting to index.html (SPA fallback). */
async function serveStatic(req, res, pathname) {
  let rel = pathname === "/" ? "/index.html" : pathname;
  // Contain the path within WEB_DIR.
  const filePath = join(WEB_DIR, normalizePath(rel).replace(/^(\.\.[/\\])+/, ""));
  if (!filePath.startsWith(WEB_DIR)) return sendJson(res, 403, { error: "forbidden" });
  try {
    const info = await stat(filePath);
    if (info.isDirectory()) throw new Error("dir");
    const body = await readFile(filePath);
    res.writeHead(200, { "Content-Type": MIME[extname(filePath)] ?? "application/octet-stream" });
    res.end(body);
  } catch {
    // SPA fallback for unknown non-API paths.
    try {
      const index = await readFile(join(WEB_DIR, "index.html"));
      res.writeHead(200, { "Content-Type": MIME[".html"] });
      res.end(index);
    } catch {
      sendJson(res, 404, { error: "not found" });
    }
  }
}

/** GET /api/repos — tracked repos, each enriched with live branch/exists. */
async function listRepos(res) {
  const repos = store.listRepos();
  const enriched = await Promise.all(
    repos.map(async (r) => {
      const exists = isRepo(r.path);
      let info = {};
      let organization = "";
      if (exists) {
        try {
          info = xform.repoSummary(await collect("repositoryStatus", { repositoryPath: r.path }, { staged: false }));
        } catch (err) {
          log.debug("repo enrich failed", { path: r.path, message: err instanceof Error ? err.message : String(err) });
        }
        try {
          organization = (await readOrg(r.path)).organization;
        } catch (err) {
          log.debug("repo org read failed", { path: r.path, message: err instanceof Error ? err.message : String(err) });
        }
      }
      return { ...r, exists, organization, ...info };
    }),
  );
  sendJson(res, 200, { repos: enriched });
}

/**
 * Read a repository's organization, parsed from its `name` metadata. Lore stores
 * the name as an `org/repo` value; the prefix before the first slash is the
 * organization. Reads local metadata only (the working copy), matching what the
 * desktop client surfaces.
 * @param {string} repoPath path to a Lore working copy
 * @returns {Promise<{ organization: string, repoName: string, name: string }>}
 */
async function readOrg(repoPath) {
  const events = await collect("repositoryMetadataGet", { repositoryPath: repoPath, local: true }, { key: "name" });
  return xform.splitOrg(xform.metadata(events).name);
}

/**
 * GET /api/org — the organization and repository name for a tracked repo.
 * @param {import("node:http").ServerResponse} res
 * @param {string|null} repoPath the `path` query parameter
 */
async function getOrg(res, repoPath) {
  if (!repoPath) return sendJson(res, 400, { error: "path required" });
  return sendJson(res, 200, await readOrg(repoPath));
}

/**
 * POST /api/org — change a repository's organization. A repo's org is the `org/`
 * prefix of its `name` metadata, which Lore makes read-only after creation: it is
 * set from the path of the create URL and cannot be edited via metadata. The only
 * way to change it in place is to recreate the working copy's `.lore` under a new
 * URL (preserving the repository id), which discards local committed history. The
 * caller must therefore confirm the destructive nature first; this endpoint
 * performs the recreate unconditionally.
 *
 * The body is `{ path, organization }`. An organization cannot be empty or contain
 * a slash, since the slash separates it from the repository name.
 * @param {import("node:http").IncomingMessage} req
 * @param {import("node:http").ServerResponse} res
 */
async function setOrg(req, res) {
  const body = await readBody(req);
  const path = typeof body.path === "string" ? toUnixPath(body.path) : "";
  if (!path) return sendJson(res, 400, { error: "path required" });
  if (!existsSync(path)) return sendJson(res, 400, { error: "path does not exist" });
  if (!isRepo(path)) return sendJson(res, 400, { error: "not a Lore repository" });
  const organization = typeof body.organization === "string" ? body.organization.trim() : "";
  if (!organization) return sendJson(res, 400, { error: "organization required" });
  if (organization.includes("/")) return sendJson(res, 400, { error: "organization cannot contain '/'" });
  const current = await readOrg(path);
  const repoName = current.repoName || path.split(/[\\/]/).filter(Boolean).pop() || path;
  const remote = readRepoRemote(path);
  const base = remote ? remoteBase(remote) : defaultRemoteBase().replace(/\/+$/, "");
  const repositoryUrl = `${base}/${organization}/${repoName}`;
  log.info("changing organization", { path, from: current.organization, to: organization });
  const id = await recreateLore(path, repositoryUrl);
  broadcastRefresh("*", "repos");
  return sendJson(res, 200, { ...xform.splitOrg(`${organization}/${repoName}`), id });
}

/**
 * POST /api/repos — start tracking a folder, smartly. If the folder is already a
 * Lore working copy it is tracked as-is; otherwise a new repository is initialized
 * there first (with an auto-generated remote URL) so the user can point at any
 * folder without caring whether it has been set up yet.
 */
async function addRepo(req, res) {
  let { path, url } = await readBody(req);
  if (!path || typeof path !== "string") return sendJson(res, 400, { error: "path required" });
  // The native lib mangles backslash paths; forward slashes are the store's
  // convention and what every other verb here is given.
  path = toUnixPath(path);
  if (!existsSync(path)) return sendJson(res, 400, { error: "path does not exist" });
  const label = path.split(/[\\/]/).filter(Boolean).pop() || path;
  let initialized = false;
  if (!isRepo(path)) {
    // Use the caller's reviewed URL when given, else the generated suggestion.
    // A bare host is rejected as invalid, so the name is part of the suggestion.
    const repositoryUrl = (typeof url === "string" && url.trim()) || suggestInitUrl(label);
    log.info("initializing repository", { path, repositoryUrl });
    await collect("repositoryCreate", { repositoryPath: path }, { repositoryUrl, id: "" });
    // Seed .loreignore (from .gitignore when present) and keep each tool's
    // metadata out of the other's history.
    setupLoreignore(path);
    initialized = true;
  }
  const entry = store.addRepo(path, label);
  watchRepo(path, () => broadcastRefresh(path, "fs"));
  broadcastRefresh("*", "repos");
  sendJson(res, 200, { repo: entry, initialized });
}

/**
 * POST /api/repair — rebuild a working copy's .lore in place. Lore can leave
 * "zombie" status entries (for example, a nested repo that was indexed then deleted) that
 * no reset/stage/commit/obliterate can remove; the only cure is recreating .lore.
 * We do that while preserving the repository id and remote, so the repo keeps its
 * identity. Refused when there is committed history (which a rebuild would drop) —
 * such a repo should be re-cloned from its remote instead.
 */
async function repairRepo(path, res) {
  if (!existsSync(path)) return sendJson(res, 400, { error: "path does not exist" });
  if (!isRepo(path)) return sendJson(res, 400, { error: "not a Lore repository" });
  // Guard: never destroy committed history.
  const hist = xform.history(await collect("revisionHistory", { repositoryPath: path }, { length: 1 }));
  if (hist.length > 0) {
    return sendJson(res, 409, {
      error: "repository has committed revisions; repair would lose them — re-clone from the remote instead",
    });
  }
  const remote = readRepoRemote(path);
  const label = path.split(/[\\/]/).filter(Boolean).pop() || path;
  const repositoryUrl = `${remote ? remoteBase(remote) : defaultRemoteBase().replace(/\/+$/, "")}/${label}`;
  const id = await recreateLore(path, repositoryUrl);
  broadcastRefresh(path, "repair");
  sendJson(res, 200, { ok: true, id });
}

/**
 * Rebuild a working copy's `.lore` in place, re-registering it under
 * `repositoryUrl` while preserving its existing repository id. The old `.lore` is
 * moved aside and restored if the rebuild throws, so a failure never leaves the
 * folder without a repository. The rebuild runs offline so it never re-registers
 * on (or conflicts with) the remote — the repo already exists there.
 *
 * This discards any local committed history (a fresh `.lore` has none), so callers
 * must guard against or warn about that before invoking it.
 * @param {string} path a Lore working copy
 * @param {string} repositoryUrl the URL whose path component becomes the repo name
 * @returns {Promise<string>} the preserved repository id (hex), or "" if none existed
 */
async function recreateLore(path, repositoryUrl) {
  const dot = join(path, ".lore");
  let id = "";
  try {
    id = readFileSync(join(dot, "id")).toString("hex");
  } catch {
    // no id file — let create mint a fresh one
  }
  log.info("recreating repository .lore", { path, repositoryUrl, id });
  const backup = `${dot}.repair-bak`;
  rmSync(backup, { recursive: true, force: true });
  renameSync(dot, backup);
  try {
    await collect("repositoryCreate", { repositoryPath: path, offline: true }, { repositoryUrl, id });
  } catch (err) {
    rmSync(dot, { recursive: true, force: true });
    renameSync(backup, dot);
    throw err;
  }
  rmSync(backup, { recursive: true, force: true });
  setupLoreignore(path);
  return id;
}

/**
 * GET /api/remote-repos?url= — ask a Lore server which repositories it hosts, so
 * the user can pick one to clone instead of typing its full URL. The server base
 * comes from the query, falling back to the same default the Add flow suggests
 * (the remote of an already-tracked repo, an env override, or the local server).
 * Each entry is returned with a ready-to-clone URL of the form <base>/<name>.
 * @param {import("node:http").ServerResponse} res
 * @param {string|null} rawUrl the server URL to query; empty/null uses the default
 */
async function listRemoteRepos(res, rawUrl) {
  const base = remoteBase((rawUrl || "").trim() || defaultRemoteBase());
  const events = await collect("repositoryList", {}, { url: base });
  const local = localRepoIds();
  // Address each repo by its id (server-listed names do not resolve for repos
  // created without an owner — see deleteRemoteRepo) but offer the name-based
  // URL for cloning, which the user expects to look like the real remote.
  const repos = xform.remoteRepos(events).map((r) => ({
    ...r,
    url: `${base}/${r.name}`,
    idUrl: `${base}/${r.id}`,
    tracked: local.has(r.id),
  }));
  sendJson(res, 200, { base, repos });
}

/** Repository ids of every tracked local working copy (from each .lore/id). */
function localRepoIds() {
  const ids = new Set();
  for (const r of store.listRepos()) {
    try {
      const id = readFileSync(join(r.path, ".lore", "id")).toString("hex");
      if (id) ids.add(id);
    } catch {
      // no id file / not a working copy — nothing to match
    }
  }
  return ids;
}

/**
 * DELETE /api/remote-repos — remove a repository from its server by id. Two Lore
 * quirks force the shape of this: (1) repos created without an owner do not
 * resolve by their listed name, only by id; (2) `lore repository delete` prints
 * a spurious "Not found" and exits 0 even when it succeeded. So we delete by id
 * and ignore the CLI's status entirely, confirming the outcome by re-listing —
 * the delete worked iff the id is gone.
 */
async function deleteRemoteRepo(req, res) {
  const { id, base: rawBase } = await readBody(req);
  if (!id) return sendJson(res, 400, { error: "id required" });
  const base = remoteBase((rawBase || "").trim() || defaultRemoteBase());
  log.info("deleting remote repository", { base, id });
  await runCli(["repository", "delete", `${base}/${id}`]);
  const events = await collect("repositoryList", {}, { url: base });
  const stillThere = xform.remoteRepos(events).some((r) => r.id === id);
  if (stillThere) return sendJson(res, 500, { error: "server did not delete the repository" });
  sendJson(res, 200, { ok: true });
}

/**
 * GET /api/config — return the configured default remote server and discovered servers.
 * @param {import("node:http").ServerResponse} res
 */
async function getConfig(res) {
  let discovered = [];
  try {
    discovered = await discoverServers();
  } catch (err) {
    log.debug("server discovery failed", { message: err instanceof Error ? err.message : String(err) });
  }
  return sendJson(res, 200, {
    defaultRemote: store.getDefaultRemote(),
    discoveredServers: discovered,
  });
}

/**
 * POST /api/config — set the default remote server URL.
 * @param {import("node:http").IncomingMessage} req
 * @param {import("node:http").ServerResponse} res
 */
async function setConfig(req, res) {
  const body = await readBody(req);
  const defaultRemote = typeof body.defaultRemote === "string" ? body.defaultRemote.trim() : "";
  try {
    store.setDefaultRemote(defaultRemote);
    log.info("remote server configured", { url: defaultRemote ? "[configured]" : "[cleared]" });
    return sendJson(res, 200, { ok: true });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return sendJson(res, 400, { error: message });
  }
}

/**
 * GET /api/discover — manually trigger discovery of Lore servers on the local network.
 * @param {import("node:http").ServerResponse} res
 */
async function manualDiscover(res) {
  try {
    const discovered = await discoverServers();
    return sendJson(res, 200, { discoveredServers: discovered });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return sendJson(res, 500, { error: message });
  }
}

/**
 * Drive roots present on this machine, used as the picker's top "This PC" level.
 * @returns {string[]} drive-root paths (Windows: C:\ … Z:\; POSIX: "/")
 */
function listDrives() {
  if (process.platform !== "win32") return ["/"];
  const drives = [];
  for (let c = 67; c <= 90; c++) {
    const d = `${String.fromCharCode(c)}:\\`;
    if (existsSync(d)) drives.push(d);
  }
  return drives;
}

/**
 * GET /api/browse?path= — list the sub-folders of a directory so the UI can offer
 * a native-feeling folder picker (the browser can't hand us a real fs path). An
 * empty path returns the drive roots ("This PC"). Each entry is flagged when it
 * is itself a Lore repo. Only directories are returned — this is a folder picker.
 * @param {import("node:http").ServerResponse} res
 * @param {string|null} rawPath directory to list; empty/null lists the roots
 */
async function browse(res, rawPath) {
  let path = (rawPath || "").trim();
  // Empty path → the roots level (drives on Windows, "/" on POSIX).
  if (!path) {
    const entries = listDrives().map((d) => ({ name: d, path: d, isRepo: isRepo(d) }));
    return sendJson(res, 200, { path: "", parent: null, sep, entries });
  }
  const norm = normalizePath(path);
  let info;
  try {
    info = await stat(norm);
  } catch {
    return sendJson(res, 400, { error: "path does not exist" });
  }
  if (!info.isDirectory()) return sendJson(res, 400, { error: "not a directory" });
  // Parent: the drives/roots level when we're at a drive root, else dirname.
  const atRoot = parsePath(norm).root === norm || norm === "/";
  const parent = atRoot ? "" : dirname(norm);
  let entries = [];
  try {
    const dirents = await readdir(norm, { withFileTypes: true });
    entries = dirents
      .filter((d) => {
        try {
          return d.isDirectory();
        } catch {
          return false;
        }
      })
      .filter((d) => !d.name.startsWith("."))
      .map((d) => {
        const full = join(norm, d.name);
        return { name: d.name, path: full, isRepo: isRepo(full) };
      })
      .sort((a, b) => a.name.localeCompare(b.name));
  } catch (err) {
    return sendJson(res, 400, { error: err instanceof Error ? err.message : "cannot read directory" });
  }
  sendJson(res, 200, { path: norm, parent, sep, isRepo: isRepo(norm), entries });
}

/**
 * DELETE /api/repos — stop tracking a repo. Always succeeds, even if the folder
 * is gone (issue #4: the desktop refused to remove a repo with a missing folder).
 */
async function deleteRepo(req, res) {
  const { path } = await readBody(req);
  if (!path) return sendJson(res, 400, { error: "path required" });
  unwatchRepo(path);
  const removed = store.removeRepo(path);
  broadcastRefresh("*", "repos");
  sendJson(res, 200, { removed });
}

/** Resolve repo-relative file paths to absolute (the native lib uses cwd). */
function absFiles(repoPath, files) {
  if (!Array.isArray(files)) return undefined;
  return files.map((f) => (repoPath ? join(repoPath, f) : f));
}

/**
 * Run a streaming verb and pipe its events to the client as newline-delimited
 * JSON (one normalized event per line). Used for long operations (sync, push,
 * clone) so the browser can render live progress. Ends with the DONE marker.
 * @param {import("node:http").ServerResponse} res
 * @param {string} verb
 * @param {Record<string, unknown>} globalArgs
 * @param {Record<string, unknown>} args
 * @param {string|null} repoPath repo to refresh on completion
 */
async function streamOp(res, verb, globalArgs, args, repoPath) {
  res.writeHead(200, { "Content-Type": "application/x-ndjson", "Cache-Control": "no-cache" });
  let ok = false;
  for await (const ev of stream(verb, globalArgs, args)) {
    if (ev.tag === "DONE") ok = ev.data?.ok;
    res.write(JSON.stringify(ev) + "\n");
  }
  res.end();
  // A mutating op changes repo state; tell every client to refetch.
  if (repoPath) broadcastRefresh(repoPath, verb);
  log.info("stream op finished", { verb, ok });
}

const server = createServer(async (req, res) => {
  try {
    const url = new URL(req.url, `http://${req.headers.host}`);
    const p = url.pathname;
    const q = url.searchParams;
    const repoPath = q.get("path");
    const globalArgs = repoPath ? { repositoryPath: repoPath } : {};

    if (p === "/events" && req.method === "GET") return addClient(res);

    if (p === "/api/auth" && req.method === "GET") {
      return sendJson(res, 200, { loggedIn: await isLoggedIn() });
    }

    if (p === "/api/browse" && req.method === "GET") return await browse(res, q.get("path"));

    // Pre-flight for the Add flow: report whether a folder is already a repo and,
    // if not, the URL it would be initialized with (editable before confirming).
    if (p === "/api/init-url" && req.method === "GET") {
      const target = toUnixPath(q.get("path") || "");
      if (!target || !existsSync(target)) return sendJson(res, 400, { error: "path does not exist" });
      const already = isRepo(target);
      const label = target.split(/[\\/]/).filter(Boolean).pop() || target;
      return sendJson(res, 200, { isRepo: already, url: already ? null : suggestInitUrl(label) });
    }

    if (p === "/api/remote-repos" && req.method === "GET") return await listRemoteRepos(res, q.get("url"));
    if (p === "/api/remote-repos" && req.method === "DELETE") return await deleteRemoteRepo(req, res);

    if (p === "/api/repos" && req.method === "GET") return await listRepos(res);
    if (p === "/api/repos" && req.method === "POST") return await addRepo(req, res);
    if (p === "/api/repos" && req.method === "DELETE") return await deleteRepo(req, res);

    if (p === "/api/org" && req.method === "GET") return await getOrg(res, repoPath);
    if (p === "/api/org" && req.method === "POST") return await setOrg(req, res);

    if (p === "/api/config" && req.method === "GET") return await getConfig(res);
    if (p === "/api/config" && req.method === "POST") return await setConfig(req, res);
    if (p === "/api/discover" && req.method === "GET") return await manualDiscover(res);

    if (p === "/api/history" && req.method === "GET") {
      const length = Number(q.get("length") ?? 50);
      const events = await collect("revisionHistory", globalArgs, { length });
      return sendJson(res, 200, { revisions: xform.history(events) });
    }
    if (p === "/api/status" && req.method === "GET") {
      const events = await collect("repositoryStatus", globalArgs, { staged: true, scan: true });
      const out = xform.status(events);
      // The UI offers an "Initialize .loreignore" action when one is absent.
      out.hasLoreignore = repoPath ? hasLoreignore(repoPath) : false;
      out.hasGitignore = repoPath ? hasGitignore(repoPath) : false;
      out.hasP4ignore = repoPath ? hasP4ignore(repoPath) : false;
      // Flag entries that are themselves Lore working copies (a directory holding
      // its own .lore). The UI prompts to ignore these *while they still exist* —
      // the only way to avoid the unremovable "zombie" entry Lore leaves behind if
      // an indexed nested repo is later deleted.
      if (repoPath) {
        for (const f of out.files) {
          if (f.type === 0 && isRepo(join(repoPath, f.path))) f.nested = true;
        }
      }
      return sendJson(res, 200, out);
    }
    if (p === "/api/branches" && req.method === "GET") {
      const events = await collect("branchList", globalArgs, {});
      return sendJson(res, 200, { branches: xform.branches(events) });
    }
    if (p === "/api/diff" && req.method === "GET") {
      const file = q.get("file");
      // The native lib resolves relative path args against process.cwd(); anchor
      // them to the repo by passing an absolute path instead.
      const abs = file && repoPath ? join(repoPath, file) : file;
      const args = abs ? { paths: [abs] } : {};
      // Optional revision range: diff a file between two revisions instead of the
      // working tree (used to show what a historical revision changed).
      const source = q.get("source");
      const target = q.get("target");
      if (source) args.sourceRevision = source;
      if (target) args.targetRevision = target;
      const events = await collect("fileDiff", globalArgs, args);
      return sendJson(res, 200, { diff: xform.diff(events) });
    }
    if (p === "/api/revision" && req.method === "GET") {
      const revision = q.get("revision");
      const events = await collect("revisionInfo", globalArgs, { revision, delta: true });
      return sendJson(res, 200, { files: xform.revisionFiles(events) });
    }

    // Quick mutating actions answer immediately and broadcast a refresh so every
    // client refetches; the response body itself carries no refreshed state.
    if (p === "/api/stage" && req.method === "POST") {
      const { path: rp, files } = await readBody(req);
      await collect("fileStage", { repositoryPath: rp }, { paths: absFiles(rp, files), scan: true });
      broadcastRefresh(rp, "stage");
      return sendJson(res, 200, { ok: true });
    }
    if (p === "/api/unstage" && req.method === "POST") {
      const { path: rp, files } = await readBody(req);
      await collect("fileUnstage", { repositoryPath: rp }, { paths: absFiles(rp, files) });
      broadcastRefresh(rp, "unstage");
      return sendJson(res, 200, { ok: true });
    }
    if (p === "/api/reset" && req.method === "POST") {
      const { path: rp, files } = await readBody(req);
      // purge is required to discard newly added (untracked) files/folders — without
      // it, fileReset only reverts already-tracked modified content and silently
      // leaves added entries dirty.
      await collect("fileReset", { repositoryPath: rp }, { paths: absFiles(rp, files), purge: true });
      broadcastRefresh(rp, "reset");
      return sendJson(res, 200, { ok: true });
    }
    // Add a file/folder/extension pattern to .loreignore (created if absent).
    if (p === "/api/ignore" && req.method === "POST") {
      const { path: rp, pattern } = await readBody(req);
      if (!rp || !pattern) return sendJson(res, 400, { error: "path and pattern required" });
      const added = appendIgnorePattern(toUnixPath(rp), pattern);
      broadcastRefresh(rp, "ignore");
      return sendJson(res, 200, { ok: true, added });
    }
    // Seed/repair .loreignore for an already-initialized repo (the same setup the
    // Add flow runs on init).
    if (p === "/api/init-loreignore" && req.method === "POST") {
      const { path: rp } = await readBody(req);
      if (!rp) return sendJson(res, 400, { error: "path required" });
      const result = setupLoreignore(toUnixPath(rp));
      broadcastRefresh(rp, "ignore");
      return sendJson(res, 200, { ok: true, ...result });
    }
    // Rebuild a repo's .lore in place to purge unremovable zombie index entries
    // (Lore has no command to drop them). Guarded: refuses if there is committed
    // history to lose. Preserves the repository id and remote so identity is kept.
    if (p === "/api/repair" && req.method === "POST") {
      const { path: rp } = await readBody(req);
      if (!rp) return sendJson(res, 400, { error: "path required" });
      return await repairRepo(toUnixPath(rp), res);
    }
    if (p === "/api/commit" && req.method === "POST") {
      const { path: rp, message } = await readBody(req);
      if (!message) return sendJson(res, 400, { error: "commit message required" });
      return await streamOp(res, "revisionCommit", { repositoryPath: rp }, { message }, rp);
    }

    // Remote operations stream their progress back as NDJSON.
    if (p === "/api/sync" && req.method === "POST") {
      const { path: rp, revision, reset } = await readBody(req);
      return await streamOp(res, "revisionSync", { repositoryPath: rp }, { revision, reset: !!reset }, rp);
    }
    if (p === "/api/push" && req.method === "POST") {
      const { path: rp, branch, fastForwardMerge } = await readBody(req);
      return await streamOp(res, "branchPush", { repositoryPath: rp }, { branch, fastForwardMerge: !!fastForwardMerge }, rp);
    }
    if (p === "/api/clone" && req.method === "POST") {
      const { url, dest } = await readBody(req);
      if (!url || !dest) return sendJson(res, 400, { error: "url and dest required" });
      return await streamOp(res, "repositoryClone", { repositoryPath: toUnixPath(dest) }, { repositoryUrl: url }, null);
    }

    // Anything else is a static asset request, falling back to the SPA shell.
    if (req.method === "GET") return await serveStatic(req, res, p);
    sendJson(res, 404, { error: "not found" });
  } catch (err) {
    sendError(res, err);
  }
});

// On startup, begin watching every already-tracked repo so refresh works before
// the user touches anything.
function startWatchers() {
  for (const r of store.listRepos()) {
    if (isRepo(r.path)) watchRepo(r.path, () => broadcastRefresh(r.path, "fs"));
  }
}

server.on("error", (err) => {
  if (err && err.code === "EADDRINUSE") {
    log.error("port already in use — is lore-web already running?", { host: HOST, port: PORT });
    process.exit(1);
  }
  log.error("server error", { error: err instanceof Error ? err.message : String(err) });
  process.exit(1);
});

configureSdk();
startWatchers();
server.listen(PORT, HOST, () => {
  log.info("lore-web listening", { url: `http://${HOST}:${PORT}` });
});

function shutdown() {
  log.info("shutting down");
  server.close();
  shutdownSdk();
  process.exit(0);
}
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

export { server, stream };
