<script lang="ts">
  import type { Snippet } from 'svelte';
  import type { InviteList, RoomList as Rooms } from '../../crates/contracts/generated/contracts';
  import AccountText from './AccountText.svelte';
  import Icon from './Icon.svelte';
  import Invites from './Invites.svelte';
  import RoomList from './RoomList.svelte';
  import { avatarColour, initials } from './people';

  let {
    rooms,
    invites = null,
    inviteBusy = null,
    selected,
    userId,
    ownName = null,
    shortcut,
    onselect,
    onsearch,
    onsettings,
    onnewconversation,
    onnewroom,
    onaccept = () => {},
    ondecline = () => {},
    notice,
    lock
  }: {
    rooms: Rooms | null;
    // Room invitations, or null when there are none to show.
    invites?: InviteList | null;
    // The room whose invitation is being answered, if any.
    inviteBusy?: string | null;
    selected: string | null;
    // The signed-in account, or null for synthetic data.
    userId: string | null;
    // The account's display name, or null when none is set.
    ownName?: string | null;
    // The jump shortcut as this system writes it, such as "Ctrl K".
    shortcut: string;
    onselect: (roomId: string) => void;
    onsearch: () => void;
    onsettings: () => void;
    // Opens the new-conversation dialog; absent when there is no account.
    onnewconversation?: () => void;
    // Opens the new-room dialog; absent when there is no account.
    onnewroom?: () => void;
    onaccept?: (roomId: string) => void;
    ondecline?: (roomId: string) => void;
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
    {#if onnewconversation}
      <button
        type="button"
        class="icon-button"
        aria-label="New conversation"
        title="New conversation"
        onclick={onnewconversation}
      >
        <Icon name="plus" />
      </button>
    {/if}
    {#if onnewroom}
      <button type="button" class="icon-button" aria-label="New room" title="New room" onclick={onnewroom}>
        <Icon name="users" />
      </button>
    {/if}
  </div>
  <div class="sidebar-scroll">
    {#if invites && invites.invites.length > 0}
      <Invites
        invites={invites.invites}
        truncated={invites.truncated}
        busy={inviteBusy}
        {onaccept}
        {ondecline}
      />
    {/if}
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
      <AccountText {userId} name={ownName} />
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
