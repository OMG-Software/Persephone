// Fixed text for how a change to the account ended, and the page's first
// checks of an email address or phone number before it is sent. The core
// checks them again and decides; passwords and codes never reach this page.
import type { AccountChange, NotificationLevel } from './bridge';

export const accountChangeText: Record<AccountChange, string> = {
  done: 'Done.',
  token_sent: 'The homeserver sent a message to that address.',
  cancelled: 'Cancelled.',
  unsupported: 'This system has no Persephone sign-in window yet, so this cannot be done here.',
  invalid_password: 'The homeserver did not accept that password.',
  weak_password: 'The homeserver thinks the new password is too weak. Choose a longer one.',
  not_confirmed:
    'The address is not confirmed yet. Open the link in the email, or check the code, then try again.',
  in_use: 'Another account already uses that address.',
  denied: 'The homeserver does not allow that address.',
  medium_not_supported:
    'This homeserver cannot add this kind of address for Persephone. Some homeservers need an identity server for it, which Persephone does not use.',
  changes_disabled: 'This homeserver does not let this account change that.',
  unsupported_auth:
    'The homeserver asks for a step other than your password. Make this change in another client.',
  nothing_pending: 'Nothing is waiting to be added. Ask for a new link or code.',
  rate_limited: 'Too many attempts. Wait a little, then try again.',
  unreachable: 'The homeserver could not be reached.',
  failed: 'That did not work.'
};

/** Text for a code the core sent, or the generic text for anything else. */
export function changeText(code: unknown): string {
  return accountChangeText[code as AccountChange] ?? accountChangeText.failed;
}

/** Text for a call the host refused (`locked`, `limit_exceeded`, …). */
export function refusalText(reason: unknown): string {
  switch (String(reason)) {
    case 'invalid_argument':
      return 'That address is not in a form Persephone can send.';
    case 'limit_exceeded':
      return 'Another change to the account is still running. Finish it first.';
    case 'locked':
      return 'The session is locked.';
    case 'not_found':
      return 'No account is signed in.';
    case 'rejected':
      return 'The homeserver refused to list the addresses.';
    default:
      return 'The homeserver could not be reached.';
  }
}

/** The core's limits, mirrored here as a first stop only. */
export const MAX_EMAIL_LENGTH = 254;
export const MAX_PHONE_DIGITS = 32;

/** Whether `email` looks like an address the core will accept. */
export function plausibleEmail(email: string): boolean {
  const at = email.indexOf('@');
  const domain = email.slice(at + 1);
  return (
    email.length <= MAX_EMAIL_LENGTH &&
    at > 0 &&
    at === email.lastIndexOf('@') &&
    domain.includes('.') &&
    !domain.startsWith('.') &&
    !domain.endsWith('.') &&
    /^[\x21-\x7e]+$/.test(email)
  );
}

/**
 * A phone number as the core takes it: the country as two capital letters
 * and the number's digits as dialled there. Spaces, dashes, dots and
 * brackets are dropped; anything else makes it `null`.
 */
export function phoneNumber(country: string, number: string): { country: string; digits: string } | null {
  const code = country.trim().toUpperCase();
  const digits = number.replace(/[\s\-.()]/g, '');
  if (!/^[A-Z]{2}$/.test(code) || !/^\d{4,}$/.test(digits) || digits.length > MAX_PHONE_DIGITS) {
    return null;
  }
  return { country: code, digits };
}

/** How each notification level is described; `custom` is read only. */
export const notificationLevelText: Record<NotificationLevel, string> = {
  all: 'All messages',
  mentions: 'Mentions and keywords only',
  none: 'Nothing',
  custom: 'Set differently for different kinds of room by another app'
};
