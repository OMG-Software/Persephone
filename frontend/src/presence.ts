// Presence as the page shows and chooses it (ADR-0057). The core decides
// when presence is sent; the page only names the account's choice.

import type { PresenceStatus } from '../../crates/contracts/generated/contracts';

/** The statuses the account can share, in menu order. */
export const PRESENCE_CHOICES: readonly { value: PresenceStatus; label: string }[] = [
  { value: 'online', label: 'Online' },
  { value: 'unavailable', label: 'Away' },
  { value: 'offline', label: 'Appear offline' }
];

/** `value` as a status, or online for anything else. */
export function presenceOf(value: unknown): PresenceStatus {
  return PRESENCE_CHOICES.some((choice) => choice.value === value)
    ? (value as PresenceStatus)
    : 'online';
}

/** The words for a contact's presence, or null when it is not known. */
export function presenceLabel(value: unknown): string | null {
  switch (value) {
    case 'online':
      return 'Online';
    case 'unavailable':
      return 'Away';
    case 'offline':
      return 'Offline';
    default:
      return null;
  }
}
