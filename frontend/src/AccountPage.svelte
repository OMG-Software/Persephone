<script lang="ts">
  import { onMount } from 'svelte';
  import {
    addThreePid,
    changePassword,
    removeThreePid,
    requestEmailToken,
    requestPhoneToken,
    threePids,
    type AccountChange,
    type ThreePid,
    type ThreePidList
  } from './bridge';
  import { MAX_EMAIL_LENGTH, changeText, phoneNumber, plausibleEmail, refusalText } from './accountChanges';

  // The account's password, email addresses and phone numbers. Passwords and
  // phone codes are typed only in the core's own windows; this page sends
  // only the choice to sign out other sessions, and addresses, which are not
  // secret. Addresses are the homeserver's word: isolated text.
  let { native }: { native: string } = $props();

  // One change to the account at a time; the core refuses a second.
  let busy = $state<'password' | 'request' | 'add' | 'remove' | null>(null);

  let signOutOthers = $state(true);
  let passwordResult = $state<{ code: AccountChange | null; text: string } | null>(null);

  let list = $state<ThreePidList | null>(null);
  let listFailure = $state<string | null>(null);
  let email = $state('');
  let country = $state('');
  let number = $state('');
  let addressResult = $state<{ ok: boolean; text: string } | null>(null);
  // The address whose removal waits for a second press.
  let removing = $state<ThreePid | null>(null);

  const emailReady = $derived(plausibleEmail(email.trim()));
  const phone = $derived(phoneNumber(country, number));
  const canChange = $derived(list?.canChange ?? false);

  // Only the newest listing is shown; a failed one shows its failure, not
  // an older list.
  let listRequest = 0;
  async function load() {
    const request = ++listRequest;
    listFailure = null;
    try {
      const listed = await threePids();
      if (request === listRequest) list = listed;
    } catch (reason) {
      if (request === listRequest) {
        list = null;
        listFailure = refusalText(reason);
      }
    }
  }

  onMount(load);

  async function run(
    kind: NonNullable<typeof busy>,
    action: () => Promise<AccountChange>,
    report: (code: AccountChange | null, text: string) => void | Promise<void>
  ) {
    if (busy) return;
    busy = kind;
    try {
      let code: AccountChange | null = null;
      let text: string;
      try {
        code = await action();
        text = changeText(code);
      } catch (reason) {
        text = refusalText(reason);
      }
      // A report that lists the addresses again holds the next change back
      // until the list is current.
      await report(code, text);
    } finally {
      busy = null;
    }
  }

  function changeThePassword() {
    passwordResult = null;
    run('password', () => changePassword(signOutOthers), (code, text) => {
      passwordResult = {
        code,
        text:
          code === 'done'
            ? signOutOthers
              ? 'Your password is changed. Your other sessions are signed out.'
              : 'Your password is changed. Your other sessions stay signed in.'
            : text
      };
    });
  }

  function addressDone(code: AccountChange | null, text: string): Promise<void> {
    addressResult = { ok: code === 'done' || code === 'token_sent', text };
    return load();
  }

  function sendEmailLink(event: SubmitEvent) {
    event.preventDefault();
    if (!emailReady) return;
    addressResult = null;
    run('request', () => requestEmailToken(email.trim()), (code, text) => {
      if (code === 'token_sent') email = '';
      return addressDone(code, code === 'token_sent' ? 'The homeserver sent a link to that address. Open it, then press Add.' : text);
    });
  }

  function sendPhoneCode(event: SubmitEvent) {
    event.preventDefault();
    if (!phone) return;
    const { country: code, digits } = phone;
    addressResult = null;
    run('request', () => requestPhoneToken(code, digits), (result, text) => {
      if (result === 'token_sent') number = '';
      return addressDone(
        result,
        result === 'token_sent' ? 'The homeserver sent a code to that number. Press Add, then type the code in the window Persephone opens.' : text
      );
    });
  }

  function addWaiting() {
    addressResult = null;
    run('add', addThreePid, (code, text) =>
      addressDone(code, code === 'done' ? 'Added to your account.' : text)
    );
  }

  function remove(item: ThreePid) {
    if (removing !== item) {
      removing = item;
      return;
    }
    removing = null;
    addressResult = null;
    run('remove', () => removeThreePid(item.medium, item.address), (code, text) =>
      addressDone(code, code === 'done' ? 'Removed from your account.' : text)
    );
  }

  const mediumText = { email: 'Email', msisdn: 'Phone' } as const;
</script>

