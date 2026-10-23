<script lang="ts">
  import type { Snippet } from 'svelte';
  import type { RoomList as Rooms } from '../../crates/contracts/generated/contracts';
  import Icon from './Icon.svelte';
  import RoomList from './RoomList.svelte';
  import { avatarColour, initials, localpart } from './people';

  let {
    rooms,
    selected,
    userId,
    shortcut,
    onselect,
    onsearch,
    onsettings,
    notice,
    lock
  }: {
    rooms: Rooms | null;
    selected: string | null;
    // The signed-in account, or null for synthetic data.
    userId: string | null;
    // The jump shortcut as this system writes it, such as "Ctrl K".
    shortcut: string;
    onselect: (roomId: string) => void;
    onsearch: () => void;
    onsettings: () => void;
    // Shown above the account footer, such as a prompt to verify this device.
    notice?: Snippet;
    // The Lock button, when the session can be locked.
    lock?: Snippet;
  } = $props();
</script>

<aside class="sidebar" aria-label="Navigation">
  <div class="sidebar-top">
    <button
      type="button"
      class="search-trigger"
      aria-keyshortcuts={shortcut === '⌘K' ? 'Meta+K' : 'Control+K'}
      onclick={onsearch}
    >
      <Icon name="search" />
      <span>Search or jump to…</span>
      <kbd>{shortcut}</kbd>
    </button>
  </div>
  <div class="sidebar-scroll">
    {#if rooms}
      <RoomList rooms={rooms.rooms} truncated={rooms.truncated} {selected} {onselect} />
    {:else}
      <p class="note" role="status">Loading rooms…</p>
    {/if}
  </div>
  {@render notice?.()}
  <div class="account-footer">
    {#if userId}
      <span class="avatar small {avatarColour(userId)}" aria-hidden="true">{initials(userId)}</span>
      <div class="account-text">
        <bdi class="account-name">{localpart(userId)}</bdi>
        <bdi class="account-id">{userId}</bdi>
      </div>
    {:else}
      <span class="avatar small c3" aria-hidden="true">?</span>
      <div class="account-text">
        <span class="account-name">Synthetic data</span>
        <span class="account-id">No account</span>
      </div>
    {/if}
    {@render lock?.()}
    <button type="button" class="icon-button" aria-label="Settings" title="Settings" onclick={onsettings}>
      <Icon name="settings" />
    </button>
  </div>
</aside>
