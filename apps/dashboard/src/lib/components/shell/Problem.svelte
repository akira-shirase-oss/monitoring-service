<!--
  A failed read or write, said plainly: what happened and what to do, with Sign in when
  that's the problem and Try again when trying again can help. The server's own words go
  in verbatim — the API's message is already the honest one; the technical detail is the
  status + message pair a report would quote.
-->
<script lang="ts">
	import CircleAlertIcon from '@lucide/svelte/icons/circle-alert';
	import * as Alert from '#lib/components/ui/alert/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import type { ApiFailure } from '#lib/api.js';
	import TechnicalDetails from './TechnicalDetails.svelte';

	let {
		error,
		what = 'this',
		retry
	}: {
		error: ApiFailure | { status: number; message: string } | null;
		/** What failed to load, for people: "monitors", "the roster". */
		what?: string;
		/** Asks again; offered when another try can help. */
		retry?: () => unknown;
	} = $props();

	const signedOut = $derived(error?.status === 401);
	const forbidden = $derived(error?.status === 403);
	const retryable = $derived(error === null || (error?.status ?? 0) >= 500 || error?.status === 429);
	const title = $derived(
		signedOut
			? 'Sign in to continue'
			: forbidden
				? "Your role can't do this"
				: `Couldn't load ${what}`
	);
</script>

<Alert.Root variant="destructive" role="alert" data-testid="problem">
	<CircleAlertIcon />
	<Alert.Title>{title}</Alert.Title>
	<Alert.Description>
		<p>
			{error?.message ?? 'The monitor server is unreachable — this says nothing about your targets.'}
			{#if signedOut}
				<a class="underline" href="/login">Sign in</a>
			{/if}
		</p>
		{#if retry && retryable}
			<Button class="mt-2" variant="outline" size="sm" onclick={() => retry?.()}>Try again</Button>
		{/if}
		{#if error}
			<TechnicalDetails class="mt-2"><p class="break-words">HTTP {error.status} · {error.message}</p></TechnicalDetails>
		{/if}
	</Alert.Description>
</Alert.Root>
