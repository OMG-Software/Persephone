<script lang="ts">
  import type { TimelineItem } from '../../crates/contracts/generated/contracts';
  import Message from './Message.svelte';

  let {
    title,
    items,
    hasOlder,
    loading,
    error,
    onolder
  }: {
    title: string;
    items: TimelineItem[];
    hasOlder: boolean;
    loading: boolean;
    error: string | null;
    onolder: () => void;
  } = $props();

  const startsWithGap = $derived(items[0]?.kind === 'gap');
  const key = (item: TimelineItem, index: number) =>
    item.kind === 'message' ? item.event_id : 'gap-' + index;
</script>

<section class="timeline" aria-labelledby="timeline-heading">
  <h2 id="timeline-heading"><bdi>{title}</bdi></h2>
  <button type="button" disabled={!hasOlder || loading} onclick={onolder}>
    {loading
      ? 'Loading…'
      : hasOlder
        ? 'Load older messages'
        : startsWithGap
          ? 'Older messages not shown'
          : 'Start of history'}
  </button>
  {#if error}
    <p role="alert" class="error">{error}</p>
  {/if}
  <ol>
    {#each items as item, index (key(item, index))}
      <li>
        {#if item.kind === 'message'}
          <Message name={item.sender} body={item.body} />
        {:else}
          <p class="gap">Some messages are not shown here.</p>
        {/if}
      </li>
    {/each}
  </ol>
</section>
