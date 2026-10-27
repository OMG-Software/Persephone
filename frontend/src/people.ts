// Display helpers for Matrix user IDs. Pure functions; the results are
// rendered as escaped text only.

/** The localpart of `@name:server`, or the whole text if it has no such form. */
export function localpart(userId: string): string {
  const match = /^@([^:]+):/.exec(userId);
  return match ? match[1] : userId;
}

/**
 * One or two letters for an avatar: the first letter of the localpart, and
 * the first letter after a `.`, `_` or `-` if there is one ("alex.moreno" is
 * "AM"). Characters that are not letters or digits are skipped, so control
 * and formatting characters never reach the avatar.
 */
export function initials(userId: string): string {
  const parts = localpart(userId)
    .split(/[._-]+/)
    .map((part) => Array.from(part).find((char) => /[\p{L}\p{N}]/u.test(char)))
    .filter((char): char is string => char !== undefined);
  const letters = parts.length > 1 ? parts[0] + parts[parts.length - 1] : (parts[0] ?? '?');
  return letters.toLocaleUpperCase();
}

/** A stable avatar colour class, c0 to c5, for a user ID. */
export function avatarColour(userId: string): string {
  let hash = 0;
  for (const char of userId) {
    hash = (hash * 31 + (char.codePointAt(0) ?? 0)) >>> 0;
  }
  return 'c' + (hash % 6);
}

/**
 * Isolates untrusted text inside a plain string, such as a placeholder or the
 * window title, where no <bdi> element can do it.
 */
/** Most people a new room may invite: the core's limit. */
export const MAX_INVITEES = 10;
const MAX_USER_ID = 255;

/** A first check that text is an exact Matrix user ID; the core decides. */
export function looksLikeUserId(text: string): boolean {
  return text.length <= MAX_USER_ID && /^@[^\s:]+:\S+$/.test(text);
}

/**
 * The people to invite to a new room, with the one still typed in the person
 * field. Nothing typed, or someone already listed, changes nothing. Text that
 * is not a Matrix ID, or a person beyond the limit, is a problem to show: the
 * room is not created without them.
 */
export function inviteesWith(
  invite: string[],
  typed: string
): { invite: string[] } | { problem: 'not_a_matrix_id' | 'too_many' } {
  const person = typed.trim();
  if (!person || invite.includes(person)) return { invite };
  if (!looksLikeUserId(person)) return { problem: 'not_a_matrix_id' };
  if (invite.length >= MAX_INVITEES) return { problem: 'too_many' };
  return { invite: [...invite, person] };
}

export function isolate(text: string): string {
  return '⁨' + text + '⁩';
}
