<script lang="ts">
  import type { RoomIdentityChanges } from '../../crates/contracts/generated/contracts';

  // Members of the open room whose encryption identity changed (ADR-0007).
  // Persephone sends nothing here until the user accepts each new identity,
  // which the core confirms in its own window, or verifies the person again.
  let {
    changes,
    contact = null,
    canVerify = false,
    busy,
    confirming,
    failure = null,
    onaccept,
    onverify
  }: {
    changes: RoomIdentityChanges;
    // The direct-message contact of this room, who can be verified again here.
    contact?: string | null;
    canVerify?: boolean;
    busy: boolean;
    // The core's window is asking right now.
    confirming: boolean;
    // Why the last "Accept new identity" in this room did not work.
    failure?: string | null;
    onaccept: (userId: string) => void;
    onverify: () => void;
  } = $props();
</script>

<section class="identity-changes" role="alert" aria-labelledby="identity-changes-heading">
  <h3 id="identity-changes-heading">Sending is paused in this room</h3>
  <p>
    {changes.changes.length === 1 && !changes.truncated
      ? "This person's encryption identity changed."
      : "These people's encryption identities changed."}
    This happens when someone resets their encryption, and also when someone who controls the server
    tries to read your messages. Ask them in person or on a call you trust before you accept.
  </p>
  <ul>
    {#each changes.changes as change (change.userId)}
      <li>
        <span class="who">
          <bdi>{change.userId}</bdi>
          {#if change.wasVerified}<span class="was-verified"
              >· was verified; accepting withdraws that, and you can verify them again after</span
            >{/if}
        </span>
        <span class="actions">
          <!-- A verified contact's change blocks every encrypted message in
               the room, the verification request too: accept first. -->
          {#if contact !== null && change.userId === contact && canVerify && !change.wasVerified}
            <button type="button" class="secondary" disabled={busy} onclick={onverify}>
              Verify instead
            </button>
          {/if}
          <button type="button" class="secondary" disabled={busy} onclick={() => onaccept(change.userId)}>
            Accept new identity… <span class="visually-hidden"><bdi>{change.userId}</bdi></span>
          </button>
        </span>
      </li>
    {/each}
  </ul>
  {#if changes.truncated}
    <p>More people changed identity than are listed here; sending stays paused until all are accepted.</p>
  {/if}
  {#if confirming}
    <p role="status">Answer in the Windows window Persephone opened.</p>
  {:else if failure}
    <p role="alert" class="error">{failure}</p>
  {/if}
</section>
