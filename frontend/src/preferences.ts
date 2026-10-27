// Key handling for the account settings. The settings themselves belong to
// the account: the core keeps them in its encrypted profile, and the web view
// keeps none (no web storage).

import type { AccountSettings } from './bridge';

/** The settings an account has until its own have loaded. */
export const DEFAULT_SETTINGS: AccountSettings = { enterSends: true };

export const SAVE_FAILED = 'The setting could not be saved to this account.';

/**
 * Keeps the signed-in account's settings in step with the core. A change
 * shows at once and is put back if saving fails. Only the newest request
 * counts: a load that finishes after a change, or anything that finishes
 * after `clear`, is discarded, so one account never sees another's settings.
 */
export class SettingsKeeper {
  #request = 0;
  #current: AccountSettings = DEFAULT_SETTINGS;

  constructor(
    private readonly core: {
      load: () => Promise<AccountSettings>;
      save: (settings: AccountSettings) => Promise<void>;
    },
    private readonly show: (settings: AccountSettings, failure: string | null) => void
  ) {}

  /** No account is signed in: back to the defaults. */
  clear() {
    this.#request += 1;
    this.#set(DEFAULT_SETTINGS, null);
  }

  /** Asks the core for this account's settings; the defaults stay on failure. */
  async load() {
    const request = ++this.#request;
    try {
      const loaded = await this.core.load();
      if (request === this.#request) this.#set(loaded, null);
    } catch {
      // A change reports its own failure.
    }
  }

  async change(next: AccountSettings) {
    const request = ++this.#request;
    const previous = this.#current;
    this.#set(next, null);
    try {
      await this.core.save(next);
    } catch {
      if (request === this.#request) this.#set(previous, SAVE_FAILED);
    }
  }

  #set(settings: AccountSettings, failure: string | null) {
    this.#current = settings;
    this.show(settings, failure);
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
