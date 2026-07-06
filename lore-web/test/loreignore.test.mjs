// .loreignore management tests. Operate on a throwaway temp directory — no SDK
// and no repository, just the file shuffling the helpers perform.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, readFileSync, existsSync, rmSync, chmodSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import { setupLoreignore, appendIgnorePattern, hasLoreignore, hasP4ignore } from "../server/loreignore.mjs";

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

test("setup seeds .loreignore from .p4ignore and ignores Perforce's files", () => {
  const dir = freshRepo();
  try {
    writeFileSync(join(dir, ".p4ignore"), "*.p4d\nbuild/\n");
    const result = setupLoreignore(dir);
    assert.equal(result.created, true);
    assert.equal(result.p4ignoreUpdated, true);

    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.match(lore, /\*\.p4d/);
    assert.match(lore, /build\//);
    assert.match(lore, /^\.p4\/$/m);
    assert.match(lore, /^\.p4ignore$/m);

    const p4 = readFileSync(join(dir, ".p4ignore"), "utf8");
    assert.match(p4, /^\.lore\/$/m);
    assert.match(p4, /^\.loreignore$/m);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("setup converts Perforce patterns (with ...) to gitignore format", () => {
  const dir = freshRepo();
  try {
    // Perforce uses `...` for recursive matching; must convert to gitignore `/`.
    writeFileSync(
      join(dir, ".p4ignore"),
      "# Unreal generated folders\n**/Intermediate/...\n**/Saved/...\n.vs/...\n*.log\n",
    );
    const result = setupLoreignore(dir);
    assert.equal(result.created, true);

    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    // Perforce `**/Intermediate/...` should be converted to `**/Intermediate/` (gitignore treats `/` as recursive).
    assert.match(lore, /^\*\*\/Intermediate\/$/m);
    assert.match(lore, /^\*\*\/Saved\/$/m);
    assert.match(lore, /^\.vs\/$/m);
    assert.match(lore, /^\*\.log$/m);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("setup merges patterns from both .gitignore and .p4ignore without duplicates", () => {
  const dir = freshRepo();
  try {
    writeFileSync(join(dir, ".gitignore"), "node_modules/\n*.log\n");
    writeFileSync(join(dir, ".p4ignore"), "*.log\nbuild/\n");
    const result = setupLoreignore(dir);
    assert.equal(result.created, true);
    assert.equal(result.gitignoreUpdated, true);
    assert.equal(result.p4ignoreUpdated, true);

    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.match(lore, /node_modules\//);
    assert.match(lore, /build\//);
    // *.log should appear only once despite being in both source files
    assert.equal(lore.match(/^\*\.log$/gm)?.length ?? 0, 1);
    assert.match(lore, /^\.git\/$/m);
    assert.match(lore, /^\.p4\/$/m);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("setup seeds .loreignore even when .p4ignore is read-only (Perforce)", () => {
  const dir = freshRepo();
  const p4 = join(dir, ".p4ignore");
  try {
    writeFileSync(p4, "build/\n");
    // Perforce leaves versioned files read-only until `p4 edit`.
    chmodSync(p4, 0o444);
    const result = setupLoreignore(dir);
    // The important half still succeeds: .loreignore is seeded from .p4ignore.
    assert.equal(result.created, true);
    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.match(lore, /build\//);
    assert.match(lore, /^\.p4ignore$/m);
    // The counterpart write into the read-only .p4ignore is reported, not thrown.
    assert.equal(result.p4ignoreUpdated, false);
    assert.equal(result.p4ignoreBlocked, true);
  } finally {
    chmodSync(p4, 0o644);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("setup is idempotent with both .gitignore and .p4ignore", () => {
  const dir = freshRepo();
  try {
    writeFileSync(join(dir, ".gitignore"), "dist/\n");
    writeFileSync(join(dir, ".p4ignore"), "build/\n");
    setupLoreignore(dir);
    setupLoreignore(dir);
    const lore = readFileSync(join(dir, ".loreignore"), "utf8");
    assert.equal(lore.match(/^\.git\/$/gm).length, 1);
    assert.equal(lore.match(/^\.p4\/$/gm).length, 1);
    const git = readFileSync(join(dir, ".gitignore"), "utf8");
    assert.equal(git.match(/^\.lore\/$/gm).length, 1);
    const p4 = readFileSync(join(dir, ".p4ignore"), "utf8");
    assert.equal(p4.match(/^\.lore\/$/gm).length, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
