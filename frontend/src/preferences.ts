// Key handling for the account settings. The settings themselves belong to
// the account: the core keeps them in its encrypted profile, and the web view
// keeps none (no web storage).

import type { AccountSettings } from './bridge';
import type { TimelineEvents } from '../../crates/contracts/generated/contracts';
import { DEFAULT_THEME } from './themes.ts';

/** The settings an account has until its own have loaded. */
export const DEFAULT_SETTINGS: AccountSettings = {
  enterSends: true,
  chatTextPercent: 100,
  interfaceTextPercent: 100,
  desktopNotifications: false,
  sendTyping: false,
  sendReadReceipts: false,
  idleLockMinutes: 0,
  notificationNames: true,
  loadRoomPictures: false,
  // Hidden from screenshots and screen sharing until the account turns it off.
  captureProtection: true,
  // Every kind of room change and other event shows until the account turns
  // one off.
  timelineEvents: {
    joins: true,
    leaves: true,
    invitations: true,
    removals: true,
    joinRequests: true,
    displayNames: true,
    profilePictures: true,
    roomName: true,
    topic: true,
    roomPicture: true,
    encryption: true,
    joinRule: true,
    historyVisibility: true,
    permissions: true,
    addresses: true,
    pinnedMessages: true,
    guestAccess: true,
    serverAccess: true,
    creation: true,
    upgrades: true,
    verification: true,
    calls: true,
    callSignalling: true,
    polls: true,
    stickers: true,
    locations: true,
    spaces: true,
    widgets: true,
    otherEvents: true,
    deletedMessages: true
  },
  theme: DEFAULT_THEME,
  // Presence is shared, as online, until the account changes it (ADR-0057).
  sharePresence: true,
  presence: 'online'
};

/** The idle-lock choices offered, in minutes; 0 never locks (ADR-0025). */
export const IDLE_LOCK_CHOICES: readonly number[] = [0, 5, 15, 30, 60];

/** How an idle-lock choice reads. */
export function idleLockText(minutes: number): string {
  if (minutes === 0) return 'Never';
  if (minutes === 60) return 'After 1 hour';
  return `After ${minutes} minutes`;
}

/**
 * Whether a key press is the lock-now shortcut: Ctrl+Shift+L, or
 * ⌘⇧L on macOS.
 */
export function isLockShortcut(
  event: Pick<KeyboardEvent, 'key' | 'shiftKey' | 'ctrlKey' | 'metaKey' | 'altKey'>,
  mac: boolean
): boolean {
  return (
    (mac ? event.metaKey : event.ctrlKey) &&
    event.shiftKey &&
    !event.altKey &&
    event.key.toLowerCase() === 'l'
  );
}

/**
 * Whether Lock is offered: the profile is open (or closing) and has a
 * passphrase to open it again (ADR-0050). Unlock is not part of this: a closed
 * profile can always be opened again.
 */
export function lockOffered(store: string | undefined, profilePassphrase: boolean | null): boolean {
  return profilePassphrase === true && (store === 'protected' || store === 'locking');
}

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
 * A change to the settings: some top-level settings, and some of the
 * timeline's switches. A switch left out keeps its value.
 */
export type SettingsPatch = Partial<Omit<AccountSettings, 'timelineEvents'>> & {
  timelineEvents?: Partial<TimelineEvents>;
};

/**
 * One setting the keeper tracks on its own: a top-level setting, or one of
 * the timeline's switches as `timelineEvents.<switch>`. Each switch is its
 * own field, so a switch changed while the settings load never writes the
 * other switches' defaults over their stored values.
 */
type Field = string;

const NESTED = 'timelineEvents';

/** The fields a patch sets, with their values. */
function fieldsOf(patch: SettingsPatch): Map<Field, unknown> {
  const fields = new Map<Field, unknown>();
  for (const [key, value] of Object.entries(patch)) {
    if (key === NESTED && value !== null && typeof value === 'object') {
      for (const [name, on] of Object.entries(value)) fields.set(`${NESTED}.${name}`, on);
    } else {
      fields.set(key, value);
    }
  }
  return fields;
}

/** The value of one field in `settings`. */
function read(settings: AccountSettings, field: Field): unknown {
  const [key, name] = field.split('.');
  const value = (settings as Record<string, unknown>)[key];
  return name === undefined ? value : (value as Record<string, unknown> | undefined)?.[name];
}

/** `settings` with `fields` set; nothing else changes. */
function applied(settings: AccountSettings, fields: ReadonlyMap<Field, unknown>): AccountSettings {
  const next: Record<string, unknown> = { ...settings };
  for (const [field, value] of fields) {
    const [key, name] = field.split('.');
    if (name === undefined) {
      next[key] = value;
    } else {
      next[key] = { ...(next[key] as Record<string, unknown> | undefined), [name]: value };
    }
  }
  return next as AccountSettings;
}

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
  // Changes not yet saved, by field, shown over the stored settings when
  // they load.
  #unsaved = new Map<Field, unknown>();
  // Which change set each unsaved field last: a change undoes or forgets
  // only the fields it still owns, even when another set the same value.
  #owners = new Map<Field, number>();
  // What each unsaved field showed before its first unsaved change: what a
  // failure puts back while the stored settings are not known. A second
  // change to the same field keeps it, so an unconfirmed value never
  // becomes the value to return to.
  #baseline = new Map<Field, unknown>();
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
    this.#unsaved = new Map();
    this.#owners = new Map();
    this.#baseline = new Map();
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
          this.#set(applied(stored, this.#unsaved), null);
        }
        return true;
      },
      // A change reports its own failure.
      () => false
    );
    this.#known = known;
    return known.then(() => {});
  }

  async change(patch: SettingsPatch) {
    const epoch = this.#epoch;
    const changed = fieldsOf(patch);
    const fields = [...changed.keys()];
    const change = ++this.#changes;
    for (const [field, value] of changed) {
      if (!this.#unsaved.has(field)) this.#baseline.set(field, read(this.#current, field));
      this.#unsaved.set(field, value);
      this.#owners.set(field, change);
    }
    this.#set(applied(this.#current, changed), null);
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
      this.#undo(change, fields);
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
        if (epoch === this.#epoch) this.#undo(change, fields);
        return;
      }
      if (epoch !== this.#epoch) return;
      this.#stored = sent;
      this.#forget(change, fields);
    });
    this.#saving = save.catch(() => {});
    await save;
  }

  /** The fields of `fields` that `change` still owns: no newer change set them. */
  #owned(change: number, fields: Field[]): Field[] {
    return fields.filter((field) => this.#owners.get(field) === change);
  }

  /** Drops a saved change's fields from the unsaved ones, where it still owns them. */
  #forget(change: number, fields: Field[]) {
    for (const field of this.#owned(change, fields)) {
      this.#unsaved.delete(field);
      this.#owners.delete(field);
      this.#baseline.delete(field);
    }
  }

  /**
   * Puts back the fields of a change that was not saved, with a failure: to
   * what the core holds now (an earlier save may have confirmed a value
   * since the change began), or, while the stored settings are not known,
   * to what the field showed before its first unsaved change. A field a
   * newer change set keeps the newer value.
   */
  #undo(change: number, fields: Field[]) {
    const stored = this.#stored;
    const owned = this.#owned(change, fields);
    const restored = new Map(
      owned.map((field) => [field, stored ? read(stored, field) : this.#baseline.get(field)])
    );
    this.#forget(change, fields);
    this.#set(applied(this.#current, restored), SAVE_FAILED);
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
