// What MonitorForm emits — mirrors the API's CreateMonitor/PatchMonitor fields.
export type MonitorPayload = {
	name?: string;
	url?: string;
	interval_s?: number;
	timeout_s?: number;
	failures_to_open?: number;
	passes_to_recover?: number;
	/** `GET` reads the response; `HEAD` asks for the status line and headers only. */
	method?: 'GET' | 'HEAD';
	/** Statuses that count as a pass: single codes and inclusive ranges, e.g. `["200-299", "401"]`. */
	expect_statuses?: string[];
	/** Text the response body must contain. `null` removes a stored expectation. */
	expect_body?: string | null;
};

/** "200-299, 401" -> ["200-299", "401"]. Empty input means "leave the default". */
export function statusSpecs(text: string): string[] | undefined {
	const parts = text
		.split(/[\s,]+/)
		.map((part) => part.trim())
		.filter((part) => part !== '');
	return parts.length === 0 ? undefined : parts;
}
