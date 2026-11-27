import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig, mergeConfig } from 'vitest/config';
import viteConfig from './vite.config.ts';

// Unit and component tests: the dashboard's own logic and components, run in jsdom
// with the same Vite and Svelte setup the build uses. The journeys that need a real
// server and a real browser are Playwright's (tests/, scripts/e2e.sh).
export default mergeConfig(
	viteConfig,
	defineConfig({
		plugins: [svelteTesting()],
		test: {
			environment: 'jsdom',
			include: ['src/**/*.test.ts'],
			restoreMocks: true,
			unstubGlobals: true
		}
	})
);
