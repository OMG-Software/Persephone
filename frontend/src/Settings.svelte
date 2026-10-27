<script module lang="ts">
  export type SettingsTab =
    | 'profile'
    | 'account'
    | 'security'
    | 'devices'
    | 'preferences'
    | 'accounts'
    | 'session';
</script>

<script lang="ts">
  import type { Snippet } from 'svelte';
  import Icon from './Icon.svelte';
  import { avatarColour, initials, localpart } from './people';

  let {
    userId,
    tab,
    ontab,
    onback,
    children
  }: {
    userId: string | null;
    tab: SettingsTab;
    ontab: (tab: SettingsTab) => void;
    onback: () => void;
    children: Snippet;
  } = $props();

  const tabs: { id: SettingsTab; label: string }[] = [
    { id: 'profile', label: 'Profile' },
    { id: 'account', label: 'Account' },
    { id: 'security', label: 'Security & encryption' },
    { id: 'devices', label: 'Sessions' },
    { id: 'preferences', label: 'Preferences' },
    { id: 'accounts', label: 'Accounts' },
    { id: 'session', label: 'This session' }
  ];
</script>

<div class="settings">
  <aside class="settings-rail">
    <button type="button" class="back" onclick={onback}><Icon name="back" />Back to chat</button>
    {#if userId}
      <div class="settings-account">
        <span class="avatar small {avatarColour(userId)}" aria-hidden="true">{initials(userId)}</span>
        <div class="account-text">
          <bdi class="account-name">{localpart(userId)}</bdi>
          <bdi class="account-id">{userId}</bdi>
        </div>
      </div>
    {/if}
    <nav aria-label="Settings">
      {#each tabs as item (item.id)}
        <button
          type="button"
          class="room-row"
          aria-current={tab === item.id ? 'page' : undefined}
          onclick={() => ontab(item.id)}
        >
          {item.label}
        </button>
      {/each}
    </nav>
  </aside>
  <div class="settings-body">
    {@render children()}
  </div>
</div>
