<script lang="ts">
  import type { Snippet } from 'svelte';
  import Icon, { type IconName } from './Icon.svelte';

  // A button that shows only an icon. Its label shows as a tooltip on hover
  // and keyboard focus; Escape hides the tooltip until the pointer or focus
  // leaves. The accessible name is the label plus `children` (visually
  // hidden), such as "to <name>", so screen readers hear the full action.
  let {
    icon,
    label,
    name,
    onclick,
    disabled = false,
    pressed,
    expanded,
    filled = false,
    tip = 'above',
    class: extra = '',
    children
  }: {
    icon: IconName;
    // The tooltip text.
    label: string;
    // The accessible name when it differs from the tooltip, such as a
    // toggle whose tooltip says what a press does next.
    name?: string;
    onclick: () => void;
    disabled?: boolean;
    // For a toggle button: whether it is on.
    pressed?: boolean;
    // For a button that opens something: whether that is open.
    expanded?: boolean;
    // Fill the icon, such as a star that is on.
    filled?: boolean;
    // Where the tooltip shows.
    tip?: 'above' | 'below' | 'start';
    class?: string;
    children?: Snippet;
  } = $props();

  let dismissed = $state(false);
</script>

<span class="action-anchor tip-{tip}">
  <button
    type="button"
    class="icon-action {extra}"
    {disabled}
    aria-pressed={pressed}
    aria-expanded={expanded}
    {onclick}
    onkeydown={(event) => {
      if (event.key === 'Escape' && !dismissed) {
        dismissed = true;
        event.stopPropagation();
      }
    }}
    onblur={() => (dismissed = false)}
    onmouseleave={() => (dismissed = false)}
  >
    <Icon name={icon} class={filled ? 'filled' : ''} />
    <span class="visually-hidden">{name ?? label}{#if children}{' '}{@render children()}{/if}</span>
  </button>
  <span class="action-tip" class:dismissed aria-hidden="true">{label}</span>
</span>