<section class="settings-page" aria-labelledby="account-heading">
  <h2 id="account-heading">Account</h2>
  <p class="lede">
    Your password, and the email addresses and phone numbers on your account. Persephone asks for
    passwords and codes in its own {native} windows, never on this page.
  </p>

  <div class="panel">
    <h3>Password</h3>
    <label class="switch-row">
      <input type="checkbox" bind:checked={signOutOthers} disabled={busy !== null} />
      <span>Sign out my other sessions</span>
    </label>
    <p class="note">
      {signOutOthers
        ? 'Every other device signed in to this account is signed out. This one stays signed in.'
        : 'Your other devices stay signed in with the old password until they sign out.'}
    </p>
    <div class="actions">
      <button type="button" class="primary" disabled={busy !== null} onclick={changeThePassword}>
        {busy === 'password' ? 'Waiting for the windows…' : 'Change password…'}
      </button>
    </div>
    {#if busy === 'password'}
      <p class="note" role="status">
        Enter your current password, then the new one twice, in the windows Persephone opened.
      </p>
    {/if}
    {#if passwordResult}
      <p role={passwordResult.code === 'done' ? 'status' : 'alert'} class={passwordResult.code === 'done' ? 'note' : 'error'}>
        {passwordResult.text}
      </p>
    {/if}
  </div>

  <div class="panel">
    <h3>Email addresses and phone numbers</h3>
    <p class="note">
      Your homeserver can use them to reset your password. Persephone does not share them with an
      identity server, so people cannot find you by them.
    </p>
    {#if list}
      {#if list.threePids.length === 0}
        <p>None yet.</p>
      {:else}
        <ul class="device-list" aria-label="Email addresses and phone numbers">
          <!-- Keyed by position: two projected addresses can match. -->
          {#each list.threePids as item, index (index)}
            <li class="account-row">
              <span>{mediumText[item.medium]} <bdi class="mono">{item.address}</bdi></span>
              <button
                type="button"
                class="secondary"
                disabled={busy !== null || !canChange}
                onclick={() => remove(item)}
              >
                {removing === item ? 'Remove it?' : 'Remove'}
              </button>
            </li>
          {/each}
        </ul>
        {#if list.truncated}
          <p class="note">Showing the first {list.threePids.length}.</p>
        {/if}
      {/if}
      {#if !canChange}
        <p class="note">This homeserver does not let this account change its addresses.</p>
      {/if}
      {#if list.pending}
        <p role="status">
          Waiting to add {mediumText[list.pending.medium].toLowerCase()}
          <bdi class="mono">{list.pending.address}</bdi>.
        </p>
        <div class="actions">
          <button type="button" class="primary" disabled={busy !== null} onclick={addWaiting}>
            {busy === 'add' ? 'Waiting for the windows…' : 'Add'}
          </button>
        </div>
      {/if}
    {:else if listFailure}
      <p role="alert" class="error">{listFailure}</p>
      <div class="actions">
        <button type="button" class="secondary" onclick={load}>Try again</button>
      </div>
    {:else}
      <p class="note" role="status">Listing…</p>
    {/if}

    {#if canChange}
      <form class="add-address" onsubmit={sendEmailLink}>
        <label for="new-email">Add an email address</label>
        <div class="field-row">
          <input
            id="new-email"
            class="field"
            type="email"
            autocomplete="email"
            maxlength={MAX_EMAIL_LENGTH}
            disabled={busy !== null}
            bind:value={email}
          />
          <button type="submit" class="secondary" disabled={busy !== null || !emailReady}>Send link</button>
        </div>
      </form>
      <form class="add-address" onsubmit={sendPhoneCode}>
        <label for="new-phone">Add a phone number</label>
        <div class="field-row">
          <input
            class="field country"
            type="text"
            aria-label="Country code, two letters, such as GB"
            placeholder="GB"
            maxlength="2"
            autocomplete="off"
            disabled={busy !== null}
            bind:value={country}
          />
          <input
            id="new-phone"
            class="field"
            type="tel"
            autocomplete="tel-national"
            maxlength="40"
            disabled={busy !== null}
            bind:value={number}
          />
          <button type="submit" class="secondary" disabled={busy !== null || !phone}>Send code</button>
        </div>
        <p class="note">The two-letter country code, then the number as you dial it in that country.</p>
      </form>
    {/if}
    {#if addressResult}
      <p role={addressResult.ok ? 'status' : 'alert'} class={addressResult.ok ? 'note' : 'error'}>
        {addressResult.text}
      </p>
    {/if}
  </div>
</section>
