<script lang="ts">
  import { avatarColour, initials } from './people';

  let { name, body }: { name: string; body: string } = $props();
</script>

<!-- Names are isolated with <bdi> and bodies with dir="auto", so bidi control
     characters in one message cannot reorder text outside it. All text is
     rendered as escaped text, never as HTML. -->
<article class="message" aria-label={'Message from ' + name}>
  <span class="avatar {avatarColour(name)}" aria-hidden="true">{initials(name)}</span>
  <div class="message-body">
    <strong class="sender"><bdi>{name}</bdi></strong>
    <p dir="auto">{body}</p>
  </div>
</article>
