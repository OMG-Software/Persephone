<script lang="ts">
  import type { VerificationView } from '../../crates/contracts/generated/contracts';

  // An emoji verification of this device with another of the account's
  // devices. Shown on its own screen while it runs, and after it ends until
  // the person continues.
  let {
    flow,
    busy,
    onaccept,
    onconfirm,
    oncancel,
    ondismiss
  }: {
    flow: VerificationView;
    busy: boolean;
    onaccept: (flowId: string) => void;
    onconfirm: (flowId: string) => void;
    oncancel: (flowId: string) => void;
    ondismiss: (flowId: string) => void;
  } = $props();

  // Only seven emoji can be compared; anything else is never offered for a match.
  const comparing = $derived(flow.phase === 'compare' && flow.emoji.length === 7);
  const asking = $derived(comparing || flow.phase === 'incoming');
</script>

<section class="centred-stack" aria-labelledby="verify-heading">
  <ol class="steps" aria-label="Progress">
    <li class="step done">Signed in</li>
    {#if flow.phase === 'done'}
      <li class="step done">Verify</li>
    {:else}
      <li class="step current" aria-current="step">Verify</li>
    {/if}
    <li class="step">Back up keys</li>
  </ol>
  <div class="verify-card">
    <h2 id="verify-heading">Verify this device</h2>
    <div
      class="verification"
      role={asking ? 'alertdialog' : null}
      aria-labelledby={asking ? 'verification-question' : null}
    >
      {#if flow.phase === 'incoming'}
        <p id="verification-question">
          Your other device
          {#if flow.otherDevice}<bdi>{flow.otherDevice}</bdi>{/if}
          wants to verify this device. Accept only if you started this on your own device.
        </p>
        <div class="actions">
          <button type="button" disabled={busy} class="secondary" onclick={() => oncancel(flow.flowId)}
            >Decline</button
          >
          <button type="button" disabled={busy} class="primary" onclick={() => onaccept(flow.flowId)}
            >Accept</button
          >
        </div>
      {:else if flow.phase === 'waiting' || (flow.phase === 'compare' && !comparing)}
        <p role="status">Waiting for your other device. Accept the request there.</p>
        <div class="actions">
          <button type="button" disabled={busy} class="secondary" onclick={() => oncancel(flow.flowId)}
            >Cancel</button
          >
        </div>
      {:else if comparing}
        <p id="verification-question">Do these emoji match the ones on your other device, in the same order?</p>
        <ol class="emoji">
          {#each flow.emoji as item, index (index)}
            <li><span class="symbol" aria-hidden="true">{item.symbol}</span> {item.description}</li>
          {/each}
        </ol>
        <div class="actions">
          <button type="button" disabled={busy} class="secondary" onclick={() => oncancel(flow.flowId)}
            >They don't match</button
          >
          <button type="button" disabled={busy} class="primary" onclick={() => onconfirm(flow.flowId)}
            >They match</button
          >
        </div>
      {:else if flow.phase === 'confirmed'}
        <p role="status">Waiting for your other device to confirm.</p>
      {:else}
        <p role="status">
          {flow.phase === 'done' ? 'Verification finished.' : 'Verification was cancelled.'}
        </p>
        <div class="actions">
          <button type="button" class="primary" onclick={() => ondismiss(flow.flowId)}>Continue</button>
        </div>
      {/if}
    </div>
  </div>
</section>
