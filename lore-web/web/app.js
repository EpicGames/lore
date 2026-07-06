// lore-web single-page UI. Vanilla ES modules, no build step. The guiding rule:
// never trust a cached snapshot — every view refetches live (on select, on a
// file-watch "refresh" push, on window focus, and on a slow history poll), which
// is what keeps lists fresh where the desktop app went stale.

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

const state = {
  repos: [],
  active: null, // repo path
  tab: "changes",
  selectedFile: null,
  defaultRemote: "",
  discoveredServers: [],
};

async function apiGet(path) {
  const res = await fetch(path);
  const body = await res.json();
  if (!res.ok) throw new Error(body.error || res.statusText);
  return body;
}

async function apiPost(path, payload) {
  const res = await fetch(path, {
    method: payload && payload._method === "DELETE" ? "DELETE" : "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  const body = await res.json();
  if (!res.ok) throw new Error(body.error || res.statusText);
  return body;
}

/** POST and consume an NDJSON progress stream, invoking onEvent per line. */
async function apiStream(path, payload, onEvent) {
  const res = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      if (line) onEvent(JSON.parse(line));
    }
  }
}

function toast(msg, isErr) {
  const t = $("#toast");
  t.textContent = msg;
  t.className = "toast" + (isErr ? " err" : "");
  t.hidden = false;
  setTimeout(() => (t.hidden = true), isErr ? 5000 : 2500);
}

async function loadRepos() {
  try {
    const { repos } = await apiGet("/api/repos");
    state.repos = repos;
    renderRepos();
  } catch (err) {
    toast(err.message, true);
  }
}

function renderRepos() {
  const ul = $("#repo-list");
  ul.innerHTML = "";
  for (const r of state.repos) {
    const li = document.createElement("li");
    li.className = r.path === state.active ? "active" : "";
    li.innerHTML = `
      <span class="r-name" title="${r.path}">${r.label}</span>
      ${r.organization ? `<span class="r-org">${r.organization}</span>` : ""}
      ${r.exists ? `<span class="r-branch">${r.branch || ""}</span>` : `<span class="r-missing">missing</span>`}
      <button class="r-remove" title="Remove">✕</button>`;
    // Select on a click anywhere in the row, not only the label, so the whole
    // row is one big hit target. The remove button stops propagation below.
    li.onclick = () => selectRepo(r.path);
    li.querySelector(".r-remove").onclick = (e) => {
      e.stopPropagation();
      removeRepo(r.path);
    };
    ul.appendChild(li);
  }
}

/** Pick a folder, then track it — initializing a new repo if it isn't one yet. */
async function addRepo() {
  const path = await pickFolder({ title: "Add a repository" });
  if (!path) return;
  let url;
  try {
    // Brand-new folders are initialized; let the user review/edit the URL first.
    const info = await apiGet(`/api/init-url?path=${encodeURIComponent(path)}`);
    if (!info.isRepo) {
      url = await confirmInit(path, info.url);
      if (url === null) return; // cancelled
    }
  } catch (err) {
    return toast(err.message, true);
  }
  try {
    const { initialized } = await apiPost("/api/repos", { path, url });
    await loadRepos();
    selectRepo(path);
    toast(initialized ? "Repository initialized" : "Repository added");
  } catch (err) {
    toast(err.message, true);
  }
}

/**
 * Show the generated repository URL for a soon-to-be-initialized folder in an
 * editable box. Resolves to the (possibly edited) URL, or null if cancelled.
 */
let initResolve = null;
function confirmInit(path, suggestedUrl) {
  $("#init-folder").textContent = `${path} is not a Lore repository yet — it will be created with:`;
  $("#init-url").value = suggestedUrl || "";
  $("#init-dialog").showModal();
  return new Promise((resolve) => {
    initResolve = resolve;
  });
}

function initFinish(url) {
  $("#init-dialog").close();
  const resolve = initResolve;
  initResolve = null;
  resolve?.(url);
}

function wireInit() {
  $("#init-cancel").onclick = () => initFinish(null);
  $("#init-go").onclick = () => {
    const v = $("#init-url").value.trim();
    if (!v) return toast("Repository URL required", true);
    initFinish(v);
  };
  $("#init-dialog").addEventListener("cancel", (e) => {
    e.preventDefault();
    initFinish(null);
  });
}

