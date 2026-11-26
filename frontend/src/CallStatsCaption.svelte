<script lang="ts">
  import type { StatsSection } from './callStats';
  import PersonName from './PersonName.svelte';

  // The caption of one table of the technical details (ADR-0066): a part of
  // the call by its title, or another device by its user ID and device ID,
  // so one person's two devices can be told apart. Both IDs are chosen by the
  // peer, so each is isolated: neither can reorder the other or the caption.
  let { section }: { section: StatsSection } = $props();
</script>

{#if section.userId !== null}<PersonName userId={section.userId} />{#if section.deviceId !== null}
    <span class="call-stats-device">(<span class="visually-hidden">{'device '}</span><bdi>{section.deviceId}</bdi>)</span
    >{/if}{:else}{section.title}{/if}
