// Thin client for the lore-drive REST API (see ../REST_API.md).
// All identifiers (node_id, address = `<b3-hash>-<file-id>`, revision) are
// passed through exactly as returned by the backend — 1-to-1 with the CAS.

const BASE = import.meta.env.VITE_API_BASE ?? '';

async function request(path, options = {}) {
	const res = await fetch(`${BASE}/api/v1${path}`, options);
	const text = await res.text();
	let body = null;
	try {
		body = text ? JSON.parse(text) : null;
	} catch {
		body = { error: text };
	}
	if (!res.ok) {
		const err = new Error(body?.error ?? `HTTP ${res.status}`);
		err.status = res.status;
		err.body = body;
		throw err;
	}
	return body;
}

export const api = {
	info: () => request('/info'),

	tree: (nodeId) =>
		request(nodeId ? `/tree?node_id=${nodeId}` : '/tree'),

	node: (nodeId) => request(`/node/${nodeId}`),

	mkdir: (parentId, name) =>
		request('/mkdir', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ parent_id: parentId, name })
		}),

	// files: [{ relPath, file }] — relPath may contain '/' for folder uploads
	// (the part filename carries the relative path).
	upload: (parentId, files, overwrite = false) => {
		const form = new FormData();
		for (const { relPath, file } of files) {
			form.append('file', file, relPath);
		}
		const qs = new URLSearchParams({ parent_id: String(parentId) });
		if (overwrite) qs.set('overwrite', 'true');
		return request(`/upload?${qs}`, { method: 'POST', body: form });
	},

	rename: (nodeId, name) =>
		request(`/node/${nodeId}`, {
			method: 'PATCH',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ name })
		}),

	remove: (nodeId) => request(`/node/${nodeId}`, { method: 'DELETE' }),

	downloadUrl: (nodeId) => `${BASE}/api/v1/download/${nodeId}`,

	// ── Custom user properties (key/value strings per node) ──
	properties: (nodeId) => request(`/node/${nodeId}/properties`),

	setProperty: (nodeId, key, value) =>
		request(`/node/${nodeId}/properties`, {
			method: 'PUT',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ key, value })
		}),

	deleteProperty: (nodeId, key) =>
		request(`/node/${nodeId}/properties/${encodeURIComponent(key)}`, {
			method: 'DELETE'
		}),

	// ── Search names and property keys/values ──
	search: (q, limit) => {
		const qs = new URLSearchParams({ q });
		if (limit) qs.set('limit', String(limit));
		return request(`/search?${qs}`);
	}
};
