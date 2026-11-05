import adapter from '@sveltejs/adapter-static';
import tailwindcss from '@tailwindcss/vite';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// Where the built dashboard goes: `build`, which monitor-server serves on its own origin
// (SMON_DASHBOARD_DIR); DASHBOARD_BUILD_DIR builds elsewhere so tests can run against new work.
const out = process.env['DASHBOARD_BUILD_DIR'] ?? 'build';

export default defineConfig({
	plugins: [
		// Tailwind CSS v4 before SvelteKit, as its docs require.
		tailwindcss(),
		sveltekit({
			compilerOptions: {
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},
			// A static single-page app that monitor-server serves at the root on its own origin.
			// Absolute asset paths, because the fallback page answers for every route.
			adapter: adapter({ pages: out, assets: out, fallback: '200.html' })
		})
	],
	server: {
		host: '127.0.0.1',
		port: 58600,
		strictPort: true,
		// In development, the API is a monitor-server on its default port (58400).
		proxy: Object.fromEntries(
			['/api', '/livez', '/readyz'].map((path) => [path, 'http://127.0.0.1:58400'])
		)
	},
	preview: { host: '127.0.0.1', port: 58600, strictPort: true }
});
