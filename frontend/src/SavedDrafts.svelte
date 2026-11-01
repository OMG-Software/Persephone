<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { roomDrafts, roomDraft, saveRoomDraft, type RoomDraft } from './bridge';
  import FeatureDialog from './FeatureDialog.svelte';
  import DraftPreview from './DraftPreview.svelte';
  import { discardInactiveDraft } from './draftManagement';
  let { selected, onclose }: { selected: string | null; onclose: () => void } = $props();
  let rooms = $state<string[] | null>(null);
  let preview = $state<{ roomId: string; draft: RoomDraft } | null>(null);
  let confirming = $state(false);
  let busy = $state(false);
  let discarding = $state(false);
  let failure = $state<string | null>(null);
  let request = 0;
  let active = true;
  onDestroy(() => { active = false; request += 1; preview = null; rooms = null; });
  async function load() {
    const asked = ++request;
    busy = true;
    failure = null;
    try { const reply = await roomDrafts(); if (active && asked === request) rooms = reply; }
    catch { if (active && asked === request) failure = 'The saved-draft list could not be loaded.'; }
    finally { if (active && asked === request) busy = false; }
  }
  onMount(load);
  async function view(roomId: string) {
    if (busy) return;
    const asked = ++request;
    busy = true;
    preview = null;
    confirming = false;
    failure = null;
    try { const draft = await roomDraft(roomId); if (active && asked === request) preview = { roomId, draft }; }
    catch { if (active && asked === request) failure = 'This saved draft could not be loaded.'; }
    finally { if (active && asked === request) busy = false; }
  }
  async function discard() {
    if (busy || !preview || preview.roomId === selected) return;
    const roomId = preview.roomId;
    busy = true;
    discarding = true;
    failure = null;
    try {
      if (!(await discardInactiveDraft(roomId, () => selected, saveRoomDraft))) return;
      if (!active) return;
      preview = null;
      confirming = false;
      await load();
    } catch { if (active) failure = 'The saved draft could not be discarded.'; }
    finally { if (active) { busy = false; discarding = false; } }
  }
</script>

<FeatureDialog title="Saved drafts" {onclose} closeDisabled={discarding}>
  <p>These drafts are stored in this encrypted profile. This list also includes rooms you left or cannot open. Viewing a draft does not rejoin a room or send a message.</p>
  <p class="note">Unsaved text in the current room stays in its editor. If storage is full, discard another saved draft here, then close this panel and retry saving the current draft.</p>
  {#if rooms}
    <ul>{#each rooms as roomId (roomId)}
      <li><button type="button" class="link" disabled={busy} onclick={() => view(roomId)}><bdi>{roomId}</bdi></button>
        {#if roomId === selected}<span> — current room; use Discard draft in its composer.</span>{/if}
      </li>
    {/each}</ul>
    {#if rooms.length === 0}<p>No saved drafts.</p>{/if}
  {:else if !failure}<p role="status">Loading saved drafts…</p>{/if}
  {#if preview}
    <section aria-label="Saved draft preview">
      <DraftPreview roomId={preview.roomId} draft={preview.draft} />
      {#if preview.roomId === selected}<p class="note">Close this panel to edit or discard the current room's draft.</p>
      {:else if confirming}<div class="confirm" role="group" aria-label="Confirm discard saved draft">
        <p>Discard this saved draft and its reply reference?</p>
        <button type="button" class="danger" disabled={busy} onclick={discard}>Confirm discard</button>
        <button type="button" class="secondary" disabled={busy} onclick={() => (confirming = false)}>Keep draft</button>
      </div>
      {:else}<button type="button" class="secondary" disabled={busy} onclick={() => (confirming = true)}>Discard saved draft…</button>{/if}
    </section>
  {/if}
  {#if failure}<p role="alert" class="error">{failure}</p>{/if}
  <button type="button" class="secondary" disabled={busy} onclick={load}>{busy ? 'Working…' : 'Reload list'}</button>
</FeatureDialog>
