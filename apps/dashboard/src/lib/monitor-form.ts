// What MonitorForm emits — mirrors the API's CreateMonitor/PatchMonitor fields.
export type MonitorPayload = {
	name?: string;
	url?: string;
	interval_s?: number;
	timeout_s?: number;
	failures_to_open?: number;
	passes_to_recover?: number;
};
