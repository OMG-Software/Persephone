<script lang="ts">
  import type { RoomDraft } from './bridge';
  let { roomId, draft }: { roomId: string; draft: RoomDraft } = $props();
</script>

<h3>Room: <bdi>{roomId}</bdi></h3>
<pre class="draft-preview" dir="auto">{draft.body}</pre>
{#if draft.replyTo}<p>Reply to event: <bdi>{draft.replyTo}</bdi></p>{/if}
