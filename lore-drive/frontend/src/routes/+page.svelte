<script>
	import { api } from '$lib/api.js';

	// ── State (Svelte 5 runes) ────────────────────────────────────────────
	let info = $state(null);
	let revision = $state('');
	// Breadcrumb trail: [{ node_id, name }], index 0 is the root.
	let trail = $state([{ node_id: 0, name: '/' }]);
	let children = $state([]);
	let loading = $state(true);
	let errorMsg = $state('');
	let menuFor = $state(null); // node_id whose ⋮ menu is open
	let renaming = $state(null); // { node_id, value }
	let newFolderOpen = $state(false);
	let newFolderName = $state('');
	let dragDepth = $state(0);
	// Conflict modal: { files: [{relPath,file}], conflicts: [paths], selected: Set }
	let conflict = $state(null);
	let busy = $state(false);

	const here = $derived(trail[trail.length - 1]);
	const visible = $derived(children.filter((c) => c.name !== '.lorekeep'));

	// ── Loading ───────────────────────────────────────────────────────────
	async function refresh() {
		loading = true;
		errorMsg = '';
		try {
			const [i, t] = await Promise.all([api.info(), api.tree(here.node_id)]);
			info = i;
			// The revision is a change-tag: it may legitimately stay UNCHANGED
			// on idempotent no-ops. Node ids belong to the revision they were
			// listed under; after every mutation we re-list, never reuse stale ids.
			revision = t.revision;
			children = t.children;
		} catch (e) {
			errorMsg = e.message;
			children = [];
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		// re-run whenever the current directory changes
		here.node_id;
		refresh();
	});

	function enter(node) {
		if (node.kind !== 'directory') return;
		trail = [...trail, { node_id: node.node_id, name: node.name }];
	}

	function jump(index) {
		trail = trail.slice(0, index + 1);
	}

	// ── Mutations ─────────────────────────────────────────────────────────
	async function createFolder() {
		const name = newFolderName.trim();
		if (!name) return;
		busy = true;
		try {
			await api.mkdir(here.node_id, name);
			newFolderOpen = false;
			newFolderName = '';
			await refresh();
		} catch (e) {
			errorMsg = e.message;
		} finally {
			busy = false;
		}
	}

	async function commitRename() {
		if (!renaming) return;
		const { node_id, value } = renaming;
		const name = value.trim();
		renaming = null;
		if (!name) return;
		busy = true;
		try {
			await api.rename(node_id, name);
			await refresh();
		} catch (e) {
			errorMsg = e.message;
		} finally {
			busy = false;
		}
	}

	async function remove(node) {
		menuFor = null;
		if (!confirm(`Delete ${node.name}?`)) return;
		busy = true;
		try {
			await api.remove(node.node_id);
			await refresh();
		} catch (e) {
			errorMsg = e.message;
		} finally {
			busy = false;
		}
	}

	function download(node) {
		menuFor = null;
		// Folder downloads arrive as a ZIP; file downloads as raw bytes whose
		// b3 hash matches the displayed address.
		window.open(api.downloadUrl(node.node_id), '_blank');
	}

	// ── Upload (buttons + drag'n'drop of files and folders) ──────────────
	async function sendUpload(files, overwrite = false) {
		if (!files.length) return;
		busy = true;
		errorMsg = '';
		try {
			await api.upload(here.node_id, files, overwrite);
			await refresh();
		} catch (e) {
			if (e.status === 409 && e.body?.conflicts) {
				conflict = {
					files,
					conflicts: e.body.conflicts,
					selected: new Set(e.body.conflicts)
				};
			} else {
				errorMsg = e.message;
			}
		} finally {
			busy = false;
		}
	}

	function pickFiles() {
		const input = document.createElement('input');
		input.type = 'file';
		input.multiple = true;
		input.onchange = () =>
			sendUpload([...input.files].map((f) => ({ relPath: f.name, file: f })));
		input.click();
	}

	function pickFolder() {
		const input = document.createElement('input');
		input.type = 'file';
		input.webkitdirectory = true;
		input.onchange = () =>
			sendUpload(
				[...input.files].map((f) => ({
					relPath: f.webkitRelativePath || f.name,
					file: f
				}))
			);
		input.click();
	}

	// FileSystemEntry traversal so dropped *folders* upload with their
	// relative paths intact (the multipart part filename carries the path).
	function walkEntry(entry, prefix) {
		return new Promise((resolve) => {
			if (entry.isFile) {
				entry.file(
					(file) => resolve([{ relPath: prefix + entry.name, file }]),
					() => resolve([])
				);
			} else if (entry.isDirectory) {
				const reader = entry.createReader();
				const acc = [];
				const readBatch = () =>
					reader.readEntries(async (entries) => {
						if (!entries.length) return resolve(acc);
						for (const e of entries) {
							acc.push(...(await walkEntry(e, prefix + entry.name + '/')));
						}
						readBatch(); // readEntries returns results in batches
					});
				readBatch();
			} else {
				resolve([]);
			}
		});
	}

	async function onDrop(event) {
		event.preventDefault();
		dragDepth = 0;
		const items = [...(event.dataTransfer?.items ?? [])];
		const collected = [];
		for (const item of items) {
			const entry = item.webkitGetAsEntry?.();
			if (entry) {
				collected.push(...(await walkEntry(entry, '')));
			} else {
				const f = item.getAsFile?.();
				if (f) collected.push({ relPath: f.name, file: f });
			}
		}
		sendUpload(collected);
	}

	// ── Conflict modal actions ────────────────────────────────────────────
	// "all": re-send everything with overwrite=true.
	// "replace": re-send the selected conflicting files with overwrite=true,
	//            plus the non-conflicting ones without it.
	// "abort": cancel.
	function conflictPathOf(relPath) {
		// The backend reports absolute virtual paths; rebuild ours the same way.
		const base = trail
			.slice(1)
			.map((t) => t.name)
			.join('/');
		return '/' + (base ? base + '/' : '') + relPath;
	}

	async function conflictAll() {
		const { files } = conflict;
		conflict = null;
		await sendUpload(files, true);
	}

	async function conflictReplaceSelected() {
		const { files, selected, conflicts } = conflict;
		conflict = null;
		const pathOf = (f) => conflictPathOf(f.relPath);
		// selected conflicting files → re-sent with overwrite=true;
		// non-conflicting files → re-sent without it;
		// conflicting-but-unselected files → dropped.
		const withOverwrite = files.filter((f) => selected.has(pathOf(f)));
		const without = files.filter((f) => !conflicts.includes(pathOf(f)));
		if (withOverwrite.length) await sendUpload(withOverwrite, true);
		if (without.length) await sendUpload(without, false);
	}

	function toggleSelected(path) {
		const next = new Set(conflict.selected);
		if (next.has(path)) next.delete(path);
		else next.add(path);
		conflict = { ...conflict, selected: next };
	}

	// ── Display helpers ───────────────────────────────────────────────────
	function fmtSize(n) {
		if (n < 1024) return `${n} B`;
		if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KiB`;
		if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MiB`;
		return `${(n / 1024 ** 3).toFixed(1)} GiB`;
	}

	// Content fingerprint: a deterministic color derived from the first bytes
	// of the b3 hash — the same content always shows the same swatch, a
	// visual restatement of "displayed ids are 1-to-1 with the CAS".
	function fingerprint(address) {
		if (!address) return 'transparent';
		const hue = parseInt(address.slice(0, 4), 16) % 360;
		const sat = 45 + (parseInt(address.slice(4, 6), 16) % 30);
		return `hsl(${hue} ${sat}% 42%)`;
	}

	function splitAddress(address) {
		// address = `<b3-hash>-<file-id>`; keep it verbatim but let the UI
		// break it visually at the separator.
		const i = address?.lastIndexOf('-') ?? -1;
		return i > 0 ? [address.slice(0, i), address.slice(i + 1)] : [address, ''];
	}

	function closeMenus() {
		menuFor = null;
	}