async function removeRepo(path) {
  try {
    await apiPost("/api/repos", { path, _method: "DELETE" });
    if (state.active === path) {
      state.active = null;
      showEmpty();
    }
    await loadRepos();
    toast("Repository removed");
  } catch (err) {
    toast(err.message, true);
  }
}

function showEmpty() {
  $("#empty").hidden = false;
  $("#repo-view").hidden = true;
}

async function selectRepo(path) {
  state.active = path;
  state.selectedFile = null;
  const repo = state.repos.find((r) => r.path === path);
  $("#empty").hidden = true;
  $("#repo-view").hidden = false;
  $("#repo-title").textContent = repo?.label || path;
  $("#repo-path").textContent = path;
  renderRepos();
  loadOrg(path);
  await refreshActive();
}

/**
 * Fetch the active repo's organization and show it as a clickable pill. The org
 * is the prefix of the repo's `name` metadata; a repo with no org prefix hides
 * the pill. Best-effort — a read failure leaves the pill hidden rather than
 * surfacing an error.
 * @param {string} path the repository path
 */
async function loadOrg(path) {
  const pill = $("#repo-org");
  pill.hidden = true;
  try {
    const { organization, repoName } = await apiGet(`/api/org?path=${encodeURIComponent(path)}`);
    state.org = { organization, repoName };
    pill.textContent = organization || "Set organization…";
    pill.classList.toggle("org-empty", !organization);
    pill.hidden = false;
  } catch (err) {
    state.org = null;
  }
}

/** Refetch every view for the active repo. The single source of freshness. */
async function refreshActive() {
  if (!state.active) return;
  const path = encodeURIComponent(state.active);
  await Promise.all([loadStatus(path), loadHistory(path), loadBranches(path)]);
}

function fileBadge(f) {
  // A directory that is itself a Lore working copy: a live nested repo. Offer to
  // ignore it (see the changes bar) rather than track a repo-inside-a-repo.
  if (f.nested) return ["nested", "badge-nested"];
  // A directory (LoreNodeType.DIRECTORY = 0) reported with action DELETE that is
  // no longer on disk is a *stale* nested-repo entry — a Lore zombie that no
  // discard can clear (only "Repair repository" can). Not a real deletion.
  if (f.type === 0 && f.action === 2) return ["stale", "badge-stale"];
  if (f.action === 1) return ["A", "badge-A"];
  if (f.action === 2) return ["D", "badge-D"];
  if (f.action === 3) return ["R", "badge-M"];
  return ["M", "badge-M"];
}

/** Fetches repository status and renders the staged/unstaged file lists. */
async function loadStatus(pathEnc) {
  try {
    const data = await apiGet(`/api/status?path=${pathEnc}`);
    $("#repo-branch").textContent = data.branch || "";
    const staged = data.files.filter((f) => f.flagStaged);
    const unstaged = data.files.filter((f) => !f.flagStaged);
    renderFiles($("#staged-files"), staged, "unstage");
    renderFiles($("#unstaged-files"), unstaged, "stage");
    $("#commit-btn").disabled = staged.length === 0;
    $("#stage-all-btn").disabled = unstaged.length === 0;
    state.unstaged = unstaged;
    updateChangesBar(data);
  } catch (err) {
    toast(err.message, true);
  }
}

function renderFiles(ul, files, action) {
  ul.innerHTML = "";
  if (files.length === 0) {
    ul.innerHTML = `<li class="muted">— none —</li>`;
    return;
  }
  for (const f of files) {
    const [label, cls] = fileBadge(f);
    const li = document.createElement("li");
    li.innerHTML = `
      <span class="f-act ${cls}">${label}</span>
      <span class="f-path" title="${f.path}">${f.path}</span>
      <button class="f-do">${action === "stage" ? "Stage" : "Unstage"}</button>
      <button class="f-ignore" title="Add to .loreignore">⊘</button>
      ${action === "stage" ? `<button class="f-reset" title="Discard changes">↺</button>` : ""}`;
    li.querySelector(".f-path").onclick = () => showDiff(f.path);
    li.querySelector(".f-do").onclick = () => fileAction(action, f.path);
    li.querySelector(".f-ignore").onclick = () => openIgnoreMenu(f);
    li.querySelector(".f-reset")?.addEventListener("click", () => fileAction("reset", f.path));
    ul.appendChild(li);
  }
}

async function fileAction(action, file) {
  try {
    await apiPost(`/api/${action}`, { path: state.active, files: [file] });
    // SSE refresh will follow, but refetch now for immediate feedback.
    await loadStatus(encodeURIComponent(state.active));
  } catch (err) {
    toast(err.message, true);
  }
}

