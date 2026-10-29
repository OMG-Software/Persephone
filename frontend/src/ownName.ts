// The signed-in account's display name, for its labels. The newest word
// wins: a read sent before a save or an account change is ignored when it
// answers, and a save made for another account changes nothing.
import type { DisplayName } from './bridge';

export class OwnNameKeeper {
  readonly #read: () => Promise<DisplayName>;
  readonly #show: (name: string | null) => void;
  #userId: string | null = null;
  // Advances on each account change and save: only a read sent since the
  // last of them is shown.
  #version = 0;

  constructor(read: () => Promise<DisplayName>, show: (name: string | null) => void) {
    this.#read = read;
    this.#show = show;
  }

  /** The signed-in account is now `userId`, or none: no name until it is read. */
  account(userId: string | null): Promise<void> {
    this.#userId = userId;
    const version = ++this.#version;
    this.#show(null);
    if (!userId) return Promise.resolve();
    return this.#read().then(
      (name) => {
        if (version === this.#version) this.#show(name.displayName);
      },
      // With none read, the labels show only the user ID.
      () => {}
    );
  }

  /** A save for `userId` set `name`, or removed it when null. */
  saved(userId: string, name: string | null) {
    if (userId !== this.#userId) return;
    this.#version += 1;
    this.#show(name);
  }
}
