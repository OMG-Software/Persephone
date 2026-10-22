<script lang="ts">
  import type { TimelineItem } from '../../crates/contracts/generated/contracts';
  import Message from './Message.svelte';

  let {
    title,
    notEncrypted = false,
    items,
    hasOlder,
    loading,
    error,
    onolder
  }: {
    title: string;
    notEncrypted?: boolean;
    items: TimelineItem[];
    hasOlder: boolean;
    loading: boolean;
    error: string | null;
    onolder: () => void;
  } = $props();

  const startsWithGap = $derived(items[0]?.kind === 'gap');
  // Fixed text per notice; the core sends codes only.
  const noticeText: Record<string, string> = {
    undecryptable: 'Encrypted message. This device has no key for it.',
    withheld: 'Encrypted message. The sender did not share its key with this device.',
    untrusted_sender:
      'Encrypted message from a device its owner has not verified. It is not shown, because it could come from a device they never added.',
    unsupported: 'A kind of message Persephone does not show yet.'
  };
  const key = (item: TimelineItem, index: number) =>
    item.kind === 'gap' ? 'gap-' + index : item.event_id;
</script>

<section class="timeline" aria-labelledby="timeline-heading">
  <h2 id="timeline-heading"><bdi>{title}</bdi></h2>
  {#if notEncrypted}
    <p class="note unencrypted">Not encrypted: the homeserver can read this room.</p>
  {/if}
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
        {:else if item.kind === 'notice'}
          <p class="notice">
            <bdi>{item.sender}</bdi>:
            {noticeText[item.notice] ?? noticeText.unsupported}
          </p>
        {:else}
          <p class="gap">Some messages are not shown here.</p>
        {/if}
      </li>
    {/each}
  </ol>
</section>
