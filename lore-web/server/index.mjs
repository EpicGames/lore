// lore-web HTTP server. Built on Node's stdlib http only (no web framework) so
// the whole tool runs on any machine with Node + the vendored SDK, no build and
// no extra native deps. Bound to 127.0.0.1: it exposes full repo write access
// and must never be reachable off-host.

import { createServer } from "node:http";
import { readFile, stat, readdir } from "node:fs/promises";
import { existsSync, readFileSync } from "node:fs";
import { join, extname, normalize as normalizePath, dirname, parse as parsePath, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";

import { log } from "./log.mjs";
import { collect, stream, configureSdk, shutdownSdk } from "./sdk.mjs";
import * as store from "./store.mjs";
import * as xform from "./transforms.mjs";
import { addClient, broadcastRefresh } from "./events.mjs";
import { watchRepo, unwatchRepo } from "./watcher.mjs";
import { isLoggedIn } from "./cli.mjs";

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
 * the user never types one. We reuse the remote of an already-tracked repo
 * (almost always the same self-hosted server), falling back to an env override
 * and finally the default local Lore server. The repo name is appended later;
 * repositoryCreate mints the per-repo UUID that distinguishes repos on a server.
 * @returns {string} a remote base URL with no trailing repo-name path component
 */
function defaultRemoteBase() {
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
  return process.env.LORE_WEB_DEFAULT_REMOTE ?? "lore://127.0.0.1:41337";
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
      if (exists) {
        try {
          info = xform.repoSummary(await collect("repositoryStatus", { repositoryPath: r.path }, { staged: false }));
        } catch (err) {
          log.debug("repo enrich failed", { path: r.path });
        }
      }
      return { ...r, exists, ...info };
    }),
  );
  sendJson(res, 200, { repos: enriched });
}

/**
 * POST /api/repos — start tracking a folder, smartly. If the folder is already a
 * Lore working copy it is just tracked; otherwise a new repository is initialized
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
    initialized = true;
  }
  const entry = store.addRepo(path, label);
  watchRepo(path, () => broadcastRefresh(path, "fs"));
  broadcastRefresh("*", "repos");
  sendJson(res, 200, { repo: entry, initialized });
}

/**
 * Drive roots present on this machine, used as the picker's top "This PC" level.
 * @returns {string[]} drive-root paths (Windows: C:\ … Z:\; POSIX: just "/")
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

    if (p === "/api/repos" && req.method === "GET") return await listRepos(res);
    if (p === "/api/repos" && req.method === "POST") return await addRepo(req, res);
    if (p === "/api/repos" && req.method === "DELETE") return await deleteRepo(req, res);

    if (p === "/api/history" && req.method === "GET") {
      const length = Number(q.get("length") ?? 50);
      const events = await collect("revisionHistory", globalArgs, { length });
      return sendJson(res, 200, { revisions: xform.history(events) });
    }
    if (p === "/api/status" && req.method === "GET") {
      const events = await collect("repositoryStatus", globalArgs, { staged: true, scan: true });
      return sendJson(res, 200, xform.status(events));
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
      await collect("fileReset", { repositoryPath: rp }, { paths: absFiles(rp, files) });
      broadcastRefresh(rp, "reset");
      return sendJson(res, 200, { ok: true });
    }
    if (p === "/api/commit" && req.method === "POST") {
      const { path: rp, message } = await readBody(req);
      if (!message) return sendJson(res, 400, { error: "commit message required" });
      await collect("revisionCommit", { repositoryPath: rp }, { message });
      broadcastRefresh(rp, "commit");
      return sendJson(res, 200, { ok: true });
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
