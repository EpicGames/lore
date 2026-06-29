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

/**
 * The files changed in a single revision, from `revisionInfo({ delta: true })`.
 * @param {LoreEvt[]} events
 */
export function revisionFiles(events) {
  return events
    .filter((e) => e.tag === "REVISION_INFO_DELTA")
    // The delta walks the whole tree; keep only entries that actually changed
    // (content modified, or added/deleted/moved — action other than KEEP=0).
    // Unchanged directory/context entries (action KEEP, not modified) are noise.
    .filter((e) => e.data?.flagModify || (e.data?.action ?? 0) !== 0)
    .map((e) => ({
      path: e.data?.path,
      action: e.data?.action,
      size: e.data?.size,
      flagModify: e.data?.flagModify,
      flagMerged: e.data?.flagMerged,
    }));
}

/** @param {LoreEvt[]} events */
export function diff(events) {
  return events
    .filter((e) => e.tag === "FILE_DIFF" || e.tag === "REVISION_DIFF_FILE")
    .map((e) => e.data);
}

/** Repositories a server hosts, from `repositoryList`. @param {LoreEvt[]} events */
export function remoteRepos(events) {
  return events
    .filter((e) => e.tag === "REPOSITORY_LIST_ENTRY")
    .map((e) => ({ id: e.data?.id, name: e.data?.name }));
}

/** Collect repository metadata key/values from `repositoryMetadataGet`. @param {LoreEvt[]} events */
export function metadata(events) {
  /** @type {Record<string, any>} */
  const out = {};
  for (const e of events) {
    if (e.tag === "METADATA" && e.data?.key != null) out[e.data.key] = metaValue(e.data.value);
  }
  return out;
}

/**
 * Split a repository `name` metadata value into its organization prefix and bare
 * repository name. Lore encodes the org as an `org/repo` prefix on the name (it
 * comes from the path of the create/clone URL); everything before the first slash
 * is the organization, the remainder is the repository name.
 * @param {string|undefined} name
 * @returns {{ organization: string, repoName: string, name: string }}
 */
export function splitOrg(name) {
  const full = typeof name === "string" ? name : "";
  const slash = full.indexOf("/");
  if (slash === -1) return { organization: "", repoName: full, name: full };
  return { organization: full.slice(0, slash), repoName: full.slice(slash + 1), name: full };
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
