// k6 script — the public status-page read surface, unauthenticated.
// Run through the harness (it starts the server and seeds the pages):
//   SM_BASE, SM_SLUG (an enabled page), SM_STALE_SLUG (enabled, never written),
//   SM_REQUESTS, SM_VUS, SM_SUMMARY
// Every iteration is one anonymous GET of /s/<slug>/status.json. The mix rotates by
// iteration so it is the same on every run: 8/10 the live beat, 1/10 the born-expired
// page, 1/10 an unknown slug's uniform 404 — the three honest answers the route gives.
import http from 'k6/http';
import { check } from 'k6';
import exec from 'k6/execution';

const BASE = __ENV.SM_BASE;
const SLUG = __ENV.SM_SLUG;
const STALE = __ENV.SM_STALE_SLUG;
const REQUESTS = Number(__ENV.SM_REQUESTS || 1000000);
const VUS = Number(__ENV.SM_VUS || 32);
const NAMES = ['beat', 'beat', 'beat', 'beat', 'beat', 'beat', 'beat', 'beat', 'stale', 'missing'];

export const options = {
	scenarios: {
		reads: { executor: 'shared-iterations', vus: VUS, iterations: REQUESTS, maxDuration: '60m' },
	},
	discardResponseBodies: true,
	summaryTrendStats: ['min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max', 'avg'],
	// The gate: every request answered its honest status — 200s for the beats, the
	// 404 for the missing page, and no transport failures at all.
	thresholds: {
		http_req_failed: ['rate==0'],
		checks: ['rate==1'],
		...Object.fromEntries([...new Set(NAMES)].map((n) => [`http_req_duration{name:${n}}`, ['max>=0']])),
	},
};

const EXPECTED = { beat: 200, stale: 200, missing: 404 };

export default function () {
	const i = exec.scenario.iterationInTest;
	const name = NAMES[i % NAMES.length];
	const slug = name === 'beat' ? SLUG : name === 'stale' ? STALE : 'no-such-page';
	const res = http.get(`${BASE}/s/${slug}/status.json`, {
		responseCallback: http.expectedStatuses(EXPECTED[name]),
		tags: { name },
	});
	check(res, { [`status ${EXPECTED[name]}`]: (r) => r.status === EXPECTED[name] });
}

export function handleSummary(data) {
	return { [__ENV.SM_SUMMARY]: JSON.stringify(data) };
}
