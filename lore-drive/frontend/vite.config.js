import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [sveltekit()],
	server: {
		// Dev-time proxy so the app can call /api/... same-origin while
		// lore-drive listens on :8080 (CORS is permissive server-side too).
		proxy: {
			'/api': 'http://localhost:8080'
		}
	}
});
