<script lang="ts">
  import type {
    RecoveryState,
    SecurityError,
    SecurityStatus
  } from '../../crates/contracts/generated/contracts';

  let {
    security,
    busy,
    onsetup,
    onrecover,
    onsetuprecovery,
    onverify
  }: {
    security: SecurityStatus;
    busy: boolean;
    onsetup: () => void;
    onrecover: () => void;
    onsetuprecovery: () => void;
    onverify: () => void;
  } = $props();

  // A running verification has its own screen; no second one starts meanwhile.
  const flow = $derived(security.verification);
  const flowOpen = $derived(flow !== null && flow.phase !== 'done' && flow.phase !== 'cancelled');

  // Fixed text per code; the core sends codes only. Secrets never reach this
  // page: the core asks for them, and shows the recovery key, in its own
  // Windows windows.
  const recoveryText: Record<RecoveryState, string> = {
    unknown: '',
    disabled: 'Recovery is not set up.',
    incomplete: 'Recovery is set up, but this device does not have all of its keys.',
    enabled: 'Recovery is set up.'
  };
  const errorText: Record<SecurityError, string> = {
    cancelled: 'Cancelled.',
    unsupported: 'This system has no Persephone sign-in window yet.',
    invalid_password: 'The homeserver did not accept that password.',
    identity_exists:
      'This account already has encryption set up. Verify this device with your recovery key instead.',
    wrong_recovery_key: 'That recovery key did not work.',
    backup_exists_on_server:
      'The homeserver already has a key backup that Persephone cannot use yet.',
    recovery_key_not_shown:
      'Recovery was set up, but its key could not be shown. Do not rely on it until it is set up again.',
    unreachable: 'The homeserver could not be reached.',
    failed: 'That did not work.'
  };

  // Health at a glance: "good" is green, "attention" is amber.
  type Health = { title: string; value: string; tone: 'good' | 'attention' | 'unknown' };
  const checking = { value: 'Checking…', tone: 'unknown' } as const;
  const health = $derived<Health[]>([
    {
      title: 'Cross-signing',
      ...(security.identity === 'unknown'
        ? checking
        : security.identity === 'none'
          ? { value: 'Not set up', tone: 'attention' }
          : { value: 'Ready', tone: 'good' })
    },
    {
      title: 'This device',
      ...(security.identity === 'unknown'
        ? checking
        : security.identity === 'verified'
          ? { value: 'Verified', tone: 'good' }
          : { value: 'Not verified', tone: 'attention' })
    },
    {
      title: 'Recovery',
      ...(security.recovery === 'unknown'
        ? checking
        : security.recovery === 'enabled'
          ? { value: 'Set up', tone: 'good' }
          : security.recovery === 'incomplete'
            ? { value: 'Incomplete', tone: 'attention' }
            : { value: 'Not set up', tone: 'attention' })
    }
  ]);

  const waiting = $derived(busy || security.operation !== 'idle');
</script>

<section class="settings-page security" aria-labelledby="security-heading">
  <h2 id="security-heading">Security &amp; encryption</h2>
  <p class="lede">
    Messages in encrypted rooms are end-to-end encrypted. Only you and the people you talk to hold
    the keys.
  </p>
  <ul class="health" aria-label="Encryption health">
    {#each health as item (item.title)}
      <li>
        <span class="health-title">{item.title}</span>
        <span class="health-value {item.tone}">{item.value}</span>
      </li>
    {/each}
  </ul>
  <div class="panel">
    {#if security.operation === 'waiting_for_password'}
      <p role="status">Enter your password in the Windows window Persephone opened.</p>
    {:else if security.operation === 'waiting_for_recovery_key'}
      <p role="status">Enter your recovery key in the Windows window Persephone opened.</p>
    {:else if security.operation === 'working'}
      <p role="status">Working…</p>
    {/if}
    {#if security.identity === 'unknown'}
      <p>Checking this account's encryption…</p>
    {:else if security.identity === 'none'}
      <p>Encryption is not set up for this account. Set it up to send encrypted messages.</p>
      <div class="actions">
        <button type="button" disabled={waiting} class="primary" onclick={onsetup}>Set up encryption</button>
      </div>
    {:else if security.identity === 'unverified'}
      <p>
        This device is not verified. Verify it to read and send encrypted messages: other people's
        devices do not share keys with an unverified device.
      </p>
      {#if !flowOpen}
        <div class="actions">
          <button type="button" disabled={waiting} class="primary" onclick={onverify}
            >Verify with another device</button
          >
          <button type="button" disabled={waiting} class="secondary" onclick={onrecover}
            >Use recovery key</button
          >
        </div>
      {/if}
    {:else}
      <p>This device is verified.</p>
      {#if security.recovery === 'disabled' && security.canSetUpRecovery}
        <p>
          Without recovery, your encrypted messages cannot be read on a new device if this one is
          lost.
        </p>
        <div class="actions">
          <button type="button" disabled={waiting} class="primary" onclick={onsetuprecovery}
            >Set up recovery</button
          >
        </div>
      {/if}
    {/if}
    {#if recoveryText[security.recovery]}
      <p>{recoveryText[security.recovery]}</p>
    {/if}
    {#if security.error}
      <p role="alert" class="error">{errorText[security.error] ?? errorText.failed}</p>
    {/if}
  </div>
</section>
