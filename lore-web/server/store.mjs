// Persistent record of the repositories the user has added to lore-web. This is
// the ONLY thing lore-web persists: a set of working-copy paths plus labels.
// Repository *data* (revisions, status, branches) is never cached here — it is
// always read live from the SDK so the UI cannot go stale (the core defect of
// the desktop app this replaces).

import { readFileSync, writeFileSync, mkdirSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { log } from "./log.mjs";

/** @typedef {{ path: string, label: string, addedAt: number }} RepoEntry */

const STORE_PATH =
  process.env.LORE_WEB_STORE ?? join(homedir(), ".lore-web", "store.json");

/** @type {{ repos: RepoEntry[], defaultRemote?: string }} */
let state = { repos: [], defaultRemote: process.env.LORE_WEB_DEFAULT_REMOTE ?? "" };

/**
 * Load persisted state from `STORE_PATH` into module state, if the file exists.
 * A missing file is normal (first run) and leaves the initial empty state in
 * place. A corrupt file must not take the app down, so it is treated the same
 * way, with a warning logged for visibility.
 */
function loadFromDisk() {
  if (!existsSync(STORE_PATH)) return;
  try {
    const parsed = JSON.parse(readFileSync(STORE_PATH, "utf8"));
    if (parsed && Array.isArray(parsed.repos)) state = parsed;
  } catch (err) {
    log.warn("store unreadable, starting empty", {
      path: STORE_PATH,
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

/** Write the current module state to `STORE_PATH`, creating its directory if needed. */
function persist() {
  mkdirSync(dirname(STORE_PATH), { recursive: true });
  writeFileSync(STORE_PATH, JSON.stringify(state, null, 2));
}

loadFromDisk();

/** @returns {RepoEntry[]} a copy of the tracked repositories, newest first */
export function listRepos() {
  return [...state.repos].sort((a, b) => b.addedAt - a.addedAt);
}

/** @param {string} path */
export function getRepo(path) {
  return state.repos.find((r) => r.path === path);
}

/**
 * Add (or relabel) a tracked repository.
 * @param {string} path absolute working-copy path
 * @param {string} label display name
 * @returns {RepoEntry}
 */
export function addRepo(path, label) {
  const existing = getRepo(path);
  if (existing) {
    if (label) existing.label = label;
    persist();
    return existing;
  }
  const entry = { path, label: label || path, addedAt: Date.now() };
  state.repos.push(entry);
  persist();
  return entry;
}

/**
 * Stop tracking a repository. Always succeeds, even for a path whose folder no
 * longer exists — removing a dangling entry must never be blocked (the desktop
 * bug this fixes refused to remove a repo once its folder was deleted).
 * @param {string} path
 * @returns {boolean} whether an entry was removed
 */
export function removeRepo(path) {
  const before = state.repos.length;
  state.repos = state.repos.filter((r) => r.path !== path);
  const removed = state.repos.length < before;
  if (removed) persist();
  return removed;
}

/**
 * Get the configured default remote server URL.
 * @returns {string} the remote server URL or empty string if not set
 */
export function getDefaultRemote() {
  return state.defaultRemote || "";
}

/**
 * Set the default remote server URL for repositories. Validates the URL format.
 * @param {string} url the remote server URL, for example "lore://127.0.0.1:41337"
 * @throws {Error} if URL is malformed
 */
export function setDefaultRemote(url) {
  const trimmed = (url || "").trim();
  if (trimmed && !trimmed.match(/^[a-z][a-z0-9+.-]*:\/\/[^/]+/i)) {
    throw new Error(`invalid remote URL format: "${trimmed}"`);
  }
  state.defaultRemote = trimmed;
  persist();
}
