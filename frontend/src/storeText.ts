import type { StoreState } from '../../crates/contracts/generated/contracts';

/** Fixed text per store state; the core sends codes only. */
export const storeText: Record<StoreState, string> = {
  not_configured: 'Demo rooms with synthetic data: nothing is stored',
  protected: 'Local store protected',
  locked: 'Local store locked',
  locking: 'Locking: work in progress is finishing',
  unlocking: 'Unlocking: if the profile has a passphrase, enter it in the Persephone window',
  lock_failed: 'Local store did not close cleanly; restart Persephone',
  restart_required: 'Locked: restart Persephone to unlock',
  key_missing: 'Local store locked: its key is missing',
  key_unreadable: 'Local store locked: its key cannot be unlocked',
  secret_rejected: 'Local store locked: the passphrase was wrong too many times',
  key_rejected: 'Local store locked: the key does not match',
  profile_in_use: 'Local store locked: open in another window',
  store_unavailable: 'Local store unavailable',
  unsupported: 'Local store unavailable on this system',
  // Linux (ADR-0061): each reason says what to do next.
  no_session_bus:
    'Local store locked: there is no session bus, so Persephone cannot reach a keyring. ' +
    'Start Persephone from a desktop session, then choose Unlock',
  no_secret_service:
    'Local store locked: no keyring service is running. ' +
    'Install or start a keyring, such as GNOME Keyring or KWallet, then choose Unlock',
  no_default_keyring:
    'Local store locked: there is no default keyring. ' +
    'Create a keyring in Passwords and Keys and make it the default, then choose Unlock. ' +
    'Or choose Unlock and answer Yes to let Persephone ask the keyring to create one',
  no_display:
    'Local store locked: Persephone could not open its own window to ask. ' +
    'Start Persephone in a desktop session with a display, then choose Unlock'
};

/**
 * States the core can try to unlock from; the others need a restart or have
 * nothing to unlock. A missing Linux keyring part can be fixed while
 * Persephone runs, so Unlock tries again.
 */
export const unlockable: readonly StoreState[] = [
  'locked',
  'secret_rejected',
  'key_missing',
  'key_unreadable',
  'key_rejected',
  'profile_in_use',
  'store_unavailable',
  'no_session_bus',
  'no_secret_service',
  'no_default_keyring',
  'no_display'
];

/**
 * Closed profiles the core may reset after a forgotten passphrase or a lost
 * key. The core checks again and asks for confirmation in its own window. A
 * missing keyring part is not one: a new profile would need it too.
 */
export const resettable: readonly StoreState[] = [
  'locked',
  'secret_rejected',
  'key_missing',
  'key_unreadable',
  'key_rejected',
  'store_unavailable'
];
