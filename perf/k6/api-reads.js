// k6 script for perf/api-load.mjs — the management API's read surface, bearer-authed.
// Run through the harness (it starts the server, registers monitors and passes these):
//   SM_BASE, SM_TOKEN, SM_IDS (comma-separated monitor ids), SM_REQUESTS, SM_VUS, SM_SUMMARY
// Every iteration is one request; the mix rotates by iteration number so it is the same on
// every run: 4/10 list monitors, 2/10 one monitor, 2/10 its checks, 1/10 its stats,
// 1/10 incidents.
import http from 'k6/http';
import { check } from 'k6';
import exec from 'k6/execution';

const BASE = __ENV.SM_BASE;
const IDS = (__ENV.SM_IDS || '').split(',').filter(Boolean);
const REQUESTS = Number(__ENV.SM_REQUESTS || 1000000);
const VUS = Number(__ENV.SM_VUS || 32);
const PARAMS = { headers: { Authorization: `Bearer ${__ENV.SM_TOKEN}` } };
const NAMES = ['list', 'list', 'list', 'list', 'get', 'get', 'checks', 'checks', 'stats', 'incidents'];

export const options = {
	scenarios: {
		reads: { executor: 'shared-iterations', vus: VUS, iterations: REQUESTS, maxDuration: '60m' },
	},
	discardResponseBodies: true,
	summaryTrendStats: ['min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max', 'avg'],
	// The gate: every request answered 200. The per-route `max>=0` lines always hold; they
	// only make k6 report each route's latency in the summary.
	thresholds: {
		http_req_failed: ['rate==0'],
		checks: ['rate==1'],
		...Object.fromEntries([...new Set(NAMES)].map((n) => [`http_req_duration{name:${n}}`, ['max>=0']])),
	},
};

function url(name, id) {
	switch (name) {
		case 'list':
			return `${BASE}/api/monitors`;
		case 'get':
			return `${BASE}/api/monitors/${id}`;
		case 'checks':
			return `${BASE}/api/monitors/${id}/checks`;
		case 'stats':
			return `${BASE}/api/monitors/${id}/stats`;
		default:
			return `${BASE}/api/incidents`;
	}
}

export default function () {
	const i = exec.scenario.iterationInTest;
	const name = NAMES[i % NAMES.length];
	const res = http.get(url(name, IDS[i % IDS.length]), { ...PARAMS, tags: { name } });
	check(res, { 'status 200': (r) => r.status === 200 });
}

export function handleSummary(data) {
	return { [__ENV.SM_SUMMARY]: JSON.stringify(data) };
}
