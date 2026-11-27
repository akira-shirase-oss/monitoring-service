// Shared helpers for the components (the shadcn-svelte conventions): `cn` merges class names,
// resolving Tailwind conflicts, and the types shape component props.
import { createCn } from 'cn/config';

/** Merges class names, resolving conflicts, including the design system's own shadows, shapes
 *  and card padding (D-056, D-182): without them, `shadow-none` couldn't replace `shadow-soft`,
 *  nor `rounded-xl` a component's `rounded-card`, nor `p-4` its `p-card`. */
export const cn = createCn({
	extend: {
		theme: { radius: ['control', 'card'], spacing: ['card', 'card-sm'] },
		classGroups: { shadow: [{ shadow: ['hairline', 'soft', 'float', 'press'] }] }
	}
});

export type WithoutChild<T> = T extends { child?: unknown } ? Omit<T, 'child'> : T;
export type WithoutChildren<T> = T extends { children?: unknown } ? Omit<T, 'children'> : T;
export type WithoutChildrenOrChild<T> = WithoutChildren<WithoutChild<T>>;
export type WithElementRef<T, U extends HTMLElement = HTMLElement> = T & { ref?: U | null };