/** Stages every currently unstaged file in the active repository. */
async function stageAll() {
  const files = (state.unstaged || []).map((f) => f.path);
  if (files.length === 0) return;
  try {
    await apiPost("/api/stage", { path: state.active, files });
    await loadStatus(encodeURIComponent(state.active));
  } catch (err) {
    toast(err.message, true);
  }
}

/**
 * The ignore patterns offered for a file: the file itself, its parent folder,
 * and its extension. Patterns are gitignore-style with forward slashes (Lore's
 * ignore syntax), regardless of the path separator the status used.
 */
function ignoreOptionsFor(f) {
  const path = (f.path || "").replace(/\\/g, "/");
  const sep = path.lastIndexOf("/");
  const name = path.slice(sep + 1);
  const parent = sep >= 0 ? path.slice(0, sep + 1) : "";
  const opts = [];
  if (f.type === 0) {
    // A directory entry (for example, a stale nested-repo marker): ignore the folder
    // itself with a trailing slash. This is the way to clear nested-repo
    // phantoms — Lore's status filter excludes ignored paths.
    opts.push({ pattern: `${path}/`, label: "This folder" });
  } else {
    opts.push({ pattern: path, label: "This file" });
    const dot = name.lastIndexOf(".");
    if (dot > 0) opts.push({ pattern: `*${name.slice(dot)}`, label: "All files with this extension" });
  }
  if (parent) opts.push({ pattern: parent, label: "Its parent folder" });
  return opts;
}

function openIgnoreMenu(f) {
  const ul = $("#ignore-options");
  ul.innerHTML = "";
  for (const o of ignoreOptionsFor(f)) {
    const li = document.createElement("li");
    li.innerHTML = `<code>${o.pattern}</code><span class="muted">${o.label}</span>`;
    li.onclick = () => {
      $("#ignore-dialog").close();
      ignorePattern(o.pattern);
    };
    ul.appendChild(li);
  }
  $("#ignore-dialog").showModal();
}

async function ignorePattern(pattern) {
  try {
    const { added } = await apiPost("/api/ignore", { path: state.active, pattern });
    toast(added ? `Ignoring ${pattern}` : `${pattern} was already ignored`);
    await loadStatus(encodeURIComponent(state.active));
  } catch (err) {
    toast(err.message, true);
  }
}

async function initLoreignore() {
  try {
    const { created, gitignoreUpdated, p4ignoreUpdated, p4ignoreBlocked } = await apiPost(
      "/api/init-loreignore",
      { path: state.active },
    );
    toast(created ? "Created .loreignore" : "Updated .loreignore");
    if (gitignoreUpdated) toast("Updated .gitignore");
    if (p4ignoreUpdated) toast("Updated .p4ignore");
    // Perforce keeps .p4ignore read-only until it is opened for edit, so Lore
    // cannot add its entries there on its own.
    if (p4ignoreBlocked) toast(".p4ignore is read-only — run p4 edit .p4ignore, then retry", true);
    await loadStatus(encodeURIComponent(state.active));
  } catch (err) {
    toast(err.message, true);
  }
}

function barButton(label, onclick, cls) {
  const b = document.createElement("button");
  b.className = cls || "ghost";
  b.textContent = label;
  b.onclick = onclick;
  return b;
}

/**
 * Populate the toolbar above the file lists with context actions: set up
 * .loreignore, ignore live nested repos (so they never rot into zombies), and
 * repair stale nested-repo entries that no discard can clear.
 */
function updateChangesBar(data) {
  const bar = $(".changes-bar");
  bar.innerHTML = "";
  const files = data.files || [];
  const nested = files.filter((f) => f.nested);
  const stale = files.filter((f) => f.type === 0 && f.action === 2 && !f.nested);

  if (data.hasLoreignore === false) {
    bar.appendChild(barButton("Initialize .loreignore", initLoreignore));
  } else if (data.hasGitignore || data.hasP4ignore) {
    bar.appendChild(barButton("Re-sync ignore patterns", initLoreignore, "ghost"));
  }
  if (nested.length) {
    const n = nested.length;
    const note = document.createElement("span");
    note.className = "bar-note";
    note.textContent = `${n} nested ${n === 1 ? "repository" : "repositories"} — ignore so Lore doesn't track a repo-in-a-repo`;
    bar.appendChild(note);
    bar.appendChild(barButton(`Ignore nested`, () => ignoreNested(nested)));
  }
  if (stale.length) {
    const n = stale.length;
    const note = document.createElement("span");
    note.className = "bar-note warn";
    note.textContent = `${n} stale nested ${n === 1 ? "entry" : "entries"} can't be discarded`;
    bar.appendChild(note);
    bar.appendChild(barButton("Repair repository…", repairRepository));
  }
  bar.hidden = bar.childElementCount === 0;
}

