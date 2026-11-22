<script lang="ts">
  // The signed-in account as its labels show it: the display name over the
  // user ID, or the user ID alone when no name is set. No name is made up
  // from the ID. `children`, such as the presence menu, shows under the
  // first line.
  import type { Snippet } from 'svelte';

  let { userId, name, children }: { userId: string; name: string | null; children?: Snippet } = $props();

  const shown = $derived(name?.trim() ? name : null);
</script>

<div class="account-text">
  {#if shown}
    <bdi class="account-name">{shown}</bdi>
    {@render children?.()}
    <bdi class="account-id" title={userId}>{userId}</bdi>
  {:else}
    <bdi class="account-name" title={userId}>{userId}</bdi>
    {@render children?.()}
  {/if}
</div>
