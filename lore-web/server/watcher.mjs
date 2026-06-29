// Per-repo filesystem watcher. When a tracked working copy changes on disk, we
// notify the browser to refetch — this is what makes lists live instead of
// stale-until-restart. Built on node:fs.watch (recursive is supported on
// Windows and macOS); changes are debounced so a burst of writes yields one
// refresh.

import { watch } from "node:fs";
import { join } from "node:path";
import { log } from "./log.mjs";

/** @type {Map<string, { watchers: import("node:fs").FSWatcher[], timer: NodeJS.Timeout|null }>} */
const active = new Map();

const DEBOUNCE_MS = 400;

/**
 * Start watching a repo's working tree and its .lore metadata dir. Calling this
 * again for an already-watched path is a no-op.
 * @param {string} repoPath
 * @param {() => void} onChange invoked (debounced) when anything changes
 */
export function watchRepo(repoPath, onChange) {
  if (active.has(repoPath)) return;
  const entry = { watchers: /** @type {import("node:fs").FSWatcher[]} */ ([]), timer: null };

  const fire = () => {
    if (entry.timer) clearTimeout(entry.timer);
    entry.timer = setTimeout(() => {
      entry.timer = null;
      onChange();
    }, DEBOUNCE_MS);
  };

  // Watch the working tree (recursive) for content changes, and the .lore dir
  // for new revisions/branch updates committed by the CLI or a remote push.
  for (const target of [repoPath, join(repoPath, ".lore")]) {
    try {
      const w = watch(target, { recursive: true }, fire);
      w.on("error", (err) => log.debug("watch error", { target, error: err.message }));
      entry.watchers.push(w);
    } catch (err) {
      log.debug("watch failed", { target, error: err instanceof Error ? err.message : String(err) });
    }
  }

  active.set(repoPath, entry);
  log.debug("watching repo", { repoPath, watchers: entry.watchers.length });
}

/** Stop watching a repo. @param {string} repoPath */
export function unwatchRepo(repoPath) {
  const entry = active.get(repoPath);
  if (!entry) return;
  if (entry.timer) clearTimeout(entry.timer);
  for (const w of entry.watchers) {
    try {
      w.close();
    } catch {
      // Already closed or the path vanished; nothing to do.
    }
  }
  active.delete(repoPath);
}