/** Add each live nested repo to .loreignore (as a folder pattern). */
async function ignoreNested(list) {
  try {
    for (const f of list) {
      const path = (f.path || "").replace(/\\/g, "/");
      await apiPost("/api/ignore", { path: state.active, pattern: `${path}/` });
    }
    toast(`Ignored ${list.length} nested ${list.length === 1 ? "repo" : "repos"}`);
    await loadStatus(encodeURIComponent(state.active));
  } catch (err) {
    toast(err.message, true);
  }
}

/**
 * Rebuild the repo's .lore to purge stale "zombie" entries Lore can't otherwise
 * remove. Files are untouched; the server refuses if there is committed history.
 */
async function repairRepository() {
  const ok = confirm(
    "Rebuild this repository's index to clear stale entries?\n\n" +
      "Your files are not touched, and the repository keeps its identity and remote. " +
      "You can only do this before anything has been committed.",
  );
  if (!ok) return;
  try {
    await apiPost("/api/repair", { path: state.active });
    toast("Repository repaired");
    await refreshActive();
  } catch (err) {
    toast(err.message, true);
  }
}

async function showDiff(file) {
  const view = $("#diff-view");
  state.selectedFile = file;
  try {
    const { diff } = await apiGet(`/api/diff?path=${encodeURIComponent(state.active)}&file=${encodeURIComponent(file)}`);
    const patch = diff.map((d) => d.patch || "").join("\n");
    view.innerHTML = colorizeDiff(patch || "(no differences)");
    view.classList.add("show");
  } catch (err) {
    toast(err.message, true);
  }
}

function colorizeDiff(text) {
  return text
    .split("\n")
    .map((line) => {
      const esc = line.replace(/&/g, "&amp;").replace(/</g, "&lt;");
      if (line.startsWith("+")) return `<span class="diff-add">${esc}</span>`;
      if (line.startsWith("-")) return `<span class="diff-del">${esc}</span>`;
      if (line.startsWith("@@")) return `<span class="diff-hunk">${esc}</span>`;
      return esc;
    })
    .join("\n");
}

async function commit() {
  const msg = $("#commit-msg").value.trim();
  if (!msg) return toast("Enter a commit message", true);
  $("#commit-btn").disabled = true;
  try {
    await runOp("Committing…", "/api/commit", { path: state.active, message: msg });
    $("#commit-msg").value = "";
  } finally {
    $("#commit-btn").disabled = false;
  }
}

async function loadHistory(pathEnc) {
  try {
    const { revisions } = await apiGet(`/api/history?path=${pathEnc}&length=50`);
    state.revisions = revisions;
    // Skip the rebuild when nothing changed, so a background refresh (poll, focus,
    // file-watch) does not collapse a revision the user has expanded.
    const sig = revisions.map((r) => r.revision).join(",");
    if (sig === state.historySig) return;
    state.historySig = sig;
    const ul = $("#history-list");
    ul.innerHTML = "";
    for (const r of revisions) {
      const li = document.createElement("li");
      const when = r.timestamp ? new Date(r.timestamp).toLocaleString() : "";
      li.innerHTML = `
        <div class="h-row">
          <div class="h-msg">${(r.message || "(no message)").split("\n")[0]}</div>
          <div class="h-meta">
            <span class="h-rev">#${r.revisionNumber} · ${(r.revision || "").slice(0, 12)}</span>
            <span>${when}</span>
          </div>
        </div>
        <div class="rev-detail" hidden></div>`;
      li.querySelector(".h-row").onclick = () => toggleRevision(r, li);
      if (r.revision === state.openRevision) toggleRevision(r, li);
      ul.appendChild(li);
    }
  } catch (err) {
    toast(err.message, true);
  }
}

