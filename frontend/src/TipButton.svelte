<script lang="ts">
  import type { Snippet } from 'svelte';

  // A session button with help text shown on hover and on keyboard focus,
  // and linked with aria-describedby for screen readers. Escape hides it
  // until the pointer or focus leaves.
  let {
    id,
    help,
    disabled = false,
    above = false,
    class: buttonClass = 'secondary',
    label,
    onclick,
    children
  }: {
    id: string;
    help: string;
    disabled?: boolean;
    // Shows the help above the button, for buttons at the bottom of the window.
    above?: boolean;
    class?: string;
    // An accessible name, for a button that shows only an icon.
    label?: string;
    onclick: () => void;
    children: Snippet;
  } = $props();

  let dismissed = $state(false);
</script>

<span class="tip-anchor" class:above>
  <button
    type="button"
    class={buttonClass}
    {disabled}
    aria-label={label}
    aria-describedby={id}
    {onclick}
    onkeydown={(event) => {
      if (event.key === 'Escape') dismissed = true;
    }}
    onblur={() => (dismissed = false)}
    onmouseleave={() => (dismissed = false)}
  >
    {@render children()}
  </button>
  <span {id} role="tooltip" class="tip" class:dismissed>{help}</span>
</span>
