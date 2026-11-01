import type { IgnoredUsers } from './bridge';

type IgnoredUsersResult =
  | { users: IgnoredUsers; failure: null }
  | { users: null; failure: string };

/** Publish a list or load error only while its dialog is active. Success clears the error. */
export async function loadIgnoredUsers(
  read: () => Promise<IgnoredUsers>,
  active: () => boolean,
  publish: (result: IgnoredUsersResult) => void
): Promise<void> {
  try {
    const users = await read();
    if (active()) publish({ users, failure: null });
  } catch {
    if (active()) publish({ users: null, failure: 'The ignored-user list could not be loaded.' });
  }
}
