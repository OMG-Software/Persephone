<script lang="ts">
  import { onMount } from 'svelte';
  import { addProfile, profileList, switchProfile, type ProfileList } from './bridge';

  // The accounts on this computer. Each has its own encrypted profile and
  // passphrase, and only one is open at a time. Nothing names an account
  // outside its encrypted profile, so they are numbered (ADR-0015).
  // Switching closes this window: the core locks this account, then starts
  // again with the other one and asks for its passphrase.
  let { compact = false }: { compact?: boolean } = $props();

  let profiles = $state<ProfileList | null>(null);
  // The slot to switch to while its question is open, or 'new'.
  let asking = $state<number | 'new' | null>(null);
  let switching = $state(false);
  let failure = $state<string | null>(null);

  const failures: Record<string, string> = {
    not_found: 'That account no longer exists on this computer.',
    limit_exceeded:
      'No more accounts can be added, a switch is already starting, or Persephone is still finishing a request. Try again in a moment.',
    invalid_argument: 'That account is already open.'
  };

  const label = (slot: number) => `Account ${slot}`;

  onMount(async () => {
    try {
      profiles = await profileList();
    } catch {
      // Without the list, switching is not offered.
    }
  });

  async function confirm() {
    const target = asking;
    if (target === null || switching) return;
    switching = true;
    failure = null;
    try {
      await (target === 'new' ? addProfile() : switchProfile(target));
      // The core closes this window next.
    } catch (reason) {
      failure = failures[String(reason)] ?? 'The switch did not start.';
      switching = false;
      asking = null;
    }
  }
</script>

{#if profiles && (profiles.profiles.length > 1 || profiles.canAdd)}
  <section class={compact ? 'accounts compact' : 'settings-page accounts'} aria-labelledby="accounts-heading">
    {#if compact}
      <h2 id="accounts-heading" class="section-label">Accounts on this computer</h2>
    {:else}
      <h2 id="accounts-heading">Accounts</h2>
      <p class="lede">
        Each account has its own encrypted data and passphrase on this computer. One is open at a
        time.
      </p>
    {/if}
    <ul class="account-list">
      {#each profiles.profiles as profile (profile.slot)}
        <li class="account-row">
          <span>{label(profile.slot)}</span>
          {#if profile.active}
            <span class="badge">Open now</span>
          {:else}
            <button
              type="button"
              class="secondary"
              disabled={switching}
              onclick={() => (asking = profile.slot)}
            >
              Switch <span class="visually-hidden">to {label(profile.slot)}</span>
            </button>
          {/if}
        </li>
      {/each}
    </ul>
    {#if asking !== null}
      <div class="confirm" role="alertdialog" aria-labelledby="switch-question">
        <p id="switch-question">
          {asking === 'new'
            ? 'Persephone closes this window, locks this account and starts again with a new, empty account. You choose its passphrase, then sign in.'
            : `Persephone closes this window, locks this account and starts again with ${label(asking)}. You enter that account's passphrase.`}
        </p>
        <button type="button" class="primary" disabled={switching} onclick={confirm}>
          {switching ? 'Switching…' : asking === 'new' ? 'Add account' : 'Switch'}
        </button>
        <button type="button" class="secondary" disabled={switching} onclick={() => (asking = null)}>
          Cancel
        </button>
      </div>
    {:else if profiles.canAdd}
      <div class="actions">
        <button type="button" class="secondary" disabled={switching} onclick={() => (asking = 'new')}>
          Add account
        </button>
      </div>
    {/if}
    {#if switching}
      <p class="note" role="status">Switching accounts. This window closes in a moment.</p>
    {/if}
    {#if failure}
      <p role="alert" class="error">{failure}</p>
    {/if}
  </section>
{/if}
