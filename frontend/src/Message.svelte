<script lang="ts">
  import { avatarColour, initials } from './people';
  import { clockTime, fullTime, toDate } from './time';
  import { bodyParts } from './links';
  import { QUICK_REACTIONS, type Quote, type ReactionCount } from './timeline';

  let {
    name,
    body,
    time = null,
    quote = null,
    edited = false,
    onreply,
    onedit,
    ondelete,
    reactions = [],
    onreact,
    onunreact,
    onlink
  }: {
    name: string;
    body: string;
    time?: number | null;
    // What this message replies to: a quote from the loaded history,
    // 'missing' when that message is not loaded, or null for no reply.
    quote?: Quote | 'missing' | null;
    // The text is the sender's newest edit.
    edited?: boolean;
    // Starts a reply to this message; absent where replying is not offered.
    onreply?: () => void;
    // Starts editing this message; only for this account's own messages.
    onedit?: () => void;
    // Asks to delete this message; only for this account's own messages.
    ondelete?: () => void;
    // The reactions to this message, the most used first.
    reactions?: ReactionCount[];
    // Reacts with a key, or takes back this account's reaction (by its
    // event); absent where reacting is not offered.
    onreact?: (key: string) => void;
    onunreact?: (reactionEventId: string) => void;
    // Asks the core to open a link; without it, links stay plain text.
    onlink?: (url: string) => void;
  } = $props();

  // Whether the quick reactions are open.
  let picking = $state(false);

  // The sender's homeserver sets this time; a value no date can show is left out.
  const date = $derived(toDate(time));
</script>

<!-- Names are isolated with <bdi> and bodies with dir="auto", so bidi control
     characters in one message cannot reorder text outside it. All text is
     rendered as escaped text, never as HTML. -->
<article class="message" aria-label={'Message from ' + name}>
  <span class="avatar {avatarColour(name)}" aria-hidden="true">{initials(name)}</span>
  <div class="message-body">
    <div class="message-head">
      <strong class="sender"><bdi>{name}</bdi></strong>
      {#if date}
        <time datetime={date.toISOString()} title={fullTime(date)}>{clockTime(date)}</time>
      {/if}
      {#if edited}
        <span class="edited">(edited)</span>
      {/if}
      {#if onreply || onedit || ondelete || onreact}
        <span class="message-actions">
          {#if onreact}
            <button type="button" class="link" aria-expanded={picking} onclick={() => (picking = !picking)}>
              React <span class="visually-hidden">to <bdi>{name}</bdi></span>
            </button>
          {/if}
          {#if onreply}
            <button type="button" class="link" onclick={onreply}>
              Reply <span class="visually-hidden">to <bdi>{name}</bdi></span>
            </button>
          {/if}
          {#if onedit}
            <button type="button" class="link" onclick={onedit}>
              Edit <span class="visually-hidden">your message</span>
            </button>
          {/if}
          {#if ondelete}
            <button type="button" class="link" onclick={ondelete}>
              Delete <span class="visually-hidden">your message</span>
            </button>
          {/if}
        </span>
      {/if}
    </div>
    {#if quote === 'missing'}
      <p class="reply-quote">In reply to an earlier message</p>
    {:else if quote}
      <p class="reply-quote">
        In reply to <bdi>{quote.sender}</bdi>: <span dir="auto">{quote.text}</span>
      </p>
    {/if}
    <!-- A link is a button that asks the core, never an anchor: the page
         navigates nowhere, and the core shows the full address first. -->
    <p dir="auto">
      {#if onlink}{#each bodyParts(body) as part, at (at)}{#if 'link' in part}<button
              type="button"
              class="link body-link"
              title={'Open ' + part.link}
              onclick={() => onlink(part.link)}>{part.link}</button
            >{:else}{part.text}{/if}{/each}{:else}{body}{/if}
    </p>
    {#if picking && onreact}
      <div class="quick-reactions" role="group" aria-label="Quick reactions">
        {#each QUICK_REACTIONS as key (key)}
          <button
            type="button"
            class="reaction"
            aria-label={'React with ' + key}
            onclick={() => {
              picking = false;
              onreact(key);
            }}>{key}</button
          >
        {/each}
      </div>
    {/if}
    {#if reactions.length > 0}
      <div class="reactions" role="group" aria-label="Reactions">
        {#each reactions as reaction (reaction.key)}
          <button
            type="button"
            class="reaction"
            class:mine={reaction.mine !== null}
            aria-pressed={reaction.mine !== null}
            disabled={reaction.mine !== null ? !onunreact : !onreact}
            onclick={() =>
              reaction.mine !== null ? onunreact?.(reaction.mine) : onreact?.(reaction.key)}
          >
            <bdi>{reaction.key}</bdi>
            <span class="count">{reaction.count}</span>
          </button>
        {/each}
      </div>
    {/if}
  </div>
</article>
