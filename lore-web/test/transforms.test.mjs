// Transform unit tests. Pure functions over synthetic event arrays — no SDK, no
// repository, no filesystem. Event shapes mirror what sdk.collect() yields.

import { test } from "node:test";
import assert from "node:assert/strict";
import * as xform from "../server/transforms.mjs";

const ev = (tag, data) => ({ tag, tagRaw: 0, data });

test("history groups metadata under each revision entry", () => {
  const events = [
    ev("REVISION_HISTORY_ENTRY", { revision: "abc", revisionNumber: 2, parent: ["x"] }),
    ev("METADATA", { key: "message", value: { tag: 6, data: "second", tagName: "string" } }),
    ev("METADATA", { key: "timestamp", value: { tag: 5, data: 1700, tagName: "numeric" } }),
    ev("REVISION_HISTORY_ENTRY", { revision: "def", revisionNumber: 1, parent: [] }),
    ev("METADATA", { key: "message", value: { tag: 6, data: "first", tagName: "string" } }),
    ev("COMPLETE", { status: 0 }),
  ];
  const revs = xform.history(events);
  assert.equal(revs.length, 2);
  assert.equal(revs[0].message, "second");
  assert.equal(revs[0].timestamp, 1700);
  assert.equal(revs[1].message, "first");
  assert.equal(revs[1].timestamp, undefined);
});

test("metadata collects key/value pairs, unwrapping Lore metadata values", () => {
  const events = [
    ev("METADATA", { key: "name", value: { tag: 6, data: "acme/widgets", tagName: "string" } }),
    ev("METADATA", { key: "description", value: "plain" }),
    ev("COMPLETE", { status: 0 }),
  ];
  const meta = xform.metadata(events);
  assert.equal(meta.name, "acme/widgets");
  assert.equal(meta.description, "plain");
});

test("splitOrg separates the org prefix from the repository name", () => {
  assert.deepEqual(xform.splitOrg("acme/widgets"), {
    organization: "acme",
    repoName: "widgets",
    name: "acme/widgets",
  });
});

test("splitOrg treats a name with no slash as having no organization", () => {
  assert.deepEqual(xform.splitOrg("lore-web"), {
    organization: "",
    repoName: "lore-web",
    name: "lore-web",
  });
});

test("splitOrg keeps only the first slash as the org separator", () => {
  const out = xform.splitOrg("org/team/repo");
  assert.equal(out.organization, "org");
  assert.equal(out.repoName, "team/repo");
});

test("splitOrg tolerates a missing name", () => {
  assert.deepEqual(xform.splitOrg(undefined), { organization: "", repoName: "", name: "" });
});

test("status splits branch summary from changed files", () => {
  const events = [
    ev("REPOSITORY_STATUS_REVISION", { branchName: "main", revision: "r1" }),
    ev("REPOSITORY_STATUS_FILE", { path: "a.txt", flagStaged: true }),
    ev("REPOSITORY_STATUS_FILE", { path: "b.txt", flagStaged: false }),
    ev("COMPLETE", { status: 0 }),
  ];
  const s = xform.status(events);
  assert.equal(s.branch, "main");
  assert.equal(s.revision, "r1");
  assert.equal(s.files.length, 2);
});

test("branches keeps only BRANCH_LIST_ENTRY events", () => {
  const events = [
    ev("BRANCH_LIST_BEGIN", {}),
    ev("BRANCH_LIST_ENTRY", { name: "main" }),
    ev("BRANCH_LIST_END", {}),
  ];
  assert.deepEqual(xform.branches(events), [{ name: "main" }]);
});

test("remoteRepos keeps only REPOSITORY_LIST_ENTRY events", () => {
  const events = [
    ev("REPOSITORY_LIST_BEGIN", {}),
    ev("REPOSITORY_LIST_ENTRY", { id: "abc", name: "ui-demo" }),
    ev("REPOSITORY_LIST_ENTRY", { id: "def", name: "scratch" }),
    ev("REPOSITORY_LIST_END", {}),
  ];
  assert.deepEqual(xform.remoteRepos(events), [
    { id: "abc", name: "ui-demo" },
    { id: "def", name: "scratch" },
  ]);
});
