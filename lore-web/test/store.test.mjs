// Store unit tests. Each run uses its own throwaway store file (env-injected
// before import) so tests stay isolated and leave no shared state behind.

import { test } from "node:test";
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rmSync } from "node:fs";

const storeFile = join(tmpdir(), `lore-web-store-test-${process.pid}.json`);
process.env.LORE_WEB_STORE = storeFile;

const store = await import("../server/store.mjs");

test.after(() => rmSync(storeFile, { force: true }));

test("add and list a repo", () => {
  store.addRepo("D:/repo-a", "A");
  const repos = store.listRepos();
  assert.equal(repos.length, 1);
  assert.equal(repos[0].label, "A");
});

test("relabel an existing repo without duplicating", () => {
  store.addRepo("D:/repo-a", "A-renamed");
  const repos = store.listRepos().filter((r) => r.path === "D:/repo-a");
  assert.equal(repos.length, 1);
  assert.equal(repos[0].label, "A-renamed");
});

test("removing a dangling repo always succeeds (issue #4)", () => {
  // The folder never has to exist — removal must never be blocked.
  store.addRepo("D:/repo-gone", "ghost");
  const removed = store.removeRepo("D:/repo-gone");
  assert.equal(removed, true);
  assert.ok(!store.listRepos().some((r) => r.path === "D:/repo-gone"));
});

test("removing an untracked path reports false, does not throw", () => {
  assert.equal(store.removeRepo("D:/never-added"), false);
});
