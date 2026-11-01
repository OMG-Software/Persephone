import type { RoomPreference, RoomSummary } from '../../crates/contracts/generated/contracts';

/** Use exact label order so section sorting does not depend on the device locale. */
function compareSections([left]: [string, RoomSummary[]], [right]: [string, RoomSummary[]]): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

/** Preserve room order within groups. A favourite appears only in Favourites.
 * Unassigned rooms use the default group; named sections sort by their exact labels.
 */
export function groupRooms(rooms: RoomSummary[], preferences: RoomPreference[]) {
  const byRoom = new Map(preferences.map((entry) => [entry.roomId, entry]));
  const favourites: RoomSummary[] = [];
  const other: RoomSummary[] = [];
  const sections = new Map<string, RoomSummary[]>();
  for (const room of rooms) {
    const preference = byRoom.get(room.roomId);
    if (preference?.favourite) favourites.push(room);
    else if (preference?.section) {
      const group = sections.get(preference.section) ?? [];
      group.push(room);
      sections.set(preference.section, group);
    } else other.push(room);
  }
  return [
    ...(favourites.length ? [{ name: 'Favourites', rooms: favourites }] : []),
    ...[...sections].sort(compareSections)
      .map(([name, entries]) => ({ name, rooms: entries })),
    ...(other.length ? [{ name: 'Rooms', rooms: other }] : [])
  ];
}
