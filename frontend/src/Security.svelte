<script lang="ts">
  import type { RecoveryState, SecurityStatus } from '../../crates/contracts/generated/contracts';
  import { securityErrorText } from './securityText';

  let {
    security,
    busy,
    onsetup,
    onrecover,
    onsetuprecovery,
    onverify,
    onchangekey = () => {},
    onpassphrase = () => {},
    onnewbackup = () => {}
  }: {
    security: SecurityStatus;
    busy: boolean;
    onsetup: () => void;
    onrecover: () => void;
    onsetuprecovery: () => void;
    onverify: () => void;
    // Replaces the recovery key; the core warns, shows and saves it.
    onchangekey?: () => void;
    // Turns the profile passphrase on or off. The core asks for it in its own
    // window; the page never sees it (ADR-0050).
    onpassphrase?: (on: boolean) => void;
    // Deletes a key backup this device cannot use and starts a new one; the
    // core asks first, then shows and saves the new key.
    onnewbackup?: () => void;
  } = $props();

  // A running verification has its own screen; no second one starts meanwhile.
  const flow = $derived(security.verification);
  const flowOpen = $derived(flow !== null && flow.phase !== 'done' && flow.phase !== 'cancelled');

  // Fixed text per code; the core sends codes only.
  const recoveryText: Record<RecoveryState, string> = {
    unknown: '',
    disabled: 'Recovery is not set up.',
    incomplete: 'Recovery is set up, but this device does not have all of its keys.',
    enabled: 'Recovery is set up.'
  };
  const errorText = securityErrorText;

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

  // Fixed text per step the core is waiting on; none while idle.
  const operationText: Partial<Record<SecurityStatus['operation'], string>> = {
    waiting_for_password: 'Enter your password in the Persephone window.',
    waiting_for_recovery_key: 'Enter your recovery key in the Persephone window.',
    waiting_for_passphrase: 'Enter the passphrase in the Persephone window.',
    waiting_for_confirmation: 'Answer the question in the Persephone window.',
    working: 'Working…'
  };

  // The recovery panel shows once the core knows the recovery state, or when a
  // verified device is offered a new backup.
  const showRecovery = $derived(
    security.recovery !== 'unknown' ||
      (security.identity === 'verified' && security.canStartNewBackup)
  );
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
  {#if operationText[security.operation]}
    <p role="status" class="security-notice">{operationText[security.operation]}</p>
  {/if}
  {#if security.error}
    <p role="alert" class="security-notice error">{errorText[security.error] ?? errorText.failed}</p>
  {/if}
  <div class="panel" role="group" aria-labelledby="device-heading">
    <h3 id="device-heading">This device</h3>
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
    {/if}
  </div>
  <div class="panel" role="group" aria-labelledby="passphrase-heading">
    <h3 id="passphrase-heading">Profile passphrase</h3>
    {#if security.profilePassphrase}
      <p>
        The passphrase is on. Persephone asks for it each time it starts. Lock is available, and
        it closes the profile until you enter the passphrase again.
      </p>
      <div class="actions">
        <button
          type="button"
          disabled={waiting}
          class="secondary"
          onclick={() => onpassphrase(false)}>Turn off passphrase</button
        >
      </div>
    {:else}
      <p>
        The passphrase is off. Persephone opens this profile without asking. Its stores are still
        encrypted, and the key is bound to this computer account. But anyone, and any program,
        that can use this computer account can open the profile.
      </p>
      <div class="actions">
        <button
          type="button"
          disabled={waiting}
          class="secondary"
          onclick={() => onpassphrase(true)}>Turn on passphrase</button
        >
      </div>
    {/if}
  </div>
  {#if showRecovery}
    <div class="panel" role="group" aria-labelledby="recovery-heading">
      <h3 id="recovery-heading">Recovery</h3>
      {#if recoveryText[security.recovery]}
        <p>{recoveryText[security.recovery]}</p>
      {/if}
      {#if security.identity === 'verified'}
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
        {#if security.canStartNewBackup}
          <p>
            {security.recovery === 'incomplete'
              ? 'This device cannot use the key backup yet. Your recovery key may give it the key.'
              : 'The homeserver has a key backup this device cannot use.'}
            If you no longer have the key, start a new backup.
            Messages kept only in the old backup can then not be read.
          </p>
          <div class="actions">
            {#if security.recovery === 'incomplete'}
              <button type="button" disabled={waiting} class="primary" onclick={onrecover}
                >Use recovery key</button
              >
            {/if}
            <button type="button" disabled={waiting} class="secondary" onclick={onnewbackup}
              >Start a new backup…</button
            >
          </div>
        {/if}
        {#if security.recovery === 'enabled'}
          <p>
            Persephone does not keep your recovery key, so it cannot show it again. To get a key you
            can save, make a new one: Persephone shows it in its own window and offers to save it to
            a file. The old key stops working.
          </p>
          <div class="actions">
            <button type="button" disabled={waiting} class="secondary" onclick={onchangekey}
              >Change recovery key…</button
            >
          </div>
        {/if}
      {/if}
    </div>
  {/if}
</section>
