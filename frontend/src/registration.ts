// The username rule for a new account, as the core checks it: a Matrix
// localpart of lowercase letters, digits and `._=-/+`. A first stop only;
// the core and the homeserver decide.

/** Longest username the core accepts, in bytes (all allowed characters are one byte). */
export const MAX_USERNAME = 255;

/** What the user typed, as a username: trimmed, lowercased, without a leading `@`. */
export function normalizeUsername(text: string): string {
  return text.trim().replace(/^@/, '').toLowerCase();
}

export type UsernameProblem = 'server' | 'characters' | 'too_long';

/**
 * Why a normalized username cannot be sent, or `null` when it can. An empty
 * one is not a problem to show; the button stays disabled.
 */
export function usernameProblem(username: string): UsernameProblem | null {
  if (username.includes(':')) return 'server';
  if (username.length > MAX_USERNAME) return 'too_long';
  if (username !== '' && !/^[a-z0-9._=\-/+]+$/.test(username)) return 'characters';
  return null;
}

export const usernameProblemText: Record<UsernameProblem, string> = {
  server: 'Type only the name, without ":" and a server. The account is made on the server above.',
  characters: 'Use only letters, digits and . _ = - / +',
  too_long: 'That name is too long.'
};
