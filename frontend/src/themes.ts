// The colour themes. The choice is an account setting, kept by the core in
// the encrypted profile like the others; the page only shows it. Each theme
// is a set of colour tokens in style.css, chosen by `data-theme` on the root
// element. Before an account's settings load (signed out, locked) the
// default theme shows.

import type { Theme } from '../../crates/contracts/generated/contracts';

/** The theme shown until an account's own choice loads. */
export const DEFAULT_THEME: Theme = 'orange';

/** The themes offered, in the order the drop-down lists them. */
export const THEMES: readonly { value: Theme; label: string }[] = [
  { value: 'orange', label: 'Dark grey and orange' },
  { value: 'red', label: 'Black and red' },
  { value: 'blue', label: 'Black and blue' }
];

/** `value` when it names a theme this build has; otherwise the default. */
export function themeOf(value: unknown): Theme {
  return THEMES.some((theme) => theme.value === value) ? (value as Theme) : DEFAULT_THEME;
}

/**
 * Shows `value` on `root` (the document element), or the default theme when
 * `value` names no known theme, so a stray value never leaves the page
 * without colours. Returns the theme shown.
 */
export function applyTheme(root: { dataset: Record<string, string | undefined> }, value: unknown): Theme {
  const theme = themeOf(value);
  if (root.dataset.theme !== theme) root.dataset.theme = theme;
  return theme;
}
