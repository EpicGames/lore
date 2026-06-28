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
};

// ---- API helpers ---------------------------------------------------------

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

// ---- Repo list -----------------------------------------------------------

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
      ${r.exists ? `<span class="r-branch">${r.branch || ""}</span>` : `<span class="r-missing">missing</span>`}
      <button class="r-remove" title="Remove">✕</button>`;
    li.querySelector(".r-name").onclick = () => selectRepo(r.path);
    li.querySelector(".r-branch, .r-missing")?.addEventListener?.("click", () => selectRepo(r.path));
    li.querySelector(".r-remove").onclick = (e) => {
      e.stopPropagation();
      removeRepo(r.path);
    };
    ul.appendChild(li);
  }
}

async function addRepo() {
  const input = $("#add-path");
  const path = input.value.trim();
  if (!path) return;
  try {
    await apiPost("/api/repos", { path, label: path.split(/[\\/]/).pop() });
    input.value = "";
    await loadRepos();
    selectRepo(path);
  } catch (err) {
    toast(err.message, true);
  }
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

// ---- Active repo views ---------------------------------------------------

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
  await refreshActive();
}

/** Refetch every view for the active repo. The single source of freshness. */
async function refreshActive() {
  if (!state.active) return;
  const path = encodeURIComponent(state.active);
  await Promise.all([loadStatus(path), loadHistory(path), loadBranches(path)]);
}

function fileBadge(f) {
  if (f.action === 1) return ["A", "badge-A"];
  if (f.action === 2) return ["D", "badge-D"];
  if (f.action === 3) return ["R", "badge-M"];
  return ["M", "badge-M"];
}

async function loadStatus(pathEnc) {
  try {
    const data = await apiGet(`/api/status?path=${pathEnc}`);
    $("#repo-branch").textContent = data.branch || "";
    const staged = data.files.filter((f) => f.flagStaged);
    const unstaged = data.files.filter((f) => !f.flagStaged);
    renderFiles($("#staged-files"), staged, "unstage");
    renderFiles($("#unstaged-files"), unstaged, "stage");
    $("#commit-btn").disabled = staged.length === 0;
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
      ${action === "stage" ? `<button class="f-reset" title="Discard changes">↺</button>` : ""}`;
    li.querySelector(".f-path").onclick = () => showDiff(f.path);
    li.querySelector(".f-do").onclick = () => fileAction(action, f.path);
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
    await apiPost("/api/commit", { path: state.active, message: msg });
    $("#commit-msg").value = "";
    toast("Committed");
    await refreshActive();
  } catch (err) {
    toast(err.message, true);
  } finally {
    $("#commit-btn").disabled = false;
  }
}

async function loadHistory(pathEnc) {
  try {
    const { revisions } = await apiGet(`/api/history?path=${pathEnc}&length=50`);
    const ul = $("#history-list");
    ul.innerHTML = "";
    for (const r of revisions) {
      const li = document.createElement("li");
      const when = r.timestamp ? new Date(r.timestamp).toLocaleString() : "";
      li.innerHTML = `
        <div class="h-msg">${(r.message || "(no message)").split("\n")[0]}</div>
        <div class="h-meta">
          <span class="h-rev">#${r.revisionNumber} · ${(r.revision || "").slice(0, 12)}</span>
          <span>${when}</span>
        </div>`;
      ul.appendChild(li);
    }
  } catch (err) {
    toast(err.message, true);
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

// ---- Remote operations (streamed) ---------------------------------------

async function runOp(title, path, payload) {
  const overlay = $("#op-overlay");
  const logEl = $("#op-log");
  const statusEl = $("#op-status");
  const closeBtn = $("#op-close");
  $("#op-title").textContent = title;
  logEl.textContent = "";
  statusEl.textContent = "";
  statusEl.className = "";
  closeBtn.hidden = true;
  overlay.hidden = false;

  try {
    await apiStream(path, payload, (ev) => {
      if (ev.tag === "LOG") logEl.textContent += (ev.data?.message || "") + "\n";
      else if (ev.tag === "DONE") {
        statusEl.textContent = ev.data.ok ? "Success" : `Failed: ${ev.data.message || "unknown error"}`;
        statusEl.className = ev.data.ok ? "ok" : "fail";
      } else if (ev.tag !== "END" && ev.tag !== "COMPLETE") {
        // Surface progress-bearing events compactly.
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

// ---- Live connection (SSE) ----------------------------------------------

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

// ---- Wiring --------------------------------------------------------------

function wire() {
  $("#add-btn").onclick = addRepo;
  $("#add-path").addEventListener("keydown", (e) => e.key === "Enter" && addRepo());
  $("#refresh-btn").onclick = refreshActive;
  $("#commit-btn").onclick = commit;

  $("#sync-btn").onclick = () => runOp("Syncing…", "/api/sync", { path: state.active });
  $("#push-btn").onclick = () => runOp("Pushing…", "/api/push", { path: state.active });
  $("#op-close").onclick = () => ($("#op-overlay").hidden = true);

  $("#clone-btn").onclick = () => $("#clone-dialog").showModal();
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

// ---- Boot ----------------------------------------------------------------

wire();
connectSSE();
loadRepos();
