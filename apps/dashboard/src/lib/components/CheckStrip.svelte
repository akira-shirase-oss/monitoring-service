<script lang="ts">
	// Uptime strip: one cell per committed check, oldest -> newest; gap cells mark
	// slots where no observation exists. Colors map outcomes to the status tokens —
	// a hole in evidence is a hole, not a green.
	import { buildStripCells, gapLabel, type StripCheck } from '#lib/check-strip.js';

	let { checks, intervalS }: { checks: StripCheck[]; intervalS: number } = $props();

	const cells = $derived(buildStripCells(checks, intervalS));

	function titleFor(check: StripCheck): string {
		const label =
			check.outcome === 'http_fail' && check.http_status
				? `HTTP ${check.http_status}`
				: (check.detail ?? check.outcome);
		const latency = check.latency_ms != null ? ` · ${check.latency_ms}ms` : '';
		return `${new Date(check.slot_at).toLocaleString()} · ${label}${latency}`;
	}
</script>

{#if cells.length > 0}
	<div class="check-strip" role="img" aria-label="check history, oldest to newest">
		{#each cells as cell}
			{#if cell.kind === 'gap'}
				<span
					class="cell gap"
					data-outcome="missing"
					title={gapLabel(cell.slots)}
				></span>
			{:else}
				<span
					class="cell {cell.check.outcome === 'pass'
						? 'up'
						: cell.check.outcome === 'missed' || cell.check.outcome === 'unknown'
							? 'missed'
							: 'down'}"
					data-outcome={cell.check.outcome}
					title={titleFor(cell.check)}
				></span>
			{/if}
		{/each}
	</div>
{/if}

<style>
	.check-strip {
		display: flex;
		gap: 1px;
		align-items: stretch;
		height: 14px;
	}
	.cell {
		flex: 1 1 4px;
		min-width: 2px;
		border-radius: 1px;
	}
	.cell.up {
		background: var(--live);
	}
	.cell.down {
		background: var(--attention);
	}
	.cell.missed {
		background: var(--draft);
	}
	.cell.gap {
		background: repeating-linear-gradient(
			45deg,
			transparent,
			transparent 2px,
			var(--muted) 2px,
			var(--muted) 4px
		);
		border: 1px dashed var(--border);
	}
</style>
