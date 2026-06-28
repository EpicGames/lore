// lore-web HTTP server. Built on Node's stdlib http only (no web framework) so
// the whole tool runs on any machine with Node + the vendored SDK, no build and
// no extra native deps. Bound to 127.0.0.1: it exposes full repo write access
// and must never be reachable off-host.

import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { existsSync } from "node:fs";
import { join, extname, normalize as normalizePath, dirname } from "node:path";
import { fileURLToPath } from "node:url";

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
const PORT = Number(process.env.LORE_WEB_PORT ?? 7420);

/** A path is a Lore working copy if it holds a .lore (or legacy .urc) dir. */
function isRepo(path) {
  return existsSync(join(path, ".lore")) || existsSync(join(path, ".urc"));
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

/** POST /api/repos — start tracking a working copy. */
async function addRepo(req, res) {
  const { path, label } = await readBody(req);
  if (!path || typeof path !== "string") return sendJson(res, 400, { error: "path required" });
  if (!existsSync(path)) return sendJson(res, 400, { error: "path does not exist" });
  if (!isRepo(path)) return sendJson(res, 400, { error: "not a Lore repository (no .lore directory)" });
  const entry = store.addRepo(path, label);
  watchRepo(path, () => broadcastRefresh(path, "fs"));
  broadcastRefresh("*", "repos");
  sendJson(res, 200, { repo: entry });
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

    // --- API ---
    if (p === "/events" && req.method === "GET") return addClient(res);

    if (p === "/api/auth" && req.method === "GET") {
      return sendJson(res, 200, { loggedIn: await isLoggedIn() });
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
      const events = await collect("fileDiff", globalArgs, abs ? { paths: [abs] } : {});
      return sendJson(res, 200, { diff: xform.diff(events) });
    }

    // --- write actions (quick; respond with refreshed nothing, broadcast refresh) ---
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

    // --- remote operations (streamed progress as NDJSON) ---
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
      return await streamOp(res, "repositoryClone", { repositoryPath: dest }, { repositoryUrl: url }, null);
    }

    // --- static SPA ---
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
