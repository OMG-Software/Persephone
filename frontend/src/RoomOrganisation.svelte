<script lang="ts">
  import { onDestroy } from 'svelte';
  import { setRoomPreference, type RoomPreference } from './bridge';
  import { byteLength } from './drafts';
  import FeatureDialog from './FeatureDialog.svelte';
  import { RoomGroupRestoration } from './roomGroupRestoration.ts';
  // The room's section in the room list. Favourites change with the star
  // next to the room name; saving a section keeps the saved favourite.
  // `favouriteSaving`: a favourite change for this profile is being saved.
  // Saving waits for it, because a save writes the whole preference and would
  // put back the favourite value from before that change.
  let { roomId, preferences, favouriteSaving = false, onchanged, onclose }: {
    roomId: string; preferences: RoomPreference[]; favouriteSaving?: boolean;
    onchanged: (preference: RoomPreference) => void; onclose: () => void;
  } = $props();
  let section = $state('');
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let active = true;
  onDestroy(() => { active = false; });
  const savedSection = $derived(preferences.find((entry) => entry.roomId === roomId)?.section ?? '');
  const savedFavourite = $derived(preferences.find((entry) => entry.roomId === roomId)?.favourite ?? false);
  const restoration = new RoomGroupRestoration();
  /** Restore when this room's saved section changes; a favourite-only change keeps unsaved text and errors. */
  function restoreGrouping() {
    const saved = restoration.changed(roomId, savedSection, false);
    if (!saved) return;
    section = saved.section;
    failure = null;
  }
  $effect(restoreGrouping);
  /** Save the profile-local room grouping after enforcing the UI byte limit. */
  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (busy || favouriteSaving || byteLength(section.trim()) > 64) return;
    const preference = { roomId, favourite: savedFavourite, section: section.trim() || null };
    busy = true;
    failure = null;
    try {
      await setRoomPreference(preference);
      if (active) { onchanged(preference); onclose(); }
    } catch {
      if (active) failure = 'The room group could not be saved. Use a section name up to 64 UTF-8 bytes and at most 16 sections.';
    } finally { if (active) busy = false; }
  }
</script>

<FeatureDialog title="Room section" {onclose} closeDisabled={busy}>
  <form class="room-organisation" onsubmit={save}>
    <label for="room-section">Section (empty means Rooms; up to 64 UTF-8 bytes)</label>
    <input id="room-section" bind:value={section} maxlength="64" disabled={busy} autocomplete="off" />
    <p class="note">Favourites appear first. A favourite appears once, even when it has a section. These groups apply to this account profile.</p>
    <button type="submit" class="secondary" disabled={busy || favouriteSaving || byteLength(section.trim()) > 64}>{busy ? 'Saving…' : 'Save room group'}</button>
    {#if byteLength(section.trim()) > 64}<p role="alert" class="error">The section name exceeds 64 UTF-8 bytes.</p>{/if}
    {#if failure}<p role="alert" class="error">{failure}</p>{/if}
  </form>
</FeatureDialog>
