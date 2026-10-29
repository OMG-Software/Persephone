<script module lang="ts">
  /** What the sign-in page shows; kept by the caller, so it outlives the page. */
  export type SignInForm = {
    homeserver: string;
    mode: 'sign_in' | 'register';
    username: string;
  };
</script>

<script lang="ts">
  import { tick } from 'svelte';
  import type { AccountStatus } from '../../crates/contracts/generated/contracts';
  import markUrl from './assets/mark.svg';
  import ServerPicker from './ServerPicker.svelte';
  import ProxySettings from './ProxySettings.svelte';
  import {
    MAX_USERNAME,
    normalizeUsername,
    usernameProblem,
    usernameProblemText
  } from './registration';

  let {
    account,
    busy,
    form = $bindable(),
    onsignin,
    onregister,
    onsso
  }: {
    account: AccountStatus;
    busy: boolean;
    form: SignInForm;
    onsignin: (homeserver: string) => void;
    onregister: (homeserver: string, username: string) => void;
    // Signs in through the homeserver's own sign-in page (OIDC, ADR-0030).
    onsso?: (homeserver: string) => void;
  } = $props();

  let picking = $state(false);
  let change = $state<HTMLButtonElement | null>(null);

  // Fixed text per error code; the core sends codes only.
  const errors: Record<string, string> = {
    cancelled: 'Cancelled.',
    oidc_unavailable:
      'This server has no single sign-on, or it did not let Persephone use it. Sign in with a password instead.',
    invalid_credentials: 'The homeserver did not accept that username and password.',
    unreachable: 'The homeserver could not be reached.',
    homeserver_refused: 'Persephone does not accept that homeserver address.',
    unsupported: 'Signing in is not available on this system yet.',
    sync_failed: 'Signed in, but syncing with the homeserver failed.',
    session_not_revoked:
      'Signed out on this device, but the homeserver may not have been told. Revoke the session from another device if you need to.',
    username_taken: 'That username is taken. Choose another.',
    invalid_username: 'The homeserver does not accept that username. Choose another.',
    username_reserved: 'That username is reserved on this homeserver. Choose another.',
    registration_disabled:
      'This homeserver does not let anyone register. Ask its administrator, or choose another server.',
    token_rejected:
      'The homeserver did not accept the registration token. Check it with whoever gave it to you.',
    weak_password: 'The homeserver says the password is too weak. Try again with a stronger one.',
    unsupported_registration:
      "This homeserver asks for a step Persephone cannot do yet, such as an email address, a captcha or accepting its terms. Register in the homeserver's own web page, then sign in here.",
    rate_limited: 'Too many attempts. Wait a while, then try again.',
    failed: 'That did not work.'
  };

  const waiting = $derived(busy || account.state === 'signing_in');
  const registering = $derived(form.mode === 'register');

  // The address as typed, without its scheme, for the server row.
  const shown = $derived(form.homeserver.replace(/^https:\/\//i, ''));
  const username = $derived(normalizeUsername(form.username));
  const problem = $derived(usernameProblem(username));

  function submit(event: SubmitEvent) {
    event.preventDefault();
    if (waiting) return;
    if (!registering) {
      onsignin(form.homeserver.trim());
    } else if (username !== '' && problem === null) {
      onregister(form.homeserver.trim(), username);
    }
  }

  function switchMode() {
    form.mode = registering ? 'sign_in' : 'register';
  }

  // The page behind the modal dialog is inert until the dialog is gone.
  async function close(chosen: string | null) {
    if (chosen !== null) form.homeserver = chosen;
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
  <h2 id="sign-in-heading">{registering ? 'Create an account' : 'Sign in'}</h2>
  <p class="auth-lede">
    {registering
      ? 'Choose a username on your server. You set the password next.'
      : 'Welcome back. Pick up where you left off.'}
  </p>
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
    {#if registering}
      <label for="register-username" class="field-label">Username</label>
      <input
        id="register-username"
        class="field mono"
        type="text"
        placeholder="name"
        maxlength={MAX_USERNAME + 1}
        spellcheck="false"
        autocapitalize="none"
        autocomplete="username"
        aria-describedby="register-username-note"
        aria-invalid={problem !== null}
        disabled={waiting}
        bind:value={form.username}
      />
      {#if problem}
        <p id="register-username-note" class="error">{usernameProblemText[problem]}</p>
      {:else}
        <p id="register-username-note" class="auth-note">
          {username
            ? `Your Matrix ID will start with @${username}. Persephone checks that the name is free first.`
            : 'Letters, digits and . _ = - / + only.'}
        </p>
      {/if}
      <p class="auth-note">
        Persephone then asks for the new password, twice, and a registration token if the server needs
        one, in separate Windows windows. This page never sees them.
      </p>
    {:else}
      <p class="auth-note">
        Persephone asks for your username and password in a separate Windows sign-in window, which
        names the homeserver. This page never sees your password.
      </p>
    {/if}
    <button
      type="submit"
      class="primary wide"
      disabled={waiting || (registering && (username === '' || problem !== null))}
    >
      {waiting ? 'Waiting…' : registering ? 'Create account' : 'Continue'}
    </button>
  </form>
  {#if !registering && onsso}
    <button
      type="button"
      class="secondary wide"
      disabled={waiting}
      onclick={() => onsso(form.homeserver.trim())}
    >
      Sign in with single sign-on
    </button>
    <p class="auth-note">
      Single sign-on opens your server's own sign-in page in your browser, after Persephone asks.
      Enter your password there.
    </p>
  {/if}
  <details class="connection">
    <summary>Connection</summary>
    <ProxySettings />
  </details>
  <p class="auth-switch">
    {registering ? 'Already have an account?' : 'New to this server?'}
    <button type="button" class="link" disabled={waiting} onclick={switchMode}>
      {registering ? 'Sign in' : 'Create an account'}
    </button>
  </p>
  {#if account.state === 'signing_in'}
    <p role="status">
      {registering
        ? 'Checking the name, then waiting for the Windows windows…'
        : 'Waiting for the Windows sign-in window…'}
    </p>
  {:else if account.state === 'sign_in_failed' && account.error}
    <p role="alert" class="error">{errors[account.error] ?? errors.failed}</p>
  {:else if account.error === 'session_not_revoked'}
    <p role="alert" class="error">{errors.session_not_revoked}</p>
  {/if}
</section>

{#if picking}
  <ServerPicker current={form.homeserver} onchoose={close} oncancel={() => close(null)} />
{/if}
