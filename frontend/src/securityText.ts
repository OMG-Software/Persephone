// Fixed text per security error code; the core sends codes only. Secrets
// never reach this page: the core asks for them, and shows the recovery key,
// in its own windows.
import type { SecurityError } from '../../crates/contracts/generated/contracts';

export const securityErrorText: Record<SecurityError, string> = {
  cancelled: 'Cancelled.',
  unsupported: 'This system has no Persephone sign-in window yet.',
  invalid_password: 'The homeserver did not accept that password.',
  identity_exists:
    'This account already has encryption set up. Verify this device with your recovery key instead.',
  wrong_recovery_key: 'That recovery key did not work.',
  backup_exists_on_server: 'The homeserver already has a key backup that Persephone cannot use yet.',
  recovery_key_not_shown:
    'Recovery was set up, but its key could not be shown. Do not rely on it until it is set up again.',
  unreachable: 'The homeserver could not be reached.',
  contact_has_no_identity:
    'This person has not set up encryption, so there is nothing to verify yet.',
  cannot_sign_contacts:
    'This device cannot vouch for other people yet. Verify it with your recovery key first.',
  not_direct_room:
    'Verification needs a direct-message room that this person is still in. Persephone does not create one or invite them.',
  cannot_sign_devices:
    'This device cannot vouch for your other sessions yet. Verify it with your recovery key first.',
  device_not_found: 'The homeserver no longer lists that session. Refresh the list.',
  recovery_key_not_saved:
    'The new recovery key was shown, but saving it to the file failed. Write it down now: it cannot be shown again.',
  failed: 'That did not work.'
};
