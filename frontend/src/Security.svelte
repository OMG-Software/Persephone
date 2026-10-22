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
    onsetuprecovery
  }: {
    security: SecurityStatus;
    busy: boolean;
    onsetup: () => void;
    onrecover: () => void;
    onsetuprecovery: () => void;
  } = $props();

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

  const waiting = $derived(busy || security.operation !== 'idle');
</script>

<section class="security" aria-labelledby="security-heading">
  <h2 id="security-heading">Encryption</h2>
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
    <button type="button" disabled={waiting} onclick={onsetup}>Set up encryption</button>
  {:else if security.identity === 'unverified'}
    <p>
      This device is not verified. Verify it to read and send encrypted messages: other people's
      devices do not share keys with an unverified device.
    </p>
    <button type="button" disabled={waiting} onclick={onrecover}>Use recovery key</button>
  {:else}
    <p>This device is verified.</p>
    {#if security.recovery === 'disabled' && security.canSetUpRecovery}
      <p>
        Without recovery, your encrypted messages cannot be read on a new device if this one is
        lost.
      </p>
      <button type="button" disabled={waiting} onclick={onsetuprecovery}>Set up recovery</button>
    {/if}
  {/if}
  {#if recoveryText[security.recovery]}
    <p>{recoveryText[security.recovery]}</p>
  {/if}
  {#if security.error}
    <p role="alert" class="error">{errorText[security.error] ?? errorText.failed}</p>
  {/if}
</section>
