// Key handling for the account settings. The settings themselves belong to
// the account: the core keeps them in its encrypted profile, and the web view
// keeps none (no web storage).

import type { AccountSettings } from './bridge';

/** The settings an account has until its own have loaded. */
export const DEFAULT_SETTINGS: AccountSettings = {
  enterSends: true,
  chatTextPercent: 100,
  interfaceTextPercent: 100,
  desktopNotifications: false,
  sendTyping: false,
  sendReadReceipts: false
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
 * Keeps the signed-in account's settings in step with the core. A change is
 * a patch of the settings it touches: it shows at once, and is saved only
 * once the account's stored settings are known, laid over them, so a change
 * made while they load never writes the defaults over the others. A change
 * that cannot be saved, or made while they cannot be read, is put back with
 * a failure. Anything that finishes after `clear` is discarded, so one
 * account never sees another's settings.
 */
export class SettingsKeeper {
  readonly #core: SettingsCore;
  readonly #show: (settings: AccountSettings, failure: string | null) => void;
  // Advances on `clear`: work from before it changes nothing.
  #epoch = 0;
  // Counts loads: only the newest one's result is shown.
  #loads = 0;
  #current: AccountSettings = DEFAULT_SETTINGS;
  // Whether the stored settings are known: the newest load's outcome.
  #known: Promise<boolean> = Promise.resolve(false);
  // The settings the core last confirmed: loaded or saved. A failed save
  // puts its fields back to these.
  #stored: AccountSettings | null = null;
  // Changes not yet saved, shown over the stored settings when they load.
  #unsaved: Partial<AccountSettings> = {};
  // Which change set each unsaved field last: a change undoes or forgets
  // only the fields it still owns, even when another set the same value.
  #owners: Partial<Record<keyof AccountSettings, number>> = {};
  #changes = 0;
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
    this.#epoch += 1;
    this.#known = Promise.resolve(false);
    this.#stored = null;
    this.#unsaved = {};
    this.#owners = {};
    this.#set(DEFAULT_SETTINGS, null);
  }

  /** Asks the core for this account's settings; the defaults stay on failure. */
  load(): Promise<void> {
    const epoch = this.#epoch;
    const load = ++this.#loads;
    const known = this.#core.load().then(
      (stored) => {
        if (epoch !== this.#epoch) return false;
        if (load === this.#loads) {
          this.#stored = stored;
          this.#set({ ...stored, ...this.#unsaved }, null);
        }
        return true;
      },
      // A change reports its own failure.
      () => false
    );
    this.#known = known;
    return known.then(() => {});
  }

  async change(patch: Partial<AccountSettings>) {
    const epoch = this.#epoch;
    const keys = Object.keys(patch) as (keyof AccountSettings)[];
    const change = ++this.#changes;
    // What a failure puts back while the stored settings are not known.
    const before: Partial<AccountSettings> = Object.fromEntries(
      keys.map((key) => [key, this.#current[key]])
    );
    this.#unsaved = { ...this.#unsaved, ...patch };
    for (const key of keys) this.#owners[key] = change;
    this.#set({ ...this.#current, ...patch }, null);
    // The stored settings come first: saving before them would write the
    // defaults over every setting this change does not touch. Read them now
    // if no load has.
    let known = await this.#known;
    if (!known && epoch === this.#epoch) {
      await this.load();
      known = await this.#known;
    }
    if (epoch !== this.#epoch) return;
    if (!known) {
      this.#undo(change, keys, before);
      return;
    }
    // Saves go one at a time, in the order they were made: two calls in
    // flight could reach the core in either order and keep the older one.
    // A failed save is undone before the next one takes its snapshot, so a
    // later save never sends a refused value again, and it does not stop the
    // next. A save queued before `clear` is skipped: it would send the reset
    // settings as the next account's.
    const save = this.#saving.then(async () => {
      if (epoch !== this.#epoch) return;
      const sent = { ...this.#current };
      try {
        await this.#core.save(sent);
      } catch {
        if (epoch === this.#epoch) this.#undo(change, keys, before);
        return;
      }
      if (epoch !== this.#epoch) return;
      this.#stored = sent;
      this.#forget(change, keys);
    });
    this.#saving = save.catch(() => {});
    await save;
  }

  /** The fields of `keys` that `change` still owns: no newer change set them. */
  #owned(change: number, keys: (keyof AccountSettings)[]): (keyof AccountSettings)[] {
    return keys.filter((key) => this.#owners[key] === change);
  }

  /** Drops a saved change's fields from the unsaved ones, where it still owns them. */
  #forget(change: number, keys: (keyof AccountSettings)[]) {
    for (const key of this.#owned(change, keys)) {
      delete this.#unsaved[key];
      delete this.#owners[key];
    }
  }

  /**
   * Puts back the fields of a change that was not saved, with a failure: to
   * what the core holds now (an earlier save may have confirmed a value
   * since the change began), or to what was shown before it while the
   * stored settings are not known. A field a newer change set keeps the
   * newer value.
   */
  #undo(change: number, keys: (keyof AccountSettings)[], before: Partial<AccountSettings>) {
    const stored = this.#stored;
    const restored = { ...this.#current };
    for (const key of this.#owned(change, keys)) {
      Object.assign(restored, { [key]: stored ? stored[key] : before[key] });
    }
    this.#forget(change, keys);
    this.#set(restored, SAVE_FAILED);
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
