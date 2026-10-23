<script lang="ts">
  import type { RoomEncryption, TimelineItem } from '../../crates/contracts/generated/contracts';
  import EncryptionIcon from './EncryptionIcon.svelte';
  import Icon from './Icon.svelte';
  import Message from './Message.svelte';
  import { clockTime, dayDividers, fullTime, toDate } from './time';

  let {
    title,
    encryption = 'unknown',
    items,
    hasOlder,
    loading,
    error,
    onolder
  }: {
    title: string;
    encryption?: RoomEncryption;
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

  // A "Today", "Yesterday" or date line where the day changes. Items stay in
  // the order the homeserver gave; their times are not used to sort.
  const timeOf = (item: TimelineItem) => (item.kind === 'gap' ? null : item.origin_server_ts);
  const dividers = $derived(dayDividers(items.map(timeOf), new Date()));

  // The timeline stays at the newest message while the reader is there; it
  // does not move when they have scrolled up to read older ones.
  let scroller = $state<HTMLElement | null>(null);
  let atEnd = true;
  $effect.pre(() => {
    void items;
    if (scroller) {
      atEnd = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
    }
  });
  $effect(() => {
    void items;
    if (scroller && atEnd) scroller.scrollTop = scroller.scrollHeight;
  });
</script>

<section class="timeline" aria-labelledby="timeline-heading">
  <header class="chat-header">
    <EncryptionIcon {encryption} size="large" />
    <Icon name="hash" class="hash" />
    <h2 id="timeline-heading"><bdi>{title}</bdi></h2>
  </header>
  <div class="scroller" bind:this={scroller}>
    <div class="stream">
      <button type="button" disabled={!hasOlder || loading} class="older" onclick={onolder}>
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
            {#if dividers[index]}
              <p class="day-divider"><span>{dividers[index]}</span></p>
            {/if}
            {#if item.kind === 'message'}
              <Message name={item.sender} body={item.body} time={item.origin_server_ts} />
            {:else if item.kind === 'notice'}
              {@const date = toDate(item.origin_server_ts)}
              <p class="notice">
                <span><bdi>{item.sender}</bdi>: {noticeText[item.notice] ?? noticeText.unsupported}</span>
                {#if date}
                  <time datetime={date.toISOString()} title={fullTime(date)}>{clockTime(date)}</time>
                {/if}
              </p>
            {:else}
              <p class="gap">Some messages are not shown here.</p>
            {/if}
          </li>
        {/each}
      </ol>
    </div>
  </div>
</section>
