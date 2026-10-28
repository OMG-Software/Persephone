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
  import { stateParts } from './stateText';
  import { clockTime, dayDividers, fullTime, toDate } from './time';
  import { applyRelations, newestEvent, quoteOf, type ShownItem } from './timeline';

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
    onsettings,
    onreply,
    ownUserId = null,
    onedit,
    ondelete,
    onreact,
    onunreact,
    unreadFrom = null,
    onread,
    readBy = [],
    onlink
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
    // Starts a reply to a message; absent where replying is not offered.
    onreply?: (eventId: string) => void;
    // This account's user ID: its own messages can be edited.
    ownUserId?: string | null;
    // Starts editing one of this account's messages; absent where editing
    // is not offered.
    onedit?: (eventId: string, body: string) => void;
    // Asks to delete one of this account's messages; absent where deleting
    // is not offered.
    ondelete?: (eventId: string) => void;
    // Reacts to a message, or takes back this account's reaction by its
    // event; absent where reacting is not offered.
    onreact?: (eventId: string, key: string) => void;
    onunreact?: (reactionEventId: string) => void;
    // The first unread message when the room opened: a "New messages" line
    // goes before it.
    unreadFrom?: string | null;
    // Called with the newest event once the reader has seen it: the window
    // has focus and the timeline is at its end.
    onread?: (eventId: string) => void;
    // Who else has read up to the newest event.
    readBy?: string[];
    // Asks the core to open a link from a message.
    onlink?: (url: string) => void;
  } = $props();

  // Edits, deletions and reactions are not lines of their own: each message
  // shows its newest edit and its reactions, or that it was deleted.
  const shown = $derived(applyRelations(items, ownUserId));

  // Fixed text per trust level; the core sends codes only.
  const trustText: Record<string, string> = {
    verified: 'Verified',
    unverified: 'Not verified',
    changed: 'Identity changed',
    verified_changed: 'Identity changed since you verified them',
    no_identity: 'Has not set up encryption',
    unknown: 'Checking…'
  };

  const startsWithGap = $derived(shown[0]?.kind === 'gap');
  // Fixed text per notice; the core sends codes only.
  const noticeText: Record<string, string> = {
    undecryptable: 'Encrypted message. This device has no key for it.',
    withheld: 'Encrypted message. The sender did not share its key with this device.',
    untrusted_sender:
      'Encrypted message from a device its owner has not verified. It is not shown, because it could come from a device they never added.',
    unsupported: 'A kind of message Persephone does not show yet.',
    deleted: 'Message deleted.'
  };
  const key = (item: ShownItem, index: number) =>
    item.kind === 'gap' ? 'gap-' + index : item.event_id;

  // A "Today", "Yesterday" or date line where the day changes. Items stay in
  // the order the homeserver gave; their times are not used to sort.
  const timeOf = (item: ShownItem) => (item.kind === 'gap' ? null : item.origin_server_ts);
  const dividers = $derived(dayDividers(shown.map(timeOf), new Date()));

  // The timeline stays at the newest message while the reader is there; it
  // does not move when they have scrolled up to read older ones. Where the
  // reader is comes from scrolling, not from measuring during an update: by
  // then a tall new message may already be in the page.
  let scroller = $state<HTMLElement | null>(null);
  let atEnd = $state(true);
  function scrolled() {
    if (scroller) atEnd = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
  }

  // The newest event counts as read only while the reader can see it: the
  // window has focus and the timeline is at its end.
  let attention = $state(0);
  let reported: string | null = null;
  $effect(() => {
    void attention;
    const newest = newestEvent(items);
    if (!onread || !atEnd || newest === null || newest === reported) return;
    if (typeof document === 'undefined' || !document.hasFocus()) return;
    reported = newest;
    onread(newest);
  });
  $effect(() => {
    // An emptied timeline is a room being opened: it starts at the newest.
    if (items.length === 0) atEnd = true;
    if (scroller && atEnd) scroller.scrollTop = scroller.scrollHeight;
  });
</script>

<svelte:window onfocus={() => (attention += 1)} />

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
        {#each shown as item, index (key(item, index))}
          <li>
            {#if dividers[index]}
              <p class="day-divider"><span>{dividers[index]}</span></p>
            {/if}
            {#if unreadFrom !== null && item.kind !== 'gap' && item.event_id === unreadFrom}
              <p class="unread-divider"><span>New messages</span></p>
            {/if}
            {#if item.kind === 'message'}
              <Message
                name={item.sender}
                body={item.body}
                time={item.origin_server_ts}
                quote={item.reply_to === null || item.reply_to === undefined
                  ? null
                  : (quoteOf(shown, item.reply_to) ?? 'missing')}
                edited={item.edited === true}
                onreply={onreply && (() => onreply(item.event_id))}
                onedit={onedit && ownUserId !== null && item.sender === ownUserId
                  ? () => onedit(item.event_id, item.body)
                  : undefined}
                ondelete={ondelete && ownUserId !== null && item.sender === ownUserId
                  ? () => ondelete(item.event_id)
                  : undefined}
                reactions={item.reactions ?? []}
                onreact={onreact && ((key) => onreact(item.event_id, key))}
                {onunreact}
                {onlink}
              />
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
            {:else if item.kind === 'state'}
              {@const date = toDate(item.origin_server_ts)}
              <p class="notice state-change">
                <span
                  >{#each stateParts(item.sender, item.change) as part, at (at)}{#if 'name' in part}<bdi
                        >{part.name}</bdi
                      >{:else}{part.text}{/if}{/each}</span
                >
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
      {#if readBy.length > 0}
        <p class="seen-by">
          Seen by {#each readBy as name, at (at)}{#if at > 0},
            {/if}<bdi>{name}</bdi>{/each}
        </p>
      {/if}
    </div>
  </div>
</section>
