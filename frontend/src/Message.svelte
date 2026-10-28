<script lang="ts">
  import { avatarColour, initials } from './people';
  import { clockTime, fullTime, toDate } from './time';
  import type { Quote } from './timeline';

  let {
    name,
    body,
    time = null,
    quote = null,
    edited = false,
    onreply,
    onedit
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
  } = $props();

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
      {#if onreply || onedit}
        <span class="message-actions">
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
    <p dir="auto">{body}</p>
  </div>
</article>
