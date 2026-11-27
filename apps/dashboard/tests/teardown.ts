import * as fs from 'node:fs';

// Removes what the run created under /tmp (playwright.config.ts lists it). Only paths
// with this suite's own prefix are touched, whatever the variable holds.
export default function teardown(): void {
	for (const path of (process.env['SMON_E2E_SCRATCH'] ?? '').split('\n')) {
		if (path.startsWith('/tmp/smon-e2e-')) fs.rmSync(path, { recursive: true, force: true });
	}
}
