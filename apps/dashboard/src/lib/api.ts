// Mutation helpers for the management API. Every write goes through the same
// error envelope the CLI sees — the server's message is shown verbatim, never
// replaced with a generic "something went wrong".

export type ApiFailure = { status: number; message: string };

export async function readEnvelope(response: Response): Promise<ApiFailure | null> {
	if (response.ok) return null;
	let message = `HTTP ${response.status}`;
	try {
		const body = (await response.json()) as { error?: { message?: string } };
		if (body?.error?.message) message = body.error.message;
	} catch {
		// keep the status-line fallback
	}
	return { status: response.status, message };
}
