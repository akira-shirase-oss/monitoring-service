// What a test alert's result means, in plain words. The wire word stays visible beside
// the sentence wherever this is shown — it is what the logs and the docs use.

const WORDS: Record<string, string> = {
	delivered: 'Your endpoint answered with a success status.',
	rejected_local: 'The address points at a private or local network, which alerts are never sent to.',
	deadline: 'Your endpoint did not answer in time.',
	dns: "The hostname didn't resolve.",
	connect: 'The connection was refused or could not be opened.',
	tls: 'The TLS handshake failed.',
	redirect: 'Your endpoint answered with a redirect. Alerts are sent to the exact address only.'
};

export function explainDelivery(detail: string): string {
	const known = WORDS[detail];
	if (known) return known;
	const status = /^http_(\d{3})$/.exec(detail);
	if (status) return `Your endpoint answered HTTP ${status[1]}, which counts as a failed delivery.`;
	return 'The delivery failed.';
}
