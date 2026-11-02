<script lang="ts">
  import type { TimelineItem } from '../../crates/contracts/generated/contracts';
  import { pinSummary } from './roomTools';
  let { item, eventId = '', canManage, busy, onview, onunpin }: {
    item?: TimelineItem; eventId?: string; canManage: boolean; busy: boolean; onview: () => void; onunpin: () => void;
  } = $props();
  const summary = $derived(pinSummary(item, eventId));
</script>
{#if item && item.kind !== 'gap'}
  <p><bdi>{item.sender}</bdi>: <span dir="auto">{item.kind === 'message' ? item.body : item.kind === 'file' || item.kind === 'image' ? item.name : 'Message'}</span></p>
{:else}<p>Message content is unavailable.</p>{/if}
<button type="button" class="secondary" disabled={busy} aria-label={'View in timeline: ' + summary} onclick={onview}>View in timeline</button>
<button type="button" class="secondary" disabled={busy || !canManage} aria-label={'Unpin: ' + summary} onclick={onunpin}>Unpin</button>
