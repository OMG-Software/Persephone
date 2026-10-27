<script lang="ts">
  import type {
    ContactStatus,
    RoomEncryption,
    TimelineItem
  } from '../../crates/contracts/generated/contracts';
  import EncryptionIcon from './EncryptionIcon.svelte';
  import Icon from './Icon.svelte';
  import ImageMessage from './ImageMessage.svelte';
  import Message from './Message.svelte';
  import { clockTime, dayDividers, fullTime, toDate } from './time';

  let {
    title,
    roomId = '',
    encryption = 'unknown',
    items,
    hasOlder,
    loading,
    error,
    onolder,
    contact = null,
    canVerify = false,
    verifying = false,
    onverify = () => {},
    oninvite,
    onsettings
  }: {
    title: string;
    // The room the items come from; images are loaded from it.
    roomId?: string;
    encryption?: RoomEncryption;
    items: TimelineItem[];
    hasOlder: boolean;
    loading: boolean;
    error: string | null;
    onolder: () => void;
    // The other person in a direct-message room, if this is one.
    contact?: ContactStatus | null;
    // This device may vouch for the contact (it is verified itself).
    canVerify?: boolean;
    verifying?: boolean;
    onverify?: () => void;
    // Opens the invite dialog; absent where inviting is not offered.
    oninvite?: () => void;
    // Opens the room's settings; absent where they are not offered.
    onsettings?: () => void;
  } = $props();

  // Fixed text per trust level; the core sends codes only.
  const trustText: Record<string, string> = {
    verified: 'Verified',
    unverified: 'Not verified',
    changed: 'Identity changed',
    verified_changed: 'Identity changed since you verified them',
    no_identity: 'Has not set up encryption',
    unknown: 'Checking…'
  };

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
  // does not move when they have scrolled up to read older ones. Where the
  // reader is comes from scrolling, not from measuring during an update: by
  // then a tall new message may already be in the page.
  let scroller = $state<HTMLElement | null>(null);
  let atEnd = true;
  function scrolled() {
    if (scroller) atEnd = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
  }
  $effect(() => {
    // An emptied timeline is a room being opened: it starts at the newest.
    if (items.length === 0) atEnd = true;
    if (scroller && atEnd) scroller.scrollTop = scroller.scrollHeight;
  });
</script>

<section class="timeline" aria-labelledby="timeline-heading">
  <header class="chat-header">
    <EncryptionIcon {encryption} size="large" />
    {#if contact}
      <div class="room-title">
        <h2 id="timeline-heading">
          <bdi>{title}</bdi>
          {#if contact.trust === 'verified'}
            <span class="verified-shield"><Icon name="shield" /></span>
          {/if}
        </h2>
        <p class="contact-id">
          <bdi>{contact.userId}</bdi> · {trustText[contact.trust] ?? trustText.unknown}
        </p>
      </div>
      {#if contact.trust === 'unverified' && canVerify}
        <button type="button" class="secondary header-action" disabled={verifying} onclick={onverify}>
          Verify <span class="visually-hidden"><bdi>{contact.userId}</bdi></span>
        </button>
      {/if}
    {:else}
      <Icon name="hash" class="hash" />
      <h2 id="timeline-heading"><bdi>{title}</bdi></h2>
    {/if}
    {#if oninvite}
      <button type="button" class="secondary header-action" onclick={oninvite}>Invite</button>
    {/if}
    {#if onsettings}
      <button
        type="button"
        class="icon-button header-settings"
        aria-label="Room settings"
        title="Room settings"
        onclick={onsettings}
      >
        <Icon name="settings" />
      </button>
    {/if}
  </header>
  <div class="scroller" bind:this={scroller} onscroll={scrolled}>
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
            {:else if item.kind === 'image'}
              <ImageMessage {roomId} {item} />
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
