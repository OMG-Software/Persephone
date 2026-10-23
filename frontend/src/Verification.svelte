<script lang="ts">
  import type { VerificationView } from '../../crates/contracts/generated/contracts';

  // An emoji verification of this device with another of the account's
  // devices, or of the account with a contact (`flow.contact`). Shown on its
  // own screen while it runs, and after it ends until the person continues.
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
  const withContact = $derived(typeof flow.contact === 'string');

  // This screen replaces whatever had focus, and each phase replaces the
  // buttons of the last one, so focus moves to the heading when the screen
  // opens and whenever the phase changes; screen readers start there.
  let heading = $state<HTMLElement | null>(null);
  $effect(() => {
    void flow.flowId;
    void flow.phase;
    heading?.focus();
  });
</script>

<section class="centred-stack" aria-labelledby="verify-heading">
  {#if !withContact}
    <ol class="steps" aria-label="Progress">
      <li class="step done">Signed in</li>
      {#if flow.phase === 'done'}
        <li class="step done">Verify</li>
      {:else}
        <li class="step current" aria-current="step">Verify</li>
      {/if}
      <li class="step">Back up keys</li>
    </ol>
  {/if}
  <div class="verify-card">
    <h2 id="verify-heading" tabindex="-1" bind:this={heading}>
      {#if withContact}Verify <bdi>{flow.contact}</bdi>{:else}Verify this device{/if}
    </h2>
    <div
      class="verification"
      role={asking ? 'alertdialog' : null}
      aria-labelledby={asking ? 'verification-question' : null}
    >
      {#if flow.phase === 'incoming' && withContact}
        <p id="verification-question">
          <bdi>{flow.contact}</bdi> wants to verify each other by comparing emoji. Accept only if
          you can compare them with this person directly, in person or on a call you trust.
        </p>
        <div class="actions">
          <button type="button" disabled={busy} class="secondary" onclick={() => oncancel(flow.flowId)}
            >Decline</button
          >
          <button type="button" disabled={busy} class="primary" onclick={() => onaccept(flow.flowId)}
            >Accept</button
          >
        </div>
      {:else if flow.phase === 'incoming'}
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
        <p role="status">
          {#if withContact}
            Waiting for <bdi>{flow.contact}</bdi> to accept the request.
          {:else}
            Waiting for your other device. Accept the request there.
          {/if}
        </p>
        <div class="actions">
          <button type="button" disabled={busy} class="secondary" onclick={() => oncancel(flow.flowId)}
            >Cancel</button
          >
        </div>
      {:else if comparing}
        <p id="verification-question">
          {#if withContact}
            Do these emoji match the ones <bdi>{flow.contact}</bdi> sees, in the same order? Compare
            them in person or on a call you trust, not in this chat.
          {:else}
            Do these emoji match the ones on your other device, in the same order?
          {/if}
        </p>
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
        <p role="status">
          {#if withContact}
            Waiting for <bdi>{flow.contact}</bdi> to confirm.
          {:else}
            Waiting for your other device to confirm.
          {/if}
        </p>
        <!-- The confirmation may not have reached the other side (a refused
             request is not sent again), so the wait can always be ended. -->
        <div class="actions">
          <button type="button" disabled={busy} class="secondary" onclick={() => oncancel(flow.flowId)}
            >Cancel</button
          >
        </div>
      {:else}
        <p role="status">
          {#if flow.phase === 'done' && withContact}
            <bdi>{flow.contact}</bdi> is verified.
          {:else}
            {flow.phase === 'done' ? 'Verification finished.' : 'Verification was cancelled.'}
          {/if}
        </p>
        <div class="actions">
          <button type="button" class="primary" onclick={() => ondismiss(flow.flowId)}>Continue</button>
        </div>
      {/if}
    </div>
  </div>
</section>