/** Expand a revision to show the files it changed; collapse if already open. */
async function toggleRevision(r, li) {
  const detail = li.querySelector(".rev-detail");
  if (!detail.hidden) {
    detail.hidden = true;
    li.classList.remove("open");
    state.openRevision = null;
    return;
  }
  detail.hidden = false;
  li.classList.add("open");
  state.openRevision = r.revision;
  detail.innerHTML = `<div class="muted">Loading changes…</div>`;
  const parent = (r.parent && r.parent[0]) || "";
  try {
    const { files } = await apiGet(
      `/api/revision?path=${encodeURIComponent(state.active)}&revision=${r.revision}`,
    );
    if (!files.length) {
      detail.innerHTML = `<div class="muted">No file changes in this revision.</div>`;
      return;
    }
    detail.innerHTML = `<ul class="rev-files"></ul><pre class="rev-diff" hidden></pre>`;
    const list = detail.querySelector(".rev-files");
    for (const f of files) {
      const [label, cls] = fileBadge(f);
      const item = document.createElement("li");
      item.innerHTML = `<span class="f-act ${cls}">${label}</span><span class="f-path" title="${f.path}">${f.path}</span>`;
      item.onclick = () => showRevisionFileDiff(r, parent, f.path, detail);
      list.appendChild(item);
    }
  } catch (err) {
    detail.innerHTML = `<div class="muted">${err.message}</div>`;
  }
}

/** Show one file's diff between a revision and its parent, inside the detail. */
async function showRevisionFileDiff(r, parent, file, detail) {
  const pre = detail.querySelector(".rev-diff");
  detail.querySelectorAll(".rev-files li").forEach((el) =>
    el.classList.toggle("sel", el.querySelector(".f-path")?.title === file),
  );
  pre.hidden = false;
  pre.textContent = "Loading diff…";
  try {
    const url =
      `/api/diff?path=${encodeURIComponent(state.active)}` +
      `&file=${encodeURIComponent(file)}&source=${parent}&target=${r.revision}`;
    const { diff } = await apiGet(url);
    const patch = diff.map((d) => d.patch || "").join("\n");
    pre.innerHTML = colorizeDiff(patch || "(no textual diff — binary file or no change)");
  } catch (err) {
    pre.textContent = err.message;
  }
}

async function loadBranches(pathEnc) {
  try {
    const { branches } = await apiGet(`/api/branches?path=${pathEnc}`);
    const ul = $("#branch-list");
    ul.innerHTML = "";
    const seen = new Set();
    for (const b of branches) {
      if (seen.has(b.name)) continue; // local + remote entries share a name
      seen.add(b.name);
      const li = document.createElement("li");
      li.innerHTML = `
        <span class="b-current">${b.isCurrent ? "●" : "○"}</span>
        <span class="b-name">${b.name}</span>
        <span class="b-loc">${(b.latest || "").slice(0, 12)}</span>`;
      ul.appendChild(li);
    }
  } catch (err) {
    toast(err.message, true);
  }
}

/** Format a byte count as a short human-readable string, for example "42.1 MB".
 * @param {number} n bytes to format
 * @returns {string} human-readable byte count
 */
