#!/usr/bin/env node
// Independent heartbeat checker — the "external" half of the heartbeat contract.
// Usage: node status-check.mjs <status.json>
// Reads a snapshot file and prints the verdict an independent host would render:
//   alive      — snapshot exists and is inside its declared TTL
//   unavailable — missing, unparseable, or expired: monitoring is down, say so
// Exits 0 for alive, 1 for unavailable. Never converts absence into "targets down".

import { readFileSync } from 'node:fs';

const path = process.argv[2] ?? 'status/status.json';

let snap;
try {
	snap = JSON.parse(readFileSync(path, 'utf8'));
} catch (error) {
	console.log(`unavailable: no readable snapshot at ${path} (${error.code ?? error.message})`);
	process.exit(1);
}

const expires = Date.parse(snap.expires_at ?? '');
if (Number.isNaN(expires)) {
	console.log('unavailable: snapshot has no valid expires_at');
	process.exit(1);
}
if (expires < Date.now()) {
	const ageS = Math.round((Date.now() - Date.parse(snap.generated_at)) / 1000);
	console.log(`unavailable: heartbeat expired (snapshot is ${ageS}s old, ttl ended)`);
	process.exit(1);
}

const services = snap.services ?? [];
const counts = { up: 0, down: 0, unknown: 0 };
for (const s of services) counts[s.status] = (counts[s.status] ?? 0) + 1;
console.log(
	`alive: ${services.length} services — up ${counts.up ?? 0}, down ${counts.down ?? 0}, ` +
		`unknown ${counts.unknown ?? 0} (snapshot ${Math.round((Date.now() - Date.parse(snap.generated_at)) / 1000)}s old)`
);
