import adapter from '@sveltejs/adapter-static';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	kit: {
		// SPA build: the backend is a pure REST API, everything renders client-side.
		adapter: adapter({ fallback: 'index.html' })
	}
};

export default config;
