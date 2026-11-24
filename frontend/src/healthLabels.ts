import type { ConnectionHealth } from '../../crates/contracts/generated/contracts';

const accounts = {
  signed_out: 'Signed out', signing_in: 'Signing in', signed_in: 'Signed in', sign_in_failed: 'Sign-in failed'
};
const stores = {
  not_configured: 'Not configured', protected: 'Unlocked and protected', locked: 'Locked', locking: 'Locking',
  unlocking: 'Unlocking', lock_failed: 'Lock failed', restart_required: 'Restart required',
  key_missing: 'Store key is missing', key_unreadable: 'Store key cannot be read',
  secret_rejected: 'Passphrase rejected', key_rejected: 'Store key rejected',
  profile_in_use: 'Profile is open in another window', store_unavailable: 'Store unavailable',
  unsupported: 'Store protection is unavailable', no_session_bus: 'No session bus',
  no_secret_service: 'No keyring service', no_default_keyring: 'No default keyring',
  no_display: 'No display for the core windows', keyring_busy: 'Keyring busy in another window'
};
const modes = { unknown: 'Not determined', sliding: 'Sliding Sync', classic: 'Classic sync' };

/** Use fixed user-facing labels. Never display an internal error code. */
export function healthLabels(health: ConnectionHealth) {
  return {
    account: accounts[health.accountState] ?? 'Account state unavailable',
    store: stores[health.storeState] ?? 'Store state unavailable',
    mode: modes[health.syncMode] ?? 'Not determined',
    error: health.lastError === null ? 'None recorded' : 'The last connection attempt failed.'
  };
}
