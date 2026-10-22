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
    onverify,
    onaccept,
    onconfirm,
    oncancel
  }: {
    security: SecurityStatus;
    busy: boolean;
    onsetup: () => void;
    onrecover: () => void;
    onsetuprecovery: () => void;
    onverify: () => void;
    onaccept: (flowId: string) => void;
    onconfirm: (flowId: string) => void;
    oncancel: (flowId: string) => void;
  } = $props();

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
    {#if !flowOpen}
      <button type="button" disabled={waiting} onclick={onverify}>Verify with another device</button>
      <button type="button" disabled={waiting} onclick={onrecover}>Use recovery key</button>
    {/if}
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
  {#if flow}
    <div class="verification" role={flow.phase === 'compare' || flow.phase === 'incoming' ? 'alertdialog' : null}>
      {#if flow.phase === 'incoming'}
        <p>
          Your other device
          {#if flow.otherDevice}<bdi>{flow.otherDevice}</bdi>{/if}
          wants to verify this device. Accept only if you started this on your own device.
        </p>
        <button type="button" disabled={busy} onclick={() => onaccept(flow.flowId)}>Accept</button>
        <button type="button" disabled={busy} onclick={() => oncancel(flow.flowId)}>Decline</button>
      {:else if flow.phase === 'waiting'}
        <p role="status">Waiting for your other device. Accept the request there.</p>
        <button type="button" disabled={busy} onclick={() => oncancel(flow.flowId)}>Cancel</button>
      {:else if flow.phase === 'compare'}
        <p>Do these emoji match the ones on your other device, in the same order?</p>
        <ol class="emoji">
          {#each flow.emoji as item, index (index)}
            <li><span class="symbol" aria-hidden="true">{item.symbol}</span> {item.description}</li>
          {/each}
        </ol>
        <button type="button" disabled={busy} onclick={() => onconfirm(flow.flowId)}>They match</button>
        <button type="button" disabled={busy} onclick={() => oncancel(flow.flowId)}>They don't match</button>
      {:else if flow.phase === 'confirmed'}
        <p role="status">Waiting for your other device to confirm.</p>
      {:else if flow.phase === 'done'}
        <p role="status">Verification finished.</p>
      {:else}
        <p role="status">Verification was cancelled.</p>
      {/if}
    </div>
  {/if}
  {#if recoveryText[security.recovery]}
    <p>{recoveryText[security.recovery]}</p>
  {/if}
  {#if security.error}
    <p role="alert" class="error">{errorText[security.error] ?? errorText.failed}</p>
  {/if}
</section>
