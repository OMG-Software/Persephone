<script lang="ts">
  import { avatarColour, initials } from './people';
  import { clockTime, fullTime, toDate } from './time';

  let { name, body, time = null }: { name: string; body: string; time?: number | null } = $props();

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
    </div>
    <p dir="auto">{body}</p>
  </div>
</article>
