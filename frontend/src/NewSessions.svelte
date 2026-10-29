<script lang="ts">
  import type { NewSessions } from './bridge';

  // New sessions of this account the user has not been shown yet
  // (ADR-0026). Each name and ID is the homeserver's or the device's word:
  // isolated text.
  let {
    found,
    busy = false,
    failure = null,
    onknown,
    onremove
  }: {
    found: NewSessions;
    busy?: boolean;
    failure?: string | null;
    // The user signed these in: stop showing them.
    onknown: () => void;
    // The user did not: sign this one out.
    onremove: (deviceId: string) => void;
  } = $props();
</script>

<section class="new-sessions" role="alert" aria-labelledby="new-sessions-heading">
  <h2 id="new-sessions-heading">
    {found.sessions.length === 1 ? 'A new session signed in' : 'New sessions signed in'} to your account
  </h2>
  <ul>
    <!-- Keyed by position: two projected IDs from the homeserver can match. -->
    {#each found.sessions as session, index (index)}
      <li>
        <bdi>{session.displayName ?? session.deviceId}</bdi>
        <span class="note">ID <bdi class="mono">{session.deviceId}</bdi></span>
        <button
          type="button"
          class="danger"
          disabled={busy}
          aria-label={`Sign out ${session.displayName ?? session.deviceId}`}
          onclick={() => onremove(session.deviceId)}>Sign it out</button
        >
      </li>
    {/each}
  </ul>
  {#if found.truncated}
    <p class="note">More new sessions are in Settings, Sessions.</p>
  {/if}
  <p class="note">
    If you did not sign in on a new device, someone may have your password: sign the session out,
    then change your password.
  </p>
  {#if failure}
    <p class="error">{failure}</p>
  {/if}
  <div class="actions">
    <button type="button" class="secondary" disabled={busy} onclick={onknown}>It was me</button>
  </div>
</section>
