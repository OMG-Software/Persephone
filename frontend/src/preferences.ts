// Key handling for the account settings. The settings themselves belong to
// the account: the core keeps them in its encrypted profile, and the web view
// keeps none (no web storage).

import type { AccountSettings } from './bridge';

/** The settings an account has until its own have loaded. */
export const DEFAULT_SETTINGS: AccountSettings = {
  enterSends: true,
  chatTextPercent: 100,
  interfaceTextPercent: 100,
  desktopNotifications: false
};

/**
 * The text sizes offered, in percent of the normal size. The core refuses
 * any size outside 80 to 200 for messages and 80 to 150 for the interface
 * (the whole interface grows with it, so it stops sooner).
 */
export const CHAT_TEXT_SIZES: readonly number[] = [80, 90, 100, 110, 125, 150, 175, 200];
export const INTERFACE_TEXT_SIZES: readonly number[] = [80, 90, 100, 110, 125, 150];

function within(value: number, sizes: readonly number[]): number {
  const low = sizes[0];
  const high = sizes[sizes.length - 1];
  return Number.isFinite(value) ? Math.min(high, Math.max(low, value)) : 100;
}

/**
 * The shell's scale factors for these settings: `interface` zooms the whole
 * interface, and `chat` sizes message text on its own. Message text is
 * divided by the interface zoom, so the two settings do not multiply. A size
 * out of bounds is held to the nearest bound.
 */
export function textScale(settings: AccountSettings): { interface: number; chat: number } {
  const ui = within(settings.interfaceTextPercent, INTERFACE_TEXT_SIZES) / 100;
  const chat = within(settings.chatTextPercent, CHAT_TEXT_SIZES) / 100;
  return { interface: ui, chat: chat / ui };
}

export const SAVE_FAILED = 'The setting could not be saved to this account.';

/** How the keeper reads and writes the settings: the bridge, or a test's fake. */
export type SettingsCore = {
  load: () => Promise<AccountSettings>;
  save: (settings: AccountSettings) => Promise<void>;
};

/**
 * Keeps the signed-in account's settings in step with the core. A change
 * shows at once and is put back if saving fails. Only the newest request
 * counts: a load that finishes after a change, or anything that finishes
 * after `clear`, is discarded, so one account never sees another's settings.
 */
export class SettingsKeeper {
  readonly #core: SettingsCore;
  readonly #show: (settings: AccountSettings, failure: string | null) => void;
  #request = 0;
  #current: AccountSettings = DEFAULT_SETTINGS;
  // The last save asked for; the next one starts after it settles.
  #saving: Promise<void> = Promise.resolve();

  constructor(
    core: SettingsCore,
    show: (settings: AccountSettings, failure: string | null) => void
  ) {
    this.#core = core;
    this.#show = show;
  }

  /** No account is signed in: back to the defaults. */
  clear() {
    this.#request += 1;
    this.#set(DEFAULT_SETTINGS, null);
  }

  /** Asks the core for this account's settings; the defaults stay on failure. */
  async load() {
    const request = ++this.#request;
    try {
      const loaded = await this.#core.load();
      if (request === this.#request) this.#set(loaded, null);
    } catch {
      // A change reports its own failure.
    }
  }

  async change(next: AccountSettings) {
    const request = ++this.#request;
    const previous = this.#current;
    this.#set(next, null);
    // Saves go one at a time, in the order they were made: two calls in
    // flight could reach the core in either order and keep the older one.
    // A failed save does not stop the next.
    const save = this.#saving.then(() => this.#core.save(next));
    this.#saving = save.catch(() => {});
    try {
      await save;
    } catch {
      if (request === this.#request) this.#set(previous, SAVE_FAILED);
    }
  }

  #set(settings: AccountSettings, failure: string | null) {
    this.#current = settings;
    this.#show(settings, failure);
  }
}

/**
 * What a key press in the composer does. With Enter to send, Enter sends and
 * Shift+Enter starts a new line; without it, Enter starts a new line and
 * Ctrl+Enter (⌘Enter on macOS) sends. Nothing happens while an input method
 * is composing text.
 */
export function composerKey(
  event: Pick<KeyboardEvent, 'key' | 'shiftKey' | 'ctrlKey' | 'metaKey' | 'isComposing'>,
  enterSends: boolean
): 'send' | 'none' {
  if (event.key !== 'Enter' || event.isComposing) return 'none';
  if (enterSends) return event.shiftKey ? 'none' : 'send';
  return event.ctrlKey || event.metaKey ? 'send' : 'none';
}