function fmtBytes(n) {
  if (!n) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

const PROGRESS_BEGIN_TAGS = new Set(["REPOSITORY_CLONE_BEGIN", "REVISION_COMMIT_BEGIN"]);
const PROGRESS_TAGS = new Set(["REPOSITORY_CLONE_PROGRESS", "REVISION_COMMIT_PROGRESS"]);
const PROGRESS_END_TAGS = new Set(["REPOSITORY_CLONE_END", "REVISION_COMMIT_END"]);

/** Render progress data (file and byte counts) onto the operation overlay bar.
 * @param {HTMLElement} barFillEl progress bar fill element
 * @param {HTMLElement} textEl progress text element
 * @param {object} data progress payload with fileComplete, fileTotal, bytesTransferred, bytesTotal, discoveryComplete
 */
function renderOpProgress(barFillEl, textEl, data) {
  const fileDone = data.fileComplete ?? data.fileCount ?? 0;
  const fileTotal = data.fileTotal ?? data.fileCount ?? 0;
  const bytesDone = data.bytesTransferred ?? 0;
  const bytesTotal = data.bytesTotal ?? 0;
  const pct = data.discoveryComplete && bytesTotal > 0 ? (bytesDone / bytesTotal) * 100 : fileTotal > 0 ? (fileDone / fileTotal) * 100 : 0;
  barFillEl.style.width = `${Math.min(100, Math.max(0, pct))}%`;
  textEl.textContent = data.discoveryComplete
    ? `${fileDone.toLocaleString()} / ${fileTotal.toLocaleString()} files · ${fmtBytes(bytesDone)} / ${fmtBytes(bytesTotal)}`
    : "Discovering…";
}

async function runOp(title, path, payload) {
  const overlay = $("#op-overlay");
  const logEl = $("#op-log");
  const statusEl = $("#op-status");
  const closeBtn = $("#op-close");
  const progressEl = $("#op-progress");
  const barFillEl = $("#op-bar-fill");
  const progressTextEl = $("#op-progress-text");
  $("#op-title").textContent = title;
  logEl.textContent = "";
  statusEl.textContent = "";
  statusEl.className = "";
  closeBtn.hidden = true;
  progressEl.hidden = true;
  barFillEl.style.width = "0%";
  progressTextEl.textContent = "";
  overlay.hidden = false;

  try {
    await apiStream(path, payload, (ev) => {
      if (ev.tag === "LOG") logEl.textContent += (ev.data?.message || "") + "\n";
      else if (ev.tag === "DONE") {
        if (ev.data.ok) barFillEl.style.width = "100%";
        statusEl.textContent = ev.data.ok ? "Success" : `Failed: ${ev.data.message || "unknown error"}`;
        statusEl.className = ev.data.ok ? "ok" : "fail";
      } else if (PROGRESS_BEGIN_TAGS.has(ev.tag)) {
        progressEl.hidden = false;
        barFillEl.style.width = "0%";
        progressTextEl.textContent = "Starting…";
      } else if (PROGRESS_TAGS.has(ev.tag)) {
        progressEl.hidden = false;
        renderOpProgress(barFillEl, progressTextEl, ev.data || {});
      } else if (PROGRESS_END_TAGS.has(ev.tag)) {
        progressEl.hidden = false;
        barFillEl.style.width = "100%";
      } else if (ev.tag !== "END" && ev.tag !== "COMPLETE") {
        // Surface other progress-bearing events compactly.
        logEl.textContent += `• ${ev.tag}\n`;
      }
      logEl.scrollTop = logEl.scrollHeight;
    });
  } catch (err) {
    statusEl.textContent = `Failed: ${err.message}`;
    statusEl.className = "fail";
  }
  closeBtn.hidden = false;
  await refreshActive();
}

function connectSSE() {
  const es = new EventSource("/events");
  es.onopen = () => $("#conn").classList.add("live");
  es.onerror = () => $("#conn").classList.remove("live");
  es.onmessage = (e) => {
    let msg;
    try {
      msg = JSON.parse(e.data);
    } catch {
      return;
    }
    if (msg.type === "refresh") {
      if (msg.repo === "*") loadRepos();
      else if (msg.repo === state.active) refreshActive();
      else loadRepos(); // a non-active repo changed; keep the sidebar fresh
    }
  };
}

// The browser can't hand the server a real filesystem path, so the folder picker
// drives a server-backed directory browser (/api/browse) instead of typed paths.
const picker ={ cur: "", parent: null, sep: "\\", resolve: null };

async function pickerNavigate(path) {
  const data = await apiGet(`/api/browse?path=${encodeURIComponent(path ?? "")}`);
  picker.cur = data.path;
  picker.parent = data.parent;
  picker.sep = data.sep || picker.sep;
  $("#picker-cur").textContent = data.path || "This PC";
  $("#picker-up").disabled = data.parent === null;
  const ul = $("#picker-list");
  ul.innerHTML = "";
  if (data.entries.length === 0) {
    ul.innerHTML = `<li class="muted">— no sub-folders —</li>`;
  }
  for (const e of data.entries) {
    const li = document.createElement("li");
    li.innerHTML = `<span class="p-name" title="${e.path}">${e.name}</span>${
      e.isRepo ? `<span class="p-tag">lore</span>` : ""
    }`;
    li.onclick = () => pickerNavigate(e.path);
    ul.appendChild(li);
  }
}

/**
 * Open the folder picker and resolve to a chosen absolute path (or null on
 * cancel). With { allowNew: true } the user may type a new sub-folder name to
 * create under the browsed directory (used for a clone destination).
 */
function pickFolder({ title, allowNew } = {}) {
  $("#picker-title").textContent = title || "Select a folder";
  $("#picker-new").hidden = !allowNew;
  $("#picker-newname").value = "";
  pickerNavigate("").catch((err) => toast(err.message, true));
  $("#picker-dialog").showModal();
  return new Promise((resolve) => {
    picker.resolve = resolve;
  });
}

function pickerFinish(path) {
  $("#picker-dialog").close();
  const resolve = picker.resolve;
  picker.resolve = null;
  resolve?.(path);
}

function wirePicker() {
  $("#picker-up").onclick = () => picker.parent !== null && pickerNavigate(picker.parent);
  $("#picker-cancel").onclick = () => pickerFinish(null);
  $("#picker-choose").onclick = () => {
    if (!picker.cur) return toast("Open a folder first", true);
    const name = $("#picker-newname")?.value.trim();
    const path = !$("#picker-new").hidden && name ? picker.cur + picker.sep + name : picker.cur;
    pickerFinish(path);
  };
  $("#picker-dialog").addEventListener("cancel", (e) => {
    e.preventDefault();
    pickerFinish(null);
  });
}

/** Fetch the server's hosted repositories into the Server repositories dialog
 * (server URL from the field, or the default when blank). */
async function loadServerRepos() {
  const server = $("#server-url").value.trim();
  const data = await apiGet(`/api/remote-repos${server ? `?url=${encodeURIComponent(server)}` : ""}`);
  $("#server-url").value = data.base;
  renderServerRepos(data.repos);
}

/**
 * Render the repositories the server hosts. Each row offers Clone (hands the
 * remote URL to the clone dialog to pick a destination) and ✕ Delete (removes
 * it from the server by id — see the server's deleteRemoteRepo). Repos already
 * cloned on this machine are tagged so the real ones stand out from stale ones.
 */
function renderServerRepos(repos) {
  const ul = $("#server-repos");
  ul.innerHTML = "";
  ul.hidden = false;
  if (repos.length === 0) {
    ul.innerHTML = `<li class="muted">— no repositories on this server —</li>`;
    return;
  }
  for (const r of repos) {
    const li = document.createElement("li");
    const tag = r.tracked ? `<span class="p-tag">cloned</span>` : "";
    li.innerHTML =
      `<span class="p-name" title="${r.url}">${r.name}</span>${tag}` +
      `<button type="button" class="p-clone">Clone</button>` +
      `<button type="button" class="p-del" title="Delete from server">✕</button>`;
    li.querySelector(".p-clone").onclick = () => cloneServerRepo(r);
    li.querySelector(".p-del").onclick = () => deleteServerRepo(r);
    ul.appendChild(li);
  }
}

/** Start cloning a server repo: prefill and open the clone-from-URL dialog so
 * the user only has to choose a destination folder. */
function cloneServerRepo(r) {
  $("#server-dialog").close();
  $("#clone-url").value = r.url;
  $("#clone-dest").value = "";
  $("#clone-dialog").showModal();
}

/** Delete a server-side repository after confirmation, then refresh the list. */
async function deleteServerRepo(r) {
  const warn = r.tracked ? "\n\nThis is one of your local working copies — its files on disk are left untouched, but it will no longer exist on the server." : "";
  if (!confirm(`Delete "${r.name}" from the server? This cannot be undone.${warn}`)) return;
  try {
    await apiPost("/api/remote-repos", { _method: "DELETE", id: r.id, base: $("#server-url").value.trim() });
    toast(`Deleted ${r.name}`);
    await loadServerRepos();
  } catch (err) {
    toast(err.message, true);
  }
}

/** Load and display the current remote server configuration. */
async function loadConfig() {
  try {
    const data = await apiGet("/api/config");
    state.defaultRemote = data.defaultRemote || "";
    state.discoveredServers = data.discoveredServers || [];
    $("#settings-remote").value = state.defaultRemote;
    renderDiscoveredServers();
  } catch (err) {
    toast(err.message, true);
  }
}

/** Display the list of discovered servers in the settings dialog. */
function renderDiscoveredServers() {
  const container = $("#discovered-servers");
  const list = $("#discovered-list");
  if (state.discoveredServers.length === 0) {
    container.hidden = true;
    return;
  }
  container.hidden = false;
  list.innerHTML = "";
  for (const server of state.discoveredServers) {
    const li = document.createElement("li");
    li.textContent = server.url;
    li.onclick = () => {
      $("#settings-remote").value = server.url;
    };
    list.appendChild(li);
  }
}

/** Trigger manual discovery of Lore servers and update the list. */
async function discoverServers() {
  const btn = $("#settings-discover");
  btn.disabled = true;
  try {
    const data = await apiGet("/api/discover");
    state.discoveredServers = data.discoveredServers || [];
    if (state.discoveredServers.length === 0) {
      toast("No Lore servers found on the local network");
    } else {
      toast(`Found ${state.discoveredServers.length} server${state.discoveredServers.length === 1 ? "" : "s"}`);
    }
    renderDiscoveredServers();
  } catch (err) {
    toast(err.message, true);
  } finally {
    btn.disabled = false;
  }
}

/** Save the remote server configuration and refresh repos. */
async function saveConfig() {
  const url = $("#settings-remote").value.trim();
  try {
    await apiPost("/api/config", { defaultRemote: url });
    toast(url ? "Remote server configured" : "Remote server cleared");
    state.defaultRemote = url;
    await loadRepos();
  } catch (err) {
    toast(err.message, true);
  }
}

function wire() {
  $("#add-btn").onclick = addRepo;
  $("#refresh-btn").onclick = refreshActive;
  $("#commit-btn").onclick = commit;
  $("#stage-all-btn").onclick = stageAll;
  $("#ignore-cancel").onclick = () => $("#ignore-dialog").close();

  $("#sync-btn").onclick = () => runOp("Syncing…", "/api/sync", { path: state.active });
  $("#push-btn").onclick = () => runOp("Pushing…", "/api/push", { path: state.active });
  $("#op-close").onclick = () => ($("#op-overlay").hidden = true);

  // Server repositories: open the dialog and list immediately (it falls back to
  // the default server when the field is blank, so the catalog shows at once).
  $("#server-btn").onclick = async () => {
    $("#server-repos").hidden = true;
    $("#server-dialog").showModal();
    try {
      await loadServerRepos();
    } catch (err) {
      toast(err.message, true);
    }
  };
  $("#server-refresh").onclick = async () => {
    const btn = $("#server-refresh");
    btn.disabled = true;
    try {
      await loadServerRepos();
    } catch (err) {
      toast(err.message, true);
    } finally {
      btn.disabled = false;
    }
  };

  $("#clone-btn").onclick = () => $("#clone-dialog").showModal();
  $("#clone-dest-browse").onclick = async () => {
    const dest = await pickFolder({ title: "Clone destination", allowNew: true });
    if (dest) $("#clone-dest").value = dest;
  };
  $("#clone-go").onclick = (e) => {
    const url = $("#clone-url").value.trim();
    const dest = $("#clone-dest").value.trim();
    if (!url || !dest) {
      e.preventDefault();
      return toast("URL and destination required", true);
    }
    setTimeout(async () => {
      await runOp("Cloning…", "/api/clone", { url, dest });
      await loadRepos();
    }, 0);
  };

  $("#repo-org").onclick = () => {
    if (!state.active) return;
    $("#org-repo").textContent = state.org?.repoName
      ? `Repository: ${state.org.repoName}`
      : "";
    $("#org-name").value = state.org?.organization || "";
    $("#org-dialog").showModal();
    $("#org-name").focus();
  };
  $("#org-go").onclick = (e) => {
    const organization = $("#org-name").value.trim();
    if (!organization || organization.includes("/")) {
      e.preventDefault();
      return toast("Organization is required and cannot contain '/'", true);
    }
    const path = state.active;
    setTimeout(async () => {
      try {
        await apiPost("/api/org", { path, organization });
        toast("Organization changed — repository rebuilt");
        if (state.active === path) loadOrg(path);
        await loadRepos();
      } catch (err) {
        toast(err.message, true);
      }
    }, 0);
  };

  // Settings dialog
  $("#settings-btn").onclick = async () => {
    await loadConfig();
    $("#settings-dialog").showModal();
  };
  $("#settings-discover").onclick = () => discoverServers();
  $("#settings-go").onclick = () => {
    saveConfig();
    $("#settings-dialog").close();
  };
  $("#settings-dialog").addEventListener("cancel", (e) => {
    e.preventDefault();
    $("#settings-dialog").close();
  });

  wirePicker();
  wireInit();

  $$(".tab").forEach((tab) => {
    tab.onclick = () => {
      state.tab = tab.dataset.tab;
      $$(".tab").forEach((t) => t.classList.toggle("active", t === tab));
      $$(".panel").forEach((pnl) => pnl.classList.toggle("active", pnl.dataset.panel === state.tab));
    };
  });

  // Freshness: refetch when the window regains focus.
  window.addEventListener("focus", () => {
    loadRepos();
    refreshActive();
  });
  // Slow poll catches revisions pushed by the other machine (no local fs event).
  setInterval(() => state.active && loadHistory(encodeURIComponent(state.active)), 10000);
}

wire();
connectSSE();
loadRepos();
