import type { RoomPreference, RoomSummary } from '../../crates/contracts/generated/contracts';

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
    ...[...sections].sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0)
      .map(([name, entries]) => ({ name, rooms: entries })),
    ...(other.length ? [{ name: 'Rooms', rooms: other }] : [])
  ];
}
