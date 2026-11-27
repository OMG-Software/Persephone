import type { AppMenuState } from './bridge';

// The "App menu" panel (ADR-0072): what it says and offers for each state the
// core reports. The core checks the AppImage, asks the user in its own window
// and writes the files; the page only asks for a change.

/** One button: the change it asks for, and its words. */
export type AppMenuAction = { on: boolean; label: string };

/** What the panel shows for a state; `null` hides the panel. */
export type AppMenuView = { note: string; actions: AppMenuAction[] };

const ADD: AppMenuAction = { on: true, label: 'Add to app menu…' };
const UPDATE: AppMenuAction = { on: true, label: 'Update menu entry…' };
const REMOVE: AppMenuAction = { on: false, label: 'Remove from app menu' };

export function appMenuView(state: AppMenuState): AppMenuView | null {
  switch (state) {
    case 'absent':
      return {
        note:
          'This copy of Persephone is an AppImage that no app menu lists. Some desktops, such as ' +
          'KDE Plasma on Wayland, then show a generic icon on its windows. Persephone can add a ' +
          'menu entry and its icon for your user. It asks you first, in its own window.',
        actions: [ADD]
      };
    case 'added':
      return {
        note:
          'The app menu lists this AppImage. Removing the entry also removes the icon that ' +
          'Persephone added.',
        actions: [REMOVE]
      };
    case 'outdated':
      return {
        note:
          'The menu entry that Persephone added starts another AppImage, or is from another ' +
          'version. Update it to start this one, or remove it.',
        actions: [UPDATE, REMOVE]
      };
    case 'provided':
      return {
        note:
          'A package or another program already gives Persephone an app menu entry, so ' +
          'Persephone does not add one.',
        actions: []
      };
    case 'unavailable':
      return null;
  }
}

/** Fixed words for a refused change; the reason itself is never shown. */
export function appMenuFailure(reason: unknown): string {
  return reason === 'limit_exceeded'
    ? 'A change to the app menu entry is already under way.'
    : 'The app menu entry could not be changed.';
}
