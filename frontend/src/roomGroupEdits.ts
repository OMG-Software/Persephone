// Room groups are the `section` of each room's profile preference
// (ADR-0035). A group exists only while at least one room has its name, so
// creating, editing and removing a group are writes to room preferences.
// The core checks every write again; these checks give early, clear errors.

import type { RoomPreference } from '../../crates/contracts/generated/contracts';
import { byteLength } from './drafts.ts';

/** The core's limits: distinct groups, kept room entries, and name bytes. */
export const MAX_GROUPS = 16;
export const MAX_GROUP_ENTRIES = 100;
export const MAX_GROUP_NAME_BYTES = 64;

/** Headings the room list uses itself; a group with one of these names would look like them. */
const RESERVED = ['favourites', 'rooms'];

/** Exact label order, so the order does not depend on the device locale. */
const compare = (left: string, right: string) => (left < right ? -1 : left > right ? 1 : 0);

/** The distinct group names in the preferences, in exact label order. */
export function groupNames(preferences: RoomPreference[]): string[] {
  const names = new Set<string>();
  for (const entry of preferences) if (entry.section) names.add(entry.section);
  return [...names].sort(compare);
}

/** Why a new group name cannot be used, or null when it can. Trim the name first. */
export function groupNameProblem(name: string): string | null {
  if (name.length === 0) return 'Enter a group name.';
  if (byteLength(name) > MAX_GROUP_NAME_BYTES) return `The group name exceeds ${MAX_GROUP_NAME_BYTES} UTF-8 bytes.`;
  if (/\p{Cc}/u.test(name)) return 'The group name cannot contain control characters.';
  if (RESERVED.includes(name.toLowerCase())) return 'Favourites and Rooms are already headings in the room list. Use another name.';
  return null;
}

/**
 * The writes that make exactly `roomIds` the joined rooms of group `name`.
 * A room that joins the group keeps its favourite. A joined room that leaves
 * the group keeps its favourite and has no group. Entries for rooms outside
 * `joined` (rooms not in the room list) are not changed.
 */
export function groupWrites(
  preferences: RoomPreference[],
  name: string,
  roomIds: string[],
  joined: string[]
): RoomPreference[] {
  const byRoom = new Map(preferences.map((entry) => [entry.roomId, entry]));
  const chosen = new Set(roomIds);
  const leaving: RoomPreference[] = [];
  const joining: RoomPreference[] = [];
  for (const roomId of joined) {
    const saved = byRoom.get(roomId);
    const favourite = saved?.favourite ?? false;
    const inGroup = saved?.section === name;
    if (chosen.has(roomId) && !inGroup) joining.push({ roomId, favourite, section: name });
    else if (!chosen.has(roomId) && inGroup) leaving.push({ roomId, favourite, section: null });
  }
  // Rooms leave first: each leave can free a room entry before a join needs one.
  return [...leaving, ...joining];
}

/** The writes that remove group `name` from every entry, including rooms not in the room list. */
export function removeGroupWrites(preferences: RoomPreference[], name: string): RoomPreference[] {
  return preferences
    .filter((entry) => entry.section === name)
    .map((entry) => ({ roomId: entry.roomId, favourite: entry.favourite, section: null }));
}

/** The preferences after the writes, as the core keeps them: an entry with no favourite and no group is removed. */
export function afterWrites(preferences: RoomPreference[], writes: RoomPreference[]): RoomPreference[] {
  const byRoom = new Map(preferences.map((entry) => [entry.roomId, entry]));
  for (const write of writes) {
    if (write.favourite || write.section !== null) byRoom.set(write.roomId, write);
    else byRoom.delete(write.roomId);
  }
  return [...byRoom.values()];
}

/**
 * Why the writes would go over the core's limits, or null when they fit.
 * The core checks each write on its own, so every step in order must fit,
 * not only the result: moving rooms into a new group while their old group
 * still has rooms holds both groups for a while.
 */
export function capacityProblem(preferences: RoomPreference[], writes: RoomPreference[]): string | null {
  let state = preferences;
  for (const write of writes) {
    state = afterWrites(state, [write]);
    if (groupNames(state).length > MAX_GROUPS) {
      return `This profile can keep at most ${MAX_GROUPS} groups. Remove a group first.`;
    }
    if (state.length > MAX_GROUP_ENTRIES) {
      return `This profile can keep groups and favourites for at most ${MAX_GROUP_ENTRIES} rooms.`;
    }
  }
  return null;
}

/**
 * The writes that save group `name`, or why they cannot be saved. `isNew`:
 * the name was typed for a new group. `chosen`: the rooms checked; a room
 * that has left the room list (`joined`) since it was checked is dropped.
 */
export function planGroupSave(
  preferences: RoomPreference[],
  name: string,
  isNew: boolean,
  chosen: string[],
  joined: string[]
): { writes: RoomPreference[] } | { failure: string } {
  const picked = chosen.filter((roomId) => joined.includes(roomId));
  if (isNew) {
    const problem = groupNameProblem(name);
    if (problem) return { failure: problem };
    if (groupNames(preferences).includes(name)) return { failure: 'A group with this name exists. Use Edit rooms.' };
    if (picked.length === 0) return { failure: 'Choose at least one room in the room list for the new group.' };
  }
  const writes = groupWrites(preferences, name, picked, joined);
  const capacity = capacityProblem(preferences, writes);
  return capacity ? { failure: capacity } : { writes };
}

/**
 * Saves the writes one at a time and stops at the first failure. `onsaved`
 * runs after each saved write, so what is shown follows what the core keeps.
 * Returns how many were saved and whether one failed.
 */
export async function applyWrites(
  writes: RoomPreference[],
  save: (preference: RoomPreference) => Promise<void>,
  onsaved: (preference: RoomPreference) => void
): Promise<{ saved: number; failed: boolean }> {
  let saved = 0;
  for (const write of writes) {
    try {
      await save(write);
    } catch {
      return { saved, failed: true };
    }
    saved += 1;
    onsaved(write);
  }
  return { saved, failed: false };
}
