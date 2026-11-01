<script lang="ts">
  import { onDestroy } from 'svelte';
  import { reportAbuse } from './bridge';
  import { byteLength } from './drafts';
  import FeatureDialog from './FeatureDialog.svelte';
  let { roomId, eventId, sender, onclose, onmanageignore }: {
    roomId: string; eventId: string; sender: string; onclose: () => void; onmanageignore: () => void;
  } = $props();
  let reason = $state('');
  let busy = $state(false);
  let sent = $state(false);
  let failure = $state<string | null>(null);
  let active = true;
  onDestroy(() => { active = false; reason = ''; });
  /** Send one bounded report and ignore completion after this dialog closes. */
  async function report(event: SubmitEvent) {
    event.preventDefault();
    if (busy || sent || !reason.trim() || byteLength(reason) > 1000) return;
    busy = true;
    failure = null;
    try { await reportAbuse(roomId, eventId, reason); if (active) sent = true; }
    catch { if (active) failure = 'The report could not be sent. You can try again.'; }
    finally { if (active) busy = false; }
  }
</script>

<FeatureDialog title="Report message" {onclose}>
  <p>Sender display text: <bdi>{sender}</bdi></p>
  <p>Room: <bdi>{roomId}</bdi></p><p>Event: <bdi>{eventId}</bdi></p>
  <p>Your own homeserver receives this room ID, event ID, and reason. Its operator can read the reason. A report does not ignore the sender or remove the message.</p>
  {#if sent}<p role="status">The report was sent to your homeserver.</p>
  {:else}
    <form onsubmit={report}>
      <label for="report-reason">Reason (required; up to 1000 UTF-8 bytes)</label>
      <textarea id="report-reason" bind:value={reason} maxlength="1000" required disabled={busy} dir="auto"></textarea>
      {#if byteLength(reason) > 1000}<p class="error" role="alert">The reason exceeds 1000 UTF-8 bytes.</p>{/if}
      <button type="submit" class="danger" disabled={busy || !reason.trim() || byteLength(reason) > 1000}>{busy ? 'Sending…' : 'Confirm and send report'}</button>
    </form>
  {/if}
  <p>Ignoring is a separate action. The sender keeps access to shared rooms and can still read messages there. Room state changes still apply.</p>
  <p>Use the exact Matrix ID in the ignored-user list. The sender text above can be shortened and is not used as an account ID.</p>
  <button type="button" class="secondary" disabled={busy} onclick={onmanageignore}>Manage ignored users</button>
  {#if failure}<p role="alert" class="error">{failure}</p>{/if}
</FeatureDialog>
