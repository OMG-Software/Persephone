// Text and checks for the public room directory. The core checks every
// request again; these give early, clear errors. Everything a directory
// sends is untrusted text: the page shows it escaped and isolated.

import type { PublicRoom, PublicRoomPage } from '../../crates/contracts/generated/contracts';

/** The core's limits on a server name, a directory filter, and listed rooms. */
export const MAX_SERVER_NAME_BYTES = 261;
export const MAX_DIRECTORY_FILTER_BYTES = 256;
/** Most rooms one listing keeps across its pages, as many as the core remembers to join. */
export const MAX_LISTED_ROOMS = 300;

const bytes = (text: string) => new TextEncoder().encode(text).length;

// A DNS name or IPv4 address, or a bracketed IPv6 address, with an optional
// port: the Matrix server-name grammar, without its finer checks.
const SERVER_NAME = /^(?:[A-Za-z0-9.-]+|\[[0-9A-Fa-f:.]+\])(?::[0-9]{1,5})?$/;

/** Why a typed server name cannot be browsed, or null when it can. An empty name browses the own homeserver. */
export function serverProblem(server: string): string | null {
  const name = server.trim();
  if (name === '') return null;
  if (name.includes('://') || name.includes('/')) return 'Enter only the server name, such as matrix.org.';
  if (bytes(name) > MAX_SERVER_NAME_BYTES || !SERVER_NAME.test(name)) {
    return 'That is not a server name. Enter one such as matrix.org.';
  }
  return null;
}

/** Why a typed filter cannot be sent, or null when it can. An empty filter lists every room. */
export function filterProblem(filter: string): string | null {
  const text = filter.trim();
  if (bytes(text) > MAX_DIRECTORY_FILTER_BYTES) return `The search text exceeds ${MAX_DIRECTORY_FILTER_BYTES} UTF-8 bytes.`;
  if (/\p{Cc}/u.test(text)) return 'The search text cannot contain control characters.';
  return null;
}

/** The server part of a Matrix user ID, such as `example.org` for `@al:example.org`. */
export function ownServer(userId: string | null): string | null {
  if (!userId) return null;
  const at = userId.indexOf(':');
  return at > 0 && at < userId.length - 1 ? userId.slice(at + 1) : null;
}

/** A listing: its pages so far, one room once, up to the limit. */
export type Listing = { rooms: PublicRoom[]; next: string | null; total: number | null };

/** Adds a page to a listing. A room listed twice keeps its first place; past the limit, no next page is offered. */
export function addPage(listing: Listing | null, page: PublicRoomPage): Listing {
  const rooms = [...(listing?.rooms ?? [])];
  const seen = new Set(rooms.map((room) => room.roomId));
  for (const room of page.rooms) {
    if (rooms.length >= MAX_LISTED_ROOMS) break;
    if (seen.has(room.roomId)) continue;
    seen.add(room.roomId);
    rooms.push(room);
  }
  return { rooms, next: rooms.length >= MAX_LISTED_ROOMS ? null : page.next, total: page.total };
}

/** What a room shows as its name: its name, else its address, else its ID. */
export function roomLabel(room: PublicRoom): string {
  return room.name ?? room.alias ?? room.roomId;
}

/** How many members a room has, in words. */
export function memberText(count: number): string {
  return count === 1 ? '1 member' : `${count.toLocaleString('en')} members`;
}

/** Why a listing failed, for the server asked. */
export function listFailure(code: string, otherServer: boolean): string {
  switch (code) {
    case 'rejected':
      return otherServer
        ? "That server's directory could not be read. Many servers do not share their directory with other servers."
        : 'Your homeserver refused to list its directory.';
    case 'invalid_argument':
      return 'Check the server name and the search text.';
    case 'limit_exceeded':
      return 'An earlier directory request is still running. Try again in a moment.';
    case 'locked':
      return 'The session is locked.';
    case 'not_found':
      return 'Sign in to browse public rooms.';
    default:
      return 'The homeserver could not be reached. Try again.';
  }
}

/** Why a join failed. */
export function joinFailure(code: string): string {
  switch (code) {
    case 'not_found':
      return 'This room is no longer in the list. Browse again, then join.';
    case 'rejected':
      return 'The homeserver refused. The room may be closed to you, or you may be banned from it.';
    case 'limit_exceeded':
      return 'Another room is being joined. Try again in a moment.';
    case 'locked':
      return 'The session is locked.';
    default:
      return 'The homeserver could not be reached. If you joined the room anyway, it appears in the room list.';
  }
}
