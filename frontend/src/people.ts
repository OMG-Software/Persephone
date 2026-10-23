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
export function isolate(text: string): string {
  return '⁨' + text + '⁩';
}
