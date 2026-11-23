// Presence as the page shows and chooses it (ADR-0057). The core decides
// when presence is sent; the page only names the account's choice.

import type { PresenceStatus } from '../../crates/contracts/generated/contracts';

/**
 * Shown next to the presence controls while the homeserver refuses the
 * account's presence (issue 86). Fixed text: the server's own error text is
 * untrusted and never shown.
 */
export const PRESENCE_REFUSED = 'Your homeserver does not accept your presence, so it is not shared.';

/**
 * What the account footer shows under its presence menu: a failed save
 * first, as it is the newest thing the user did, then a refusal.
 */
export function presenceNotice(failure: string | null, refused: boolean): string | null {
  return failure ?? (refused ? PRESENCE_REFUSED : null);
}

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

/** `value` as a status, or null when it is not one of the three. */
export function knownPresence(value: unknown): PresenceStatus | null {
  return presenceLabel(value) === null ? null : (value as PresenceStatus);
}

/** What the account footer shows of this account's own presence. */
export type OwnPresence = { status: PresenceStatus | 'hidden'; label: string };

/** The account's own presence as its settings choose it. */
export function ownPresence(settings: { sharePresence: boolean; presence: unknown }): OwnPresence {
  if (!settings.sharePresence) return { status: 'hidden', label: 'Not shared' };
  const status = presenceOf(settings.presence);
  return { status, label: presenceLabel(status) ?? 'Online' };
}

/** A change to the account settings from the account footer's menu. */
export type PresencePatch = { sharePresence: boolean; presence?: PresenceStatus };

/** One command of the presence menu (a `MenuItem` of `Menu.svelte`). */
export type PresenceItem = {
  label: string;
  onselect: () => void;
  checked: boolean;
  mark?: string;
  separated?: boolean;
};

/**
 * The account footer's presence menu. Choosing a status also turns sharing
 * on: choosing one is asking for it to be shared. "Stop sharing" keeps the
 * chosen status for when sharing is turned on again.
 */
export function presenceMenu(
  settings: { sharePresence: boolean; presence: unknown },
  onchange: (patch: PresencePatch) => void
): PresenceItem[] {
  const shown = ownPresence(settings);
  const items: PresenceItem[] = PRESENCE_CHOICES.map((choice) => ({
    label: choice.label,
    mark: 'presence-mark ' + choice.value,
    checked: shown.status === choice.value,
    onselect: () => onchange({ sharePresence: true, presence: choice.value })
  }));
  items.push({
    label: 'Do not share presence',
    checked: shown.status === 'hidden',
    separated: true,
    onselect: () => onchange({ sharePresence: false })
  });
  return items;
}
