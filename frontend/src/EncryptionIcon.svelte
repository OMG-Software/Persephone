<script lang="ts">
  import type { RoomEncryption } from '../../crates/contracts/generated/contracts';

  // The same icon marks a room's encryption in the room list, the chat header
  // and the composer. "unknown" makes neither claim: the core refuses to send
  // until the room's state is known.
  let {
    encryption,
    size = 'small'
  }: {
    encryption: RoomEncryption;
    size?: 'small' | 'large' | 'bar';
  } = $props();

  const label = $derived(
    encryption === 'encrypted'
      ? 'Encrypted'
      : encryption === 'not_encrypted'
        ? 'Not encrypted'
        : 'Encryption unknown'
  );
</script>

<span class="enc-icon {size} {encryption}" role="img" aria-label={label} title={label}>
  <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
    <rect x="5" y="11" width="14" height="10" rx="2" />
    {#if encryption === 'not_encrypted'}
      <!-- Open shackle, lifted on the right. -->
      <path d="M8 11V7a4 4 0 0 1 7.6-1.8" />
    {:else}
      <path d="M8 11V7a4 4 0 0 1 8 0v4" />
    {/if}
  </svg>
</span>
