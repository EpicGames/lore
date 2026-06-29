// .loreignore management tests. Operate on a throwaway temp directory — no SDK
// and no repository, just the file shuffling the helpers perform.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import { setupLoreignore, appendIgnorePattern, hasLoreignore } from "../server/loreignore.mjs";

function freshRepo() {
  return mkdtempSync(join(tmpdir(), "loreignore-"));
}

test("setup seeds .loreignore from .gitignore and ignores Git's files", () => {
  const dir = freshRepo();
  try {
    writeFileSync(join(dir, ".gitignore"), "node_modules/\n*.log\n");
    const result = setupLoreignore(dir);
    assert.equal(result.created, true);
    assert.equal(result.gitignoreUpdated, true);

    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.match(lore, /node_modules\//);
    assert.match(lore, /\*\.log/);
    assert.match(lore, /^\.git\/$/m);
    assert.match(lore, /^\.gitignore$/m);

    const git = readFileSync(join(dir, ".gitignore"), "utf8");
    assert.match(git, /^\.lore\/$/m);
    assert.match(git, /^\.loreignore$/m);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("setup creates .loreignore even without a .gitignore", () => {
  const dir = freshRepo();
  try {
    const result = setupLoreignore(dir);
    assert.equal(result.created, true);
    assert.equal(result.gitignoreUpdated, false);
    assert.ok(hasLoreignore(dir));
    assert.equal(existsSync(join(dir, ".gitignore")), false);
    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.match(lore, /^\.git\/$/m);
    assert.match(lore, /^\.gitignore$/m);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("setup is idempotent — no duplicate entries on re-run", () => {
  const dir = freshRepo();
  try {
    writeFileSync(join(dir, ".gitignore"), "dist/\n");
    setupLoreignore(dir);
    setupLoreignore(dir);
    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.equal(lore.match(/^\.git\/$/gm).length, 1);
    const git = readFileSync(join(dir, ".gitignore"), "utf8");
    assert.equal(git.match(/^\.lore\/$/gm).length, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("setup tolerates trailing-slash variants already present", () => {
  const dir = freshRepo();
  try {
    // .git (no slash) should be recognized as the same entry as .git/.
    writeFileSync(join(dir, ".loreignore"), ".git\n");
    setupLoreignore(dir);
    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.equal(lore.match(/^\.git\b/gm).length, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("appendIgnorePattern adds a pattern once and reports duplicates", () => {
  const dir = freshRepo();
  try {
    assert.equal(appendIgnorePattern(dir, "*.tmp"), true);
    assert.equal(appendIgnorePattern(dir, "*.tmp"), false);
    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.equal(lore.match(/^\*\.tmp$/gm).length, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
