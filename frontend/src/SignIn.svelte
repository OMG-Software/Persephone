<script lang="ts">
  import type { AccountStatus } from '../../crates/contracts/generated/contracts';

  let {
    account,
    busy,
    onsignin
  }: {
    account: AccountStatus;
    busy: boolean;
    onsignin: (homeserver: string) => void;
  } = $props();

  let homeserver = $state('https://');

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

  function submit(event: SubmitEvent) {
    event.preventDefault();
    onsignin(homeserver.trim());
  }
</script>

<section class="timeline sign-in" aria-labelledby="sign-in-heading">
  <h2 id="sign-in-heading">Sign in</h2>
  <form onsubmit={submit}>
    <label for="homeserver">Homeserver address</label>
    <input
      id="homeserver"
      type="url"
      required
      maxlength="2048"
      autocomplete="url"
      spellcheck="false"
      bind:value={homeserver}
      disabled={waiting}
    />
    <button type="submit" disabled={waiting}>{waiting ? 'Waiting…' : 'Sign in'}</button>
  </form>
  <p>
    Persephone asks for your username and password in a separate Windows sign-in window, which
    names the homeserver. This page never sees your password.
  </p>
  {#if account.state === 'signing_in'}
    <p role="status">Waiting for the Windows sign-in window…</p>
  {:else if account.state === 'sign_in_failed' && account.error}
    <p role="alert" class="error">{errors[account.error] ?? errors.failed}</p>
  {:else if account.error === 'session_not_revoked'}
    <p role="alert" class="error">{errors.session_not_revoked}</p>
  {/if}
</section>
