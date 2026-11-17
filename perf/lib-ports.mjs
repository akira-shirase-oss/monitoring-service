// Imported by every perf harness that starts its own monitor-server and fixtures — the
// Node counterpart of scripts/lib-ports.sh. Each harness has its own default ports (never
// 58400, the default an installed monitor listens on), refuses to start when one of them
// already answers, and talks only to the server it started: after /livez answers, the
// child must still be alive, or what answered is somebody else's process.
//
// Children are stopped through their ChildProcess handles, which Node retires when the
// child exits — never by port, never by a PID read back from somewhere.

import net from 'node:net';

/** True when something accepts TCP connections on 127.0.0.1:port. */
export function portInUse(port) {
	return new Promise((resolve) => {
		const socket = net.connect({ host: '127.0.0.1', port });
		const done = (inUse) => {
			socket.destroy();
			resolve(inUse);
		};
		socket.setTimeout(1000, () => done(true)); // accepted but silent still means taken
		socket.once('connect', () => done(true));
		socket.once('error', () => done(false));
	});
}

/**
 * Refuse to run (exit 1) when any port is taken, naming the flag that overrides it. Runs
 * before anything is spawned, so a refusal starts nothing.
 * @param {Array<[number, string]>} ports `[port, '--flag']` pairs
 */
export async function requireFreePorts(ports) {
	const taken = [];
	for (const [port, flag] of ports) {
		if (!Number.isInteger(port) || port <= 0 || port > 65535) {
			console.error(`${flag} ${port} is not a port`);
			process.exit(2);
		}
		if (await portInUse(port)) taken.push([port, flag]);
	}
	if (taken.length > 0) {
		for (const [port, flag] of taken) {
			console.error(
				`port ${port} is already in use on 127.0.0.1 — refusing to run; pass ${flag} ` +
					'a free port. This harness never stops a process it did not start.',
			);
		}
		process.exit(1);
	}
}

/**
 * Wait (up to `deadlineMs`) for `${base}/livez` while `child` — the server this harness
 * spawned — is alive. Throws if the child exits first, or if /livez never answers.
 */
export async function waitOwnServer(base, child, deadlineMs = 15000) {
	const alive = () => child.exitCode === null && child.signalCode === null;
	const end = Date.now() + deadlineMs;
	while (Date.now() < end) {
		if (!alive()) break;
		try {
			const r = await fetch(`${base}/livez`, { signal: AbortSignal.timeout(1000) });
			if (r.ok && alive()) return;
		} catch {}
		await new Promise((r) => setTimeout(r, 200));
	}
	throw new Error(
		alive()
			? `${base}/livez did not answer within ${deadlineMs} ms`
			: `the monitor-server this harness started exited (code ${child.exitCode}, signal ` +
				`${child.signalCode}); refusing to drive whatever answers on ${base}`,
	);
}
