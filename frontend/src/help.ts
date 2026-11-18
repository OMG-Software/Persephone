// Fixed help text for the session buttons. Keep it true to the core's
// behaviour (ADR-0006, docs/architecture/sdk-lifecycle.md).

export const LOCK_HELP =
  "Closes Persephone's encrypted profile on this computer and clears the screen. " +
  'It does not sign you out of the server. To open the profile again you need your ' +
  'profile passphrase; after you have signed in, you must also restart Persephone. ' +
  'Lock is available only while the profile passphrase is on.';

export const RESET_HELP =
  "For a forgotten passphrase or an unusable key: deletes Persephone's profile on this " +
  'computer after Persephone asks you to confirm in its own window. The new profile has no ' +
  'passphrase until you turn one on. You then sign in again; encrypted history comes back ' +
  'only from key backup.';
