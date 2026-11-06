<script lang="ts">
	import * as Card from '#lib/components/ui/card/index.js';
	import { ArrowLeft } from '@lucide/svelte';
	import { VOCABULARY, CONCEPTS, type GuideWord } from '#lib/vocabulary.js';

	// Groups render in vocabulary order — the words themselves come from the single
	// glossary; a Rust drift test keeps this file's `word` set honest against the enums.
	const groups: { kind: GuideWord['kind']; title: string; intro: string }[] = [
		{
			kind: 'status',
			title: 'Monitor status',
			intro: 'The badge color. Every status is a claim about evidence, not hope.'
		},
		{
			kind: 'outcome',
			title: 'Check outcomes',
			intro: 'What one scheduled slot produced.'
		},
		{
			kind: 'detail',
			title: 'Failure detail',
			intro: 'The stable classification a transport failure carries in the detail column.'
		},
		{
			kind: 'incident',
			title: 'Incident vocabulary',
			intro: 'An incident tracks four times separately — start-of-failure ≠ time-of-notice.'
		}
	];

	const wordsFor = (kind: GuideWord['kind']) => VOCABULARY.filter((w) => w.kind === kind);
</script>

<svelte:head><title>Guide · Service Monitor</title></svelte:head>

<div class="flex flex-col gap-6">
	<a
		href="/"
		class="inline-flex w-fit items-center gap-1.5 text-sm text-muted-foreground hover:text-foreground"
	>
		<ArrowLeft class="size-4" /> Monitors
	</a>
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">Guide</h1>
		<p class="mt-1 text-sm text-muted-foreground">
			Every status and word on this dashboard, in plain English. The rule underneath
			all of them: this UI only shows committed evidence — `unknown`, gaps and
			“unavailable” are first-class states, never hidden.
		</p>
	</div>

	{#each groups as group}
		<section>
			<h2 class="text-sm font-medium text-muted-foreground">{group.title}</h2>
			<p class="mt-0.5 text-xs text-muted-foreground/80">{group.intro}</p>
			<div class="mt-2 flex flex-col gap-1.5">
				{#each wordsFor(group.kind) as word (word.word)}
					<Card.Root>
						<Card.Content class="flex items-baseline gap-4 px-3 py-2">
							<code class="w-40 shrink-0 rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
								{word.word}
							</code>
							<div>
								<span class="text-sm font-medium">{word.title}</span>
								<p class="text-sm text-muted-foreground">{word.body}</p>
							</div>
						</Card.Content>
					</Card.Root>
				{/each}
			</div>
		</section>
	{/each}

	<section>
		<h2 class="text-sm font-medium text-muted-foreground">Concepts</h2>
		<div class="mt-2 flex flex-col gap-1.5">
			{#each CONCEPTS as concept (concept.title)}
				<Card.Root>
					<Card.Content class="px-3 py-2">
						<span class="text-sm font-medium">{concept.title}</span>
						<p class="text-sm text-muted-foreground">{concept.body}</p>
					</Card.Content>
				</Card.Root>
			{/each}
		</div>
	</section>
</div>
