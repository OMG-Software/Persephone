<script lang="ts">
  import type { TimelineItem } from './bridge';
  let { item, canManage, busy, onview, onunpin }: {
    item?: TimelineItem; canManage: boolean; busy: boolean; onview: () => void; onunpin: () => void;
  } = $props();
</script>
{#if item && item.kind !== 'gap'}
  <p><bdi>{item.sender}</bdi>: <span dir="auto">{item.kind === 'message' ? item.body : item.kind === 'file' || item.kind === 'image' ? item.name : 'Message'}</span></p>
{:else}<p>Message content is unavailable.</p>{/if}
<button type="button" class="secondary" disabled={busy} onclick={onview}>View in timeline</button>
<button type="button" class="secondary" disabled={busy || !canManage} onclick={onunpin}>Unpin</button>
