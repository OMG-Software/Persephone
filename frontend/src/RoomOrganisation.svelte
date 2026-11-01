<script lang="ts">
  import { onDestroy } from 'svelte';
  import { setRoomPreference, type RoomPreference } from './bridge';
  import { byteLength } from './drafts';
  let { roomId, preferences, onchanged }: {
    roomId: string; preferences: RoomPreference[]; onchanged: (preference: RoomPreference) => void;
  } = $props();
  let section = $state('');
  let favourite = $state(false);
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let active = true;
  onDestroy(() => { active = false; });
  $effect(/**
   * Restore the selected room's grouping controls from the current profile preferences.
   */ () => {
    const preference = preferences.find((entry) => entry.roomId === roomId);
    section = preference?.section ?? '';
    favourite = preference?.favourite ?? false;
    failure = null;
  });
  /** Save the profile-local room grouping after enforcing the UI byte limit. */
  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (busy || byteLength(section.trim()) > 64) return;
    const preference = { roomId, favourite, section: section.trim() || null };
    busy = true;
    failure = null;
    try {
      await setRoomPreference(preference);
      if (active) onchanged(preference);
    } catch {
      if (active) failure = 'The room group could not be saved. Use a section name up to 64 UTF-8 bytes and at most 16 sections.';
    } finally { if (active) busy = false; }
  }
</script>

<details class="room-organisation">
  <summary>Organise this room</summary>
  <form onsubmit={save}>
    <label><input type="checkbox" bind:checked={favourite} disabled={busy} /> Favourite</label>
    <label for="room-section">Section (empty means Rooms; up to 64 UTF-8 bytes)</label>
    <input id="room-section" bind:value={section} maxlength="64" disabled={busy} autocomplete="off" />
    <p class="note">Favourites appear first. A favourite appears once, even when it has a section. These groups apply to this account profile.</p>
    <button type="submit" class="secondary" disabled={busy || byteLength(section.trim()) > 64}>{busy ? 'Saving…' : 'Save room group'}</button>
    {#if byteLength(section.trim()) > 64}<p role="alert" class="error">The section name exceeds 64 UTF-8 bytes.</p>{/if}
    {#if failure}<p role="alert" class="error">{failure}</p>{/if}
  </form>
</details>
