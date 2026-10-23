<script lang="ts">
  import type { Snippet } from 'svelte';

  // A header button with help text shown on hover and on keyboard focus, and
  // linked with aria-describedby for screen readers. Escape hides it until
  // the pointer or focus leaves.
  let {
    id,
    help,
    disabled = false,
    onclick,
    children
  }: {
    id: string;
    help: string;
    disabled?: boolean;
    onclick: () => void;
    children: Snippet;
  } = $props();

  let dismissed = $state(false);
</script>

<span class="session tip-anchor">
  <button
    type="button"
    {disabled}
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
