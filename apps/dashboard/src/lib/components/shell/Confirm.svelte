<!--
  The one confirmation every destructive action goes through: it names what will happen
  in the title, says what can't be undone underneath, and its confirm button carries the
  action's own verb — never "OK". An action that needs something typed first (a password)
  puts its field in `extra`, outside the description a screen reader reads as one
  sentence, and keeps the button `disabled` until it has it.
-->
<script lang="ts">
	import type { Snippet } from 'svelte';
	import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js';

	let {
		open = $bindable(false),
		title,
		confirm,
		busy = false,
		disabled = false,
		onConfirm,
		children,
		extra
	}: {
		open?: boolean;
		title: string;
		/** The action's verb: "Revoke token", "Remove member". */
		confirm: string;
		busy?: boolean;
		/** The action can't run yet: a field in `extra` is still empty. */
		disabled?: boolean;
		onConfirm: () => unknown;
		children?: Snippet;
		/** Fields the action needs, and its refusal, between the text and the buttons. */
		extra?: Snippet;
	} = $props();
</script>

<AlertDialog.Root bind:open>
	<AlertDialog.Content data-testid="confirm">
		<AlertDialog.Header>
			<AlertDialog.Title>{title}</AlertDialog.Title>
			{#if children}
				<AlertDialog.Description>{@render children()}</AlertDialog.Description>
			{/if}
		</AlertDialog.Header>
		{#if extra}{@render extra()}{/if}
		<AlertDialog.Footer>
			<AlertDialog.Cancel disabled={busy}>Cancel</AlertDialog.Cancel>
			<AlertDialog.Action
				variant="destructive"
				disabled={busy || disabled}
				data-testid="confirm-action"
				onclick={(event: MouseEvent) => {
					// The dialog closes when the write lands, not when the button is pressed.
					event.preventDefault();
					onConfirm();
				}}
			>
				{busy ? 'Working…' : confirm}
			</AlertDialog.Action>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
