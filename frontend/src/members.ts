import type {
  ContactStatus,
  ContactTrust,
  RoomMember,
  RoomReader,
  RoomSummary
} from '../../crates/contracts/generated/contracts';
import { localpart } from './people.ts';

/** One person in the room's member panel. All text is untrusted. */
export type MemberRow = {
  userId: string;
  name: string | null;
  trust: ContactTrust;
  own: boolean;
  // Invited but not joined yet. Invited members can read new messages too.
  invited: boolean;
};

/** What the panel shows next to a member's name. */
export type MemberMark = 'own' | 'verified' | 'verify' | 'changed' | 'no_identity' | 'unknown';

/** Exact, locale-independent order, as for room sections. */
const compare = (left: string, right: string) => (left < right ? -1 : left > right ? 1 : 0);

/**
 * Join the readers (who is in the room, and how far they are trusted) with
 * the member list (display names). The direct-message contact's status comes
 * from `contact_status`, which asks the homeserver when the store has no keys,
 * so it replaces the reader's trust. This account comes first, then by name.
 */
export function memberRows(
  readers: readonly RoomReader[],
  members: readonly RoomMember[] | null,
  contact: ContactStatus | null
): MemberRow[] {
  const byId = new Map((members ?? []).map((member) => [member.userId, member]));
  const rows = readers.map((reader) => ({
    userId: reader.userId,
    name: byId.get(reader.userId)?.displayName ?? null,
    invited: byId.get(reader.userId)?.membership === 'invite',
    trust: contact !== null && contact.userId === reader.userId ? contact.trust : reader.trust,
    own: reader.own
  }));
  // Sorted by what the list shows first: the display name, or else the
  // user ID without its "@", so names and bare IDs sort together.
  const key = (row: MemberRow) => (row.name ?? localpart(row.userId)).toLowerCase();
  return rows.sort(
    (a, b) => Number(b.own) - Number(a.own) || compare(key(a), key(b)) || compare(a.userId, b.userId)
  );
}

/** The mark for one member. A trust level the page does not know never reads as verified. */
export function memberMark(row: MemberRow): MemberMark {
  if (row.own) return 'own';
  switch (row.trust) {
    case 'verified':
      return 'verified';
    case 'unverified':
      return 'verify';
    case 'changed':
    case 'verified_changed':
      return 'changed';
    case 'no_identity':
      return 'no_identity';
    default:
      return 'unknown';
  }
}

/**
 * The joined direct-message room for this person, if the room list has one.
 * The core verifies people only in such a room, and checks it again itself.
 */
export function directRoomOf(rooms: readonly RoomSummary[], userId: string): string | null {
  return rooms.find((room) => room.contact === userId)?.roomId ?? null;
}
