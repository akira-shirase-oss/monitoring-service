// Times as people read them. The API speaks RFC 3339; a value that doesn't parse is
// shown as written rather than as "Invalid Date".

const DAY = 86_400_000;

export function when(iso: string | null | undefined): string {
	if (!iso) return '—';
	const at = Date.parse(iso);
	if (Number.isNaN(at)) return iso;
	return new Date(at).toLocaleString(undefined, {
		year: 'numeric',
		month: 'short',
		day: 'numeric',
		hour: '2-digit',
		minute: '2-digit'
	});
}

/** "12 s", "4 min", "3 h 10 min", "2 d 5 h" — a span, never a clock time. */
export function span(ms: number): string {
	const s = Math.max(0, Math.round(ms / 1000));
	if (s < 60) return `${s} s`;
	const m = Math.floor(s / 60);
	if (m < 60) return `${m} min`;
	const h = Math.floor(m / 60);
	if (h < 24) return m % 60 === 0 ? `${h} h` : `${h} h ${m % 60} min`;
	const d = Math.floor(h / 24);
	return h % 24 === 0 ? `${d} d` : `${d} d ${h % 24} h`;
}

export function ago(iso: string | null | undefined, now: number = Date.now()): string {
	if (!iso) return 'never';
	const at = Date.parse(iso);
	if (Number.isNaN(at)) return iso;
	const gap = now - at;
	if (gap < 0) return when(iso);
	if (gap < 45_000) return 'just now';
	if (gap > 30 * DAY) return when(iso);
	return `${span(gap)} ago`;
}

/** A `datetime-local` value (the browser's own zone) as the RFC 3339 instant the API takes. */
export function fromLocalInput(value: string): string | null {
	if (value === '') return null;
	const at = new Date(value);
	return Number.isNaN(at.getTime()) ? null : at.toISOString();
}

/** Basis points as a percentage: 9987 -> "99.87%". `null` means "nothing to divide by". */
export function percent(bps: number | null | undefined): string {
	if (bps === null || bps === undefined) return '—';
	return `${(bps / 100).toFixed(bps % 100 === 0 ? 0 : 2)}%`;
}
