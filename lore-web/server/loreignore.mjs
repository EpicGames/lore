// .loreignore management. Lore reads ignore rules from a gitignore-style
// .loreignore at the working-copy root (see lore-revision repository::load_filter).
// These helpers seed and extend it from the UI without the user hand-editing files.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const LOREIGNORE = ".loreignore";
const GITIGNORE = ".gitignore";

// Git's own metadata is meaningless to Lore, so a fresh .loreignore always
// excludes it. .gitignore is listed too: when Git and Lore coexist it is Git's
// file, not something Lore should version.
const LORE_IGNORES = [".git/", ".gitignore"];
// Conversely, once Lore manages the working copy its metadata should stay out of
// Git, so .gitignore gains the Lore counterparts.
const GIT_IGNORES = [".lore/", ".loreignore"];

const splitLines = (text) => text.split(/\r?\n/);

/** Compare ignore entries ignoring trailing slashes and surrounding space. */
function sameEntry(a, b) {
  const norm = (s) => s.trim().replace(/\/+$/, "");
  return norm(a) === norm(b);
}

/**
 * Append the entries not already present to an ignore file, creating it if
 * needed. A header comment is written above the first batch actually added.
 * @returns {string[]} the entries that were newly written (empty if no change)
 */
function appendEntries(filePath, header, entries) {
  let text = existsSync(filePath) ? readFileSync(filePath, "utf8") : "";
  const lines = splitLines(text);
  const missing = entries.filter((e) => !lines.some((l) => sameEntry(l, e)));
  if (missing.length === 0) return [];
  let out = text;
  if (out && !out.endsWith("\n")) out += "\n";
  if (header) out += `${out ? "\n" : ""}${header}\n`;
  out += missing.join("\n") + "\n";
  writeFileSync(filePath, out);
  return missing;
}

/**
 * Set up .loreignore for a working copy (on init, or on demand for an existing
 * repo). Seeds .loreignore from .gitignore when present, ensures Git's files are
 * ignored by Lore, and — when a .gitignore exists — ensures Lore's files are
 * ignored by Git. Idempotent: safe to call on an already-configured repo.
 * @param {string} repoPath working-copy root
 */
export function setupLoreignore(repoPath) {
  const lorePath = join(repoPath, LOREIGNORE);
  const gitPath = join(repoPath, GITIGNORE);
  const gitExists = existsSync(gitPath);
  const created = !existsSync(lorePath);

  // Seed a brand-new .loreignore from .gitignore — the same paths a user already
  // declared uninteresting to Git are a sensible starting point for Lore.
  if (created && gitExists) {
    writeFileSync(lorePath, readFileSync(gitPath, "utf8"));
  }
  appendEntries(lorePath, "# Git files (managed by Git, not Lore)", LORE_IGNORES);

  let gitignoreUpdated = false;
  if (gitExists) {
    gitignoreUpdated = appendEntries(gitPath, "# Lore files", GIT_IGNORES).length > 0;
  }
  return { created, gitignoreUpdated };
}

/**
 * Append a single gitignore-style pattern (a file, folder, or *.ext glob) to
 * .loreignore, creating the file if it does not exist yet. No-op if already
 * present.
 * @returns {boolean} whether the pattern was newly added
 */
export function appendIgnorePattern(repoPath, pattern) {
  return appendEntries(join(repoPath, LOREIGNORE), null, [pattern]).length > 0;
}

/** Whether the working copy already has a .loreignore file. */
export function hasLoreignore(repoPath) {
  return existsSync(join(repoPath, LOREIGNORE));
}

/** Whether the working copy has a .gitignore file. */
export function hasGitignore(repoPath) {
  return existsSync(join(repoPath, GITIGNORE));
}
