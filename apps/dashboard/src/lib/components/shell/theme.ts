// The dashboard's theme: it follows the system until someone picks otherwise. One action
// flips between light and dark; flipping onto the scheme the system already asks for goes
// back to following the system, so "system" needs no menu entry of its own.
import { tick } from 'svelte';
import { mode, resetMode, setMode, systemPrefersMode } from 'mode-watcher';

/** Light becomes dark and dark becomes light. `origin` is where the new theme spreads from. */
export function toggleTheme(origin?: HTMLElement | null): void {
	const next = mode.current === 'dark' ? 'light' : 'dark';
	const apply = () => (next === systemPrefersMode.current ? resetMode() : setMode(next));
	const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
	if (!('startViewTransition' in document) || reduced) {
		apply();
		return;
	}
	// The new theme grows from the button as a circle (a view transition).
	const box = origin?.getBoundingClientRect();
	const x = box ? box.left + box.width / 2 : window.innerWidth - 32;
	const y = box ? box.top + box.height / 2 : 32;
	const radius = Math.hypot(Math.max(x, window.innerWidth - x), Math.max(y, window.innerHeight - y));
	const transition = document.startViewTransition(async () => {
		apply();
		await tick();
	});
	void transition.ready.then(() => {
		document.documentElement.animate(
			{ clipPath: [`circle(0px at ${x}px ${y}px)`, `circle(${radius}px at ${x}px ${y}px)`] },
			{ duration: 480, easing: 'cubic-bezier(0.2, 0.8, 0.2, 1)', pseudoElement: '::view-transition-new(root)' }
		);
	});
}
