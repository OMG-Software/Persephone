<script lang="ts">
  import { onDestroy } from 'svelte';
  import type { RoomSummary } from '../../crates/contracts/generated/contracts';
  import { setRoomPreference, type RoomPreference } from './bridge';
  import FeatureDialog from './FeatureDialog.svelte';
  import {
    MAX_GROUP_NAME_BYTES, applyWrites, capacityProblem, groupNameProblem, groupNames, groupWrites,
    removeGroupWrites
  } from './roomGroupEdits.ts';

  // Creates, edits and removes room groups. A group is the section name of
  // its rooms' profile preferences (ADR-0035): it exists while one of its
  // rooms has the name. Writes go one at a time; each saved write is shown
  // at once, so a failure part way leaves an honest, partly changed state.
  // `favouriteSaving`: a favourite change is being saved. Writes wait for it,
  // because each write carries the room's favourite value.
  // `onbusy`: this dialog starts or stops saving; the owner holds favourite
  // changes meanwhile for the same reason.
  let { rooms, preferences, favouriteSaving = false, onchanged, onbusy, onclose }: {
    rooms: RoomSummary[]; preferences: RoomPreference[]; favouriteSaving?: boolean;
    onchanged: (preference: RoomPreference) => void; onbusy: (busy: boolean) => void; onclose: () => void;
  } = $props();

  type View = { kind: 'list' } | { kind: 'edit'; group: string | null };
  let view = $state<View>({ kind: 'list' });
  let name = $state('');
  let chosen = $state<string[]>([]);
  let confirming = $state<string | null>(null);
  let busy = $state(false);
  let failure = $state<string | null>(null);
  let active = true;
  onDestroy(() => {
    active = false;
    if (busy) onbusy(false);
  });

  const joined = $derived(rooms.map((room) => room.roomId));
  const groups = $derived(groupNames(preferences));
  const byRoom = $derived(new Map(preferences.map((entry) => [entry.roomId, entry])));
  const joinedCount = (group: string) => rooms.filter((room) => byRoom.get(room.roomId)?.section === group).length;
  const blocked = $derived(busy || favouriteSaving);

  function edit(group: string | null) {
    view = { kind: 'edit', group };
    name = '';
    chosen = group === null ? [] : rooms.filter((room) => byRoom.get(room.roomId)?.section === group).map((room) => room.roomId);
    confirming = null;
    failure = null;
  }

  function back() {
    view = { kind: 'list' };
    failure = null;
  }

  function toggle(roomId: string, checked: boolean) {
    chosen = checked ? [...chosen, roomId] : chosen.filter((id) => id !== roomId);
  }

  /** Save the writes; return whether all were saved. Results after the dialog closes are ignored. */
  async function run(writes: RoomPreference[]): Promise<boolean> {
    busy = true;
    onbusy(true);
    failure = null;
    const save = async (preference: RoomPreference) => {
      if (!active) throw new Error('closed');
      await setRoomPreference(preference);
    };
    const result = await applyWrites(writes, save, (preference) => { if (active) onchanged(preference); });
    if (!active) return false;
    busy = false;
    onbusy(false);
    if (result.failed) {
      failure = `${result.saved} of ${writes.length} changes were saved. The rest could not be saved. Try again; this profile keeps at most 16 groups and 100 rooms with a group or favourite.`;
    }
    return !result.failed;
  }

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (blocked || view.kind !== 'edit') return;
    const group = view.group ?? name.trim();
    if (view.group === null) {
      const problem = groupNameProblem(group);
      if (problem) { failure = problem; return; }
      if (groups.includes(group)) { failure = 'A group with this name exists. Use Edit rooms.'; return; }
      if (chosen.length === 0) { failure = 'Choose at least one room for the new group.'; return; }
    }
    const writes = groupWrites(preferences, group, chosen, joined);
    const capacity = capacityProblem(preferences, writes);
    if (capacity) { failure = capacity; return; }
    if (await run(writes)) back();
  }

  async function remove(group: string) {
    if (blocked) return;
    const writes = removeGroupWrites(preferences, group);
    if (await run(writes)) confirming = null;
  }
</script>

<FeatureDialog title="Room groups" {onclose} closeDisabled={busy}>
  <div class="room-groups">
    {#if view.kind === 'list'}
      <p class="note">Groups show in the room list after Favourites. A favourite room shows only under Favourites. Groups are kept in this account profile and are not sent to the homeserver.</p>
      {#if groups.length === 0}
        <p class="note">No groups yet.</p>
      {:else}
        <ul class="room-group-list" aria-label="Groups">
          {#each groups as group (group)}
            {@const count = joinedCount(group)}
            <li>
              <span class="room-group-name"><bdi>{group}</bdi> <span class="note">({count} {count === 1 ? 'room' : 'rooms'})</span></span>
              {#if confirming === group}
                <span class="room-group-confirm" role="group" aria-label="Confirm removal">
                  <span>Remove group <bdi>{group}</bdi>? Its rooms move to Rooms. Favourites stay.</span>
                  <button type="button" class="danger" disabled={blocked} onclick={() => remove(group)}>{busy ? 'Removing…' : 'Remove'}</button>
                  <button type="button" class="secondary" disabled={busy} onclick={() => (confirming = null)}>Cancel</button>
                </span>
              {:else}
                <span class="actions">
                  <button type="button" class="secondary" disabled={blocked} onclick={() => edit(group)}>Edit rooms</button>
                  <button type="button" class="secondary" disabled={blocked} onclick={() => { confirming = group; failure = null; }}>Remove group</button>
                </span>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
      <button type="button" class="primary" disabled={blocked} onclick={() => edit(null)}>New group</button>
    {:else}
      <form onsubmit={save}>
        {#if view.group === null}
          <label for="room-group-name">Group name (up to {MAX_GROUP_NAME_BYTES} UTF-8 bytes)</label>
          <input id="room-group-name" bind:value={name} maxlength={MAX_GROUP_NAME_BYTES} disabled={busy} autocomplete="off" />
          <p class="note">A group exists while it has at least one room.</p>
        {:else}
          <p>Group: <bdi>{view.group}</bdi></p>
          <p class="note">A group with no rooms left is removed from the room list.</p>
        {/if}
        <fieldset class="room-group-rooms">
          <legend>Rooms in this group</legend>
          {#each rooms as room (room.roomId)}
            {@const saved = byRoom.get(room.roomId)}
            <label>
              <input type="checkbox" checked={chosen.includes(room.roomId)} disabled={busy}
                onchange={(event) => toggle(room.roomId, event.currentTarget.checked)} />
              <bdi>{room.displayName ?? room.roomId}</bdi>
              {#if saved?.favourite}<span class="note">(favourite: listed under Favourites)</span>{/if}
              {#if saved?.section && saved.section !== view.group}<span class="note">(now in <bdi>{saved.section}</bdi>)</span>{/if}
            </label>
          {:else}
            <p class="note">No rooms yet.</p>
          {/each}
        </fieldset>
        <div class="actions">
          <button type="button" class="secondary" disabled={busy} onclick={back}>Cancel</button>
          <button type="submit" class="primary" disabled={blocked}>{busy ? 'Saving…' : 'Save'}</button>
        </div>
      </form>
    {/if}
    {#if failure}<p role="alert" class="error">{failure}</p>{/if}
  </div>
</FeatureDialog>
