/**
 * Keyboard movement inside an open menu (WAI-ARIA menu button pattern).
 * Returns the index of the item to focus, or null when the key does not
 * move focus or no item can take it. Disabled items are skipped.
 */
export function menuTarget(key: string, current: number, enabled: readonly boolean[]): number | null {
  const count = enabled.length;
  if (!enabled.some(Boolean)) return null;
  /** Step from `start` in `step` direction, wrapping, to the next enabled item. */
  const seek = (start: number, step: 1 | -1) => {
    for (let moved = 0, at = start; moved < count; moved += 1) {
      at = (at + step + count) % count;
      if (enabled[at]) return at;
    }
    return null;
  };
  switch (key) {
    case 'ArrowDown':
      return seek(current < 0 || current >= count ? -1 : current, 1);
    case 'ArrowUp':
      return seek(current < 0 || current >= count ? count : current, -1);
    case 'Home':
      return seek(-1, 1);
    case 'End':
      return seek(count, -1);
    default:
      return null;
  }
}

/** One command of a menu, as `Menu.svelte` shows it. */
export type MenuCommand = {
  label: string;
  onselect: () => void;
  disabled?: boolean;
  separated?: boolean;
};

/** What the main menu's commands do. */
export type MainMenuActions = {
  explore: () => void;
  groups: () => void;
  search: () => void;
  drafts: () => void;
  ignored: () => void;
  health: () => void;
  settings: () => void;
  lock: () => void;
};

/**
 * The main menu. The account tools show only when signed in. Settings is
 * always there, so it stays reachable with synthetic data. Lock shows when
 * the profile can be locked, and is disabled while the session changes or
 * the sign-out question is open.
 */
export function mainMenuItems(
  state: { signedIn: boolean; canLock: boolean; switching: boolean; confirmingSignOut: boolean },
  act: MainMenuActions
): MenuCommand[] {
  const account: MenuCommand[] = state.signedIn
    ? [
        { label: 'Explore public rooms', onselect: act.explore },
        { label: 'Room groups', onselect: act.groups },
        { label: 'Search cached messages', onselect: act.search },
        { label: 'Saved drafts', onselect: act.drafts },
        { label: 'Ignored users', onselect: act.ignored },
        { label: 'Connection health', onselect: act.health }
      ]
    : [];
  const lock: MenuCommand[] = state.canLock
    ? [
        {
          label: state.switching ? 'Working…' : 'Lock Persephone',
          onselect: act.lock,
          disabled: state.switching || state.confirmingSignOut
        }
      ]
    : [];
  return [...account, { label: 'Settings', onselect: act.settings, separated: account.length > 0 }, ...lock];
}
