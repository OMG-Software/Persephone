<script lang="ts">
  import type { InviteSummary } from '../../crates/contracts/generated/contracts';

  // Direct-message invitations. The inviter's ID and name are their own word:
  // shown as isolated text, never as a link or markup.
  let {
    invites,
    truncated,
    busy,
    onaccept,
    ondecline
  }: {
    invites: InviteSummary[];
    truncated: boolean;
    // The room whose invitation is being answered, if any.
    busy: string | null;
    onaccept: (roomId: string) => void;
    ondecline: (roomId: string) => void;
  } = $props();
</script>

<section class="invites" aria-labelledby="invites-label">
  <p class="section-label" id="invites-label">Invitations</p>
  <ul>
    {#each invites as invite (invite.roomId)}
      <li class="invite">
        <p class="invite-text">
          {#if invite.inviterName}
            <bdi class="invite-name">{invite.inviterName}</bdi>
            <bdi class="invite-id">{invite.inviter}</bdi>
          {:else}
            <bdi class="invite-name">{invite.inviter}</bdi>
          {/if}
          <span class="invite-what">wants to talk with you</span>
        </p>
        <div class="invite-actions">
          <button
            type="button"
            class="secondary"
            disabled={busy !== null}
            onclick={() => onaccept(invite.roomId)}>Accept</button
          >
          <button
            type="button"
            class="secondary"
            disabled={busy !== null}
            onclick={() => ondecline(invite.roomId)}>Decline</button
          >
        </div>
      </li>
    {/each}
  </ul>
  {#if truncated}
    <p class="note">Showing the first {invites.length} invitations.</p>
  {/if}
</section>
