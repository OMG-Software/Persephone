<script lang="ts">
  import { onDestroy } from 'svelte';
  import { setRoomPreference, type RoomPreference } from './bridge';
  let { preferences, joinedRooms, onchanged }: {
    preferences: RoomPreference[]; joinedRooms: string[]; onchanged: (preference: RoomPreference) => void;
  } = $props();
  const stale = $derived(preferences.filter((entry) => !joinedRooms.includes(entry.roomId)));
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let active = true;
  onDestroy(() => { active = false; });
  /** Remove a stale profile preference; this operation does not change room membership. */
  async function remove(roomId: string) {
    if (busy) return;
    busy = true;
    failure = null;
    const preference = { roomId, favourite: false, section: null };
    try { await setRoomPreference(preference); if (active) onchanged(preference); }
    catch { if (active) failure = 'The saved room group could not be removed.'; }
    finally { if (active) busy = false; }
  }
</script>

{#if stale.length > 0}
  <details class="room-organisation">
    <summary>Saved groups for rooms outside this room list ({stale.length})</summary>
    <p class="note">Remove a saved group to free room or section capacity. This only changes the local profile. It does not leave or rejoin a room.</p>
    <ul>{#each stale as preference (preference.roomId)}
      <li><bdi>{preference.roomId}</bdi> — {preference.favourite ? 'Favourite' : ''} <bdi>{preference.section ?? 'Rooms'}</bdi>
        <button type="button" class="link" disabled={busy} onclick={() => remove(preference.roomId)}>Remove saved group</button>
      </li>
    {/each}</ul>
    {#if failure}<p role="alert" class="error">{failure}</p>{/if}
  </details>
{/if}
