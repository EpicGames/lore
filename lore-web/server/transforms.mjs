// Shape normalized SDK event streams into the compact JSON the SPA consumes.
// Each function takes the array returned by sdk.collect() and returns plain data.

/** @typedef {{ tag: string, tagRaw: number, data: any }} LoreEvt */

/** Unwrap a Lore metadata value ({ tag, data, tagName }) to its inner value. */
function metaValue(value) {
  return value && typeof value === "object" && "data" in value ? value.data : value;
}

/**
 * Group revision-history events into revision records. Each REVISION_HISTORY_ENTRY
 * is followed by METADATA events (message, timestamp, branch) that belong to it.
 * @param {LoreEvt[]} events
 */
export function history(events) {
  const revisions = [];
  let current = null;
  for (const e of events) {
    if (e.tag === "REVISION_HISTORY_ENTRY") {
      current = {
        revision: e.data?.revision,
        revisionNumber: e.data?.revisionNumber,
        parent: e.data?.parent,
        message: undefined,
        timestamp: undefined,
        branch: undefined,
      };
      revisions.push(current);
    } else if (e.tag === "METADATA" && current) {
      const key = e.data?.key;
      const value = metaValue(e.data?.value);
      if (key === "message") current.message = value;
      else if (key === "timestamp") current.timestamp = value;
      else if (key === "branch") current.branch = value;
    }
  }
  return revisions;
}

/**
 * Reduce status events to a branch summary plus the list of changed files.
 * @param {LoreEvt[]} events
 */
export function status(events) {
  let branch = null;
  let revision = null;
  const files = [];
  let summary = null;
  for (const e of events) {
    if (e.tag === "REPOSITORY_STATUS_REVISION") {
      branch = e.data?.branchName ?? branch;
      revision = e.data?.revision ?? revision;
    } else if (e.tag === "REPOSITORY_STATUS_FILE") {
      files.push(e.data);
    } else if (e.tag === "REPOSITORY_STATUS_SUMMARY") {
      summary = e.data;
    }
  }
  return { branch, revision, files, summary };
}

/** @param {LoreEvt[]} events */
export function branches(events) {
  return events.filter((e) => e.tag === "BRANCH_LIST_ENTRY").map((e) => e.data);
}

/** @param {LoreEvt[]} events */
export function diff(events) {
  return events
    .filter((e) => e.tag === "FILE_DIFF" || e.tag === "REVISION_DIFF_FILE")
    .map((e) => e.data);
}

/** Branch/revision summary used to enrich the repo list. @param {LoreEvt[]} events */
export function repoSummary(events) {
  for (const e of events) {
    if (e.tag === "REPOSITORY_STATUS_REVISION") {
      return {
        branch: e.data?.branchName,
        revision: e.data?.revision,
        repository: e.data?.repository,
      };
    }
  }
  return {};
}
