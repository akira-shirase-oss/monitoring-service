<script lang="ts">
	import { untrack } from 'svelte';
	import type { MonitorPayload } from '#lib/monitor-form.js';

	// Shared schedule/threshold editor for create (list page) and patch (detail page).
	// Numeric fields are parsed here — empty means "leave unset", the API applies its
	// own validation and its error message is what the caller displays.
	let {
		initial = {},
		showIdentity = false,
		submitLabel,
		onSubmit,
		onCancel
	}: {
		initial?: Partial<{
			name: string;
			url: string;
			interval_s: number;
			timeout_s: number;
			failures_to_open: number;
			passes_to_recover: number;
		}>;
		showIdentity?: boolean;
		submitLabel: string;
		onSubmit: (payload: MonitorPayload) => Promise<void>;
		onCancel?: () => void;
	} = $props();

	// `initial` seeds the fields once — they must not follow prop changes (the user is
	// editing them), so the capture is deliberately untracked.
	let name = $state(untrack(() => initial.name ?? ''));
	let url = $state(untrack(() => initial.url ?? ''));
	let interval = $state(untrack(() => initial.interval_s?.toString() ?? ''));
	let timeout = $state(untrack(() => initial.timeout_s?.toString() ?? ''));
	let failures = $state(untrack(() => initial.failures_to_open?.toString() ?? ''));
	let passes = $state(untrack(() => initial.passes_to_recover?.toString() ?? ''));
	let busy = $state(false);

	function num(value: string): number | undefined {
		const trimmed = value.trim();
		if (trimmed === '') return undefined;
		const parsed = Number(trimmed);
		return Number.isFinite(parsed) ? parsed : undefined;
	}

	async function submit() {
		busy = true;
		try {
			await onSubmit({
				...(showIdentity ? { name: name.trim(), url: url.trim() } : {}),
				interval_s: num(interval),
				timeout_s: num(timeout),
				failures_to_open: num(failures),
				passes_to_recover: num(passes)
			});
		} finally {
			busy = false;
		}
	}

	const field =
		'w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm tabular';
	const label = 'block text-xs font-medium text-muted-foreground';
</script>

<form
	class="grid grid-cols-2 gap-3 sm:grid-cols-3"
	onsubmit={(event) => {
		event.preventDefault();
		submit();
	}}
>
	{#if showIdentity}
		<div>
			<span class={label}>name</span>
			<input bind:value={name} required class={field} placeholder="billing-api" />
		</div>
		<div class="col-span-2">
			<span class={label}>health url</span>
			<input
				bind:value={url}
				required
				type="url"
				class={`${field} font-mono`}
				placeholder="https://example.com/healthz"
			/>
		</div>
	{/if}
	<div>
		<span class={label}>interval (s)</span>
		<input bind:value={interval} inputmode="numeric" class={field} placeholder="60" />
	</div>
	<div>
		<span class={label}>timeout (s)</span>
		<input bind:value={timeout} inputmode="numeric" class={field} placeholder="5" />
	</div>
	<div>
		<span class={label}>fails to open</span>
		<input bind:value={failures} inputmode="numeric" class={field} placeholder="3" />
	</div>
	<div>
		<span class={label}>passes to recover</span>
		<input bind:value={passes} inputmode="numeric" class={field} placeholder="2" />
	</div>
	<div class="col-span-2 flex items-end justify-end gap-2">
		{#if onCancel}
			<button
				type="button"
				onclick={onCancel}
				class="rounded-md border border-input px-3 py-1.5 text-sm"
			>
				Cancel
			</button>
		{/if}
		<button
			type="submit"
			disabled={busy}
			class="rounded-md bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground disabled:opacity-50"
		>
			{busy ? 'Saving…' : submitLabel}
		</button>
	</div>
</form>
