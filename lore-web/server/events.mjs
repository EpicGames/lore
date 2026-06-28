// Server-Sent Events hub. lore-web pushes two kinds of messages to the browser:
//   - "refresh": a tracked repo changed on disk (from the file watcher) so the
//     SPA should refetch the affected views. This is what keeps lists live
//     instead of stale-until-restart.
//   - operation progress: streamed per-request from a long-running verb (sync,
//     push, clone) on its own dedicated SSE response, handled in routes.
// This module owns the shared "refresh" channel.

import { log } from "./log.mjs";

/** @type {Set<import("node:http").ServerResponse>} */
const clients = new Set();

/**
 * Register an HTTP response as an SSE subscriber to the refresh channel.
 * @param {import("node:http").ServerResponse} res
 */
export function addClient(res) {
  res.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
    Connection: "keep-alive",
  });
  res.write(": connected\n\n");
  clients.add(res);
  log.debug("sse client connected", { clients: clients.size });
  const keepAlive = setInterval(() => res.write(": ping\n\n"), 25_000);
  res.on("close", () => {
    clearInterval(keepAlive);
    clients.delete(res);
    log.debug("sse client disconnected", { clients: clients.size });
  });
}

/**
 * Broadcast a refresh notification to every connected browser.
 * @param {string} repoPath the repo whose data changed (or "*" for all)
 * @param {string} [reason]
 */
export function broadcastRefresh(repoPath, reason = "change") {
  const payload = JSON.stringify({ type: "refresh", repo: repoPath, reason });
  for (const res of clients) res.write(`data: ${payload}\n\n`);
}