</script>

<svelte:window onclick={closeMenus} />

<div
	class="app"
	role="region"
	aria-label="File browser and drop zone"
	ondragenter={(e) => {
		e.preventDefault();
		dragDepth += 1;
	}}
	ondragleave={() => (dragDepth = Math.max(0, dragDepth - 1))}
	ondragover={(e) => e.preventDefault()}
	ondrop={onDrop}
>
	<header>
		<div class="brand">
			<span class="glyph">⟠</span>
			<h1>lore-drive</h1>
			{#if info}
				<span class="mode" class:versioned={info.mode === 'versioned'}>{info.mode}</span>
			{/if}
		</div>
		{#if info}
			<div class="workspace mono">
				<span title="repository id">{info.repository_id}</span>
				<span class="dim">·</span>
				<span title="branch">{info.branch_name}</span>
				<span class="dim">·</span>
				<span title="served revision (change-tag)">{revision.slice(0, 12)}…</span>
			</div>
		{/if}
	</header>

	<nav class="crumbs" aria-label="Breadcrumb">
		{#each trail as part, i}
			{#if i > 0}<span class="dim">/</span>{/if}
			<button class="crumb" disabled={i === trail.length - 1} onclick={() => jump(i)}>
				{i === 0 ? '⌂' : part.name}
			</button>
		{/each}
		<div class="spacer"></div>
		<button class="action" onclick={() => (newFolderOpen = true)} disabled={busy}>
			+ Create folder
		</button>
		<button class="action" onclick={pickFiles} disabled={busy}>↑ Upload files</button>
		<button class="action" onclick={pickFolder} disabled={busy}>↑ Upload folder</button>
	</nav>

	{#if errorMsg}
		<div class="error" role="alert">
			{errorMsg}
			<button class="dismiss" onclick={() => (errorMsg = '')}>×</button>
		</div>
	{/if}

	<main>
		{#if loading}
			<p class="hint">Loading…</p>
		{:else if visible.length === 0}
			<p class="hint">This folder is empty — drop files or folders anywhere to upload.</p>
		{:else}
			<ul class="cards">
				{#each visible as node (node.node_id)}
					<li class="card" class:dir={node.kind === 'directory'}>
						{#if node.kind === 'file'}
							<span
								class="swatch"
								style:background={fingerprint(node.address)}
								title="content fingerprint (derived from the b3 hash)"
							></span>
						{:else}
							<span class="swatch dirsw">📁</span>
						{/if}

						<div class="body">
							{#if renaming?.node_id === node.node_id}
								<!-- svelte-ignore a11y_autofocus -->
								<input
									class="rename mono"
									autofocus
									bind:value={renaming.value}
									onkeydown={(e) => {
										if (e.key === 'Enter') commitRename();
										if (e.key === 'Escape') renaming = null;
									}}
									onblur={commitRename}
								/>
							{:else if node.kind === 'directory'}
								<button class="name asdir" onclick={() => enter(node)}>{node.name}</button>
							{:else}
								<span class="name">{node.name}</span>
							{/if}

							<div class="meta mono">
								<span title="node id">#{node.node_id}</span>
								{#if node.kind === 'file'}
									<span class="dim">·</span>
									<span title="size">{fmtSize(node.size)}</span>
								{/if}
							</div>

							{#if node.kind === 'file' && node.address}
								{@const [hash, fileId] = splitAddress(node.address)}
								<div class="addr mono" title="address: <b3-hash>-<file-id>, exactly as stored in the CAS">
									<span class="hash">{hash}</span><span class="dim">-</span><span class="fid">{fileId}</span>
								</div>
							{/if}
						</div>

						<div class="menuwrap">
							<button
								class="burger"
								aria-label="Actions for {node.name}"
								onclick={(e) => {
									e.stopPropagation();
									menuFor = menuFor === node.node_id ? null : node.node_id;
								}}
							>
								⋮
							</button>
							{#if menuFor === node.node_id}
								<div class="menu" role="menu">
									<button
										role="menuitem"
										onclick={(e) => {
											e.stopPropagation();
											menuFor = null;
											renaming = { node_id: node.node_id, value: node.name };
										}}
									>
										Rename
									</button>
									<button role="menuitem" onclick={() => download(node)}>
										Download{node.kind === 'directory' ? ' as ZIP' : ''}
									</button>
									<button role="menuitem" class="danger" onclick={() => remove(node)}>
										Delete
									</button>
								</div>
							{/if}
						</div>
					</li>
				{/each}
			</ul>
		{/if}
	</main>

	{#if dragDepth > 0}
		<div class="dropveil">
			<div class="dropcard">Drop to upload into <strong>{here.name}</strong></div>
		</div>
	{/if}

	{#if newFolderOpen}
		<div class="veil" role="dialog" aria-label="Create folder">
			<div class="modal">
				<h2>Create folder</h2>
				<!-- svelte-ignore a11y_autofocus -->
				<input
					class="mono"
					autofocus
					placeholder="folder name"
					bind:value={newFolderName}
					onkeydown={(e) => {
						if (e.key === 'Enter') createFolder();
						if (e.key === 'Escape') newFolderOpen = false;
					}}
				/>
				<div class="row">
					<button class="action" onclick={() => (newFolderOpen = false)}>Cancel</button>
					<button class="action primary" onclick={createFolder} disabled={!newFolderName.trim()}>
						Create
					</button>
				</div>
			</div>
		</div>
	{/if}

	{#if conflict}
		<div class="veil" role="dialog" aria-label="Upload conflicts">
			<div class="modal">
				<h2>{conflict.conflicts.length} path(s) already exist</h2>
				<p class="hint">
					Choose which existing files to replace, replace all of them, or abort the upload.
				</p>
				<ul class="conflictlist mono">
					{#each conflict.conflicts as path}
						<li>
							<label>
								<input
									type="checkbox"
									checked={conflict.selected.has(path)}
									onchange={() => toggleSelected(path)}
								/>
								{path}
							</label>
						</li>
					{/each}
				</ul>
				<div class="row">
					<button class="action" onclick={() => (conflict = null)}>Abort</button>
					<button
						class="action"
						onclick={conflictReplaceSelected}
						disabled={conflict.selected.size === 0}
					>
						Replace selected
					</button>
					<button class="action primary" onclick={conflictAll}>Replace all</button>
				</div>
			</div>
		</div>
	{/if}
</div>

<style>
	:global(:root) {
		--bg: #f4f6f8;
		--panel: #ffffff;
		--ink: #171c22;
		--dim: #7b8794;
		--line: #dde3e9;
		--accent: #14635a;
		--accent-ink: #ffffff;
		--danger: #b3362b;
		--mono: 'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
		--sans: 'Inter', system-ui, -apple-system, 'Segoe UI', sans-serif;
	}
	:global(body) {
		margin: 0;
		background: var(--bg);
		color: var(--ink);
		font-family: var(--sans);
		font-size: 14px;
	}
	.mono {
		font-family: var(--mono);
	}
	.dim {
		color: var(--dim);
	}

	.app {
		max-width: 980px;
		margin: 0 auto;
		padding: 20px 20px 60px;
		min-height: 100vh;
		box-sizing: border-box;
	}

	header {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 12px;
		padding-bottom: 14px;
		border-bottom: 1px solid var(--line);
	}
	.brand {
		display: flex;
		align-items: baseline;
		gap: 8px;
	}
	.glyph {
		color: var(--accent);
		font-size: 20px;
	}
	h1 {
		font-size: 18px;
		font-weight: 650;
		letter-spacing: 0.01em;
		margin: 0;
	}
	.mode {
		font-family: var(--mono);
		font-size: 11px;
		padding: 2px 7px;
		border-radius: 999px;
		background: var(--accent);
		color: var(--accent-ink);
	}
	.mode.versioned {
		background: #4a3f9f;
	}
	.workspace {
		margin-left: auto;
		font-size: 11px;
		color: var(--dim);
		display: flex;
		gap: 6px;
	}

	.crumbs {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 12px 0;
		flex-wrap: wrap;
	}
	.crumb {
		background: none;
		border: none;
		font: inherit;
		color: var(--accent);
		cursor: pointer;
		padding: 2px 4px;
		border-radius: 4px;
	}
	.crumb:disabled {
		color: var(--ink);
		font-weight: 600;
		cursor: default;
	}
	.crumb:not(:disabled):hover {
		background: #e6efed;
	}
	.spacer {
		flex: 1;
	}
	.action {
		font: inherit;
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: 7px;
		padding: 6px 12px;
		cursor: pointer;
	}
	.action:hover:not(:disabled) {
		border-color: var(--accent);
	}
	.action:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.action.primary {
		background: var(--accent);
		border-color: var(--accent);
		color: var(--accent-ink);
	}

	.error {
		background: #fbeae8;
		border: 1px solid #eecbc6;
		color: var(--danger);
		border-radius: 8px;
		padding: 10px 12px;
		margin: 8px 0;
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.dismiss {
		margin-left: auto;
		border: none;
		background: none;
		color: inherit;
		font-size: 16px;
		cursor: pointer;
	}

	.hint {
		color: var(--dim);
		padding: 28px 4px;
	}

	.cards {
		list-style: none;
		margin: 8px 0 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.card {
		display: flex;
		align-items: center;
		gap: 14px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 10px;
		padding: 12px 14px;
	}
	.card:hover {
		border-color: #c4ced7;
	}
	.swatch {
		flex: none;
		width: 14px;
		height: 42px;
		border-radius: 4px;
	}
	.swatch.dirsw {
		width: auto;
		height: auto;
		font-size: 22px;
		background: none;
	}
	.body {
		min-width: 0;
		flex: 1;
	}
	.name {
		font-weight: 600;
		font-size: 14.5px;
		word-break: break-all;
	}
	.name.asdir {
		background: none;
		border: none;
		padding: 0;
		font: inherit;
		font-weight: 600;
		color: var(--accent);
		cursor: pointer;
	}
	.name.asdir:hover {
		text-decoration: underline;
	}
	.meta {
		font-size: 11.5px;
		color: var(--dim);
		margin-top: 2px;
		display: flex;
		gap: 6px;
	}
	.addr {
		font-size: 11px;
		margin-top: 4px;
		word-break: break-all;
		line-height: 1.5;
	}
	.addr .hash {
		color: var(--accent);
	}
	.addr .fid {
		color: var(--dim);
	}
	.rename {
		font-size: 13px;
		padding: 4px 6px;
		border: 1px solid var(--accent);
		border-radius: 5px;
		width: 100%;
		box-sizing: border-box;
	}

	.menuwrap {
		position: relative;
		flex: none;
	}
	.burger {
		border: none;
		background: none;
		font-size: 18px;
		color: var(--dim);
		cursor: pointer;
		padding: 6px 10px;
		border-radius: 6px;
	}
	.burger:hover {
		background: var(--bg);
		color: var(--ink);
	}
	.menu {
		position: absolute;
		right: 0;
		top: 100%;
		z-index: 10;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 8px;
		box-shadow: 0 8px 24px rgba(23, 28, 34, 0.12);
		min-width: 160px;
		padding: 4px;
		display: flex;
		flex-direction: column;
	}
	.menu button {
		text-align: left;
		background: none;
		border: none;
		font: inherit;
		padding: 8px 10px;
		border-radius: 6px;
		cursor: pointer;
	}
	.menu button:hover {
		background: var(--bg);
	}
	.menu .danger {
		color: var(--danger);
	}

	.veil,
	.dropveil {
		position: fixed;
		inset: 0;
		background: rgba(23, 28, 34, 0.35);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 50;
	}
	.dropveil {
		pointer-events: none;
		background: rgba(20, 99, 90, 0.14);
		border: 3px dashed var(--accent);
		box-sizing: border-box;
	}
	.dropcard {
		background: var(--panel);
		border-radius: 10px;
		padding: 18px 26px;
		font-size: 16px;
		box-shadow: 0 8px 24px rgba(23, 28, 34, 0.18);
	}
	.modal {
		background: var(--panel);
		border-radius: 12px;
		padding: 20px 22px;
		width: min(480px, calc(100vw - 40px));
		box-shadow: 0 12px 40px rgba(23, 28, 34, 0.25);
	}
	.modal h2 {
		margin: 0 0 8px;
		font-size: 16px;
	}
	.modal input[type='text'],
	.modal input:not([type]) {
		width: 100%;
		box-sizing: border-box;
		padding: 8px 10px;
		border: 1px solid var(--line);
		border-radius: 7px;
		font-size: 13px;
		margin: 8px 0;
	}
	.row {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		margin-top: 12px;
	}
	.conflictlist {
		list-style: none;
		margin: 8px 0;
		padding: 0;
		max-height: 200px;
		overflow: auto;
		font-size: 12px;
	}
	.conflictlist li {
		padding: 4px 0;
	}
	.conflictlist label {
		display: flex;
		gap: 8px;
		align-items: center;
		word-break: break-all;
	}

	@media (prefers-reduced-motion: no-preference) {
		.card {
			transition: border-color 120ms ease;
		}
	}
</style>
