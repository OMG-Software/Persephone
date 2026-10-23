<script lang="ts">
  import { tick } from 'svelte';
  import type { AccountStatus } from '../../crates/contracts/generated/contracts';
  import markUrl from './assets/mark.svg';
  import ServerPicker from './ServerPicker.svelte';

  let {
    account,
    busy,
    onsignin
  }: {
    account: AccountStatus;
    busy: boolean;
    onsignin: (homeserver: string) => void;
  } = $props();

  let homeserver = $state('https://matrix.org');
  let picking = $state(false);
  let change = $state<HTMLButtonElement | null>(null);

  // Fixed text per error code; the core sends codes only.
  const errors: Record<string, string> = {
    cancelled: 'Sign-in was cancelled.',
    invalid_credentials: 'The homeserver did not accept that username and password.',
    unreachable: 'The homeserver could not be reached.',
    homeserver_refused: 'Persephone does not accept that homeserver address.',
    unsupported: 'Signing in is not available on this system yet.',
    sync_failed: 'Signed in, but syncing with the homeserver failed.',
    session_not_revoked:
      'Signed out on this device, but the homeserver may not have been told. Revoke the session from another device if you need to.',
    failed: 'Signing in failed.'
  };

  const waiting = $derived(busy || account.state === 'signing_in');

  // The address as typed, without its scheme, for the server row.
  const shown = $derived(homeserver.replace(/^https:\/\//i, ''));

  function submit(event: SubmitEvent) {
    event.preventDefault();
    onsignin(homeserver.trim());
  }

  // The page behind the modal dialog is inert until the dialog is gone.
  async function close(chosen: string | null) {
    if (chosen !== null) homeserver = chosen;
    picking = false;
    await tick();
    change?.focus();
  }
</script>

<section class="auth-card sign-in" aria-labelledby="sign-in-heading">
  <div class="auth-brand">
    <img src={markUrl} alt="" width="32" height="32" />
    Persephone
  </div>
  <h2 id="sign-in-heading">Sign in</h2>
  <p class="auth-lede">Welcome back. Pick up where you left off.</p>
  <form onsubmit={submit}>
    <div class="server-row">
      <div>
        <span class="server-label">Your server</span>
        <bdi class="server-value">{shown}</bdi>
      </div>
      <button
        type="button"
        class="link"
        aria-label="Change server"
        disabled={waiting}
        bind:this={change}
        onclick={() => (picking = true)}
      >
        Change
      </button>
    </div>
    <p class="auth-note">
      Persephone asks for your username and password in a separate Windows sign-in window, which
      names the homeserver. This page never sees your password.
    </p>
    <button type="submit" class="primary wide" disabled={waiting}>
      {waiting ? 'Waiting…' : 'Continue'}
    </button>
  </form>
  {#if account.state === 'signing_in'}
    <p role="status">Waiting for the Windows sign-in window…</p>
  {:else if account.state === 'sign_in_failed' && account.error}
    <p role="alert" class="error">{errors[account.error] ?? errors.failed}</p>
  {:else if account.error === 'session_not_revoked'}
    <p role="alert" class="error">{errors.session_not_revoked}</p>
  {/if}
</section>

{#if picking}
  <ServerPicker current={homeserver} onchoose={close} oncancel={() => close(null)} />
{/if}
