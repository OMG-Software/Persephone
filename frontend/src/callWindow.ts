// The call window's grid (ADR-0060), as plain data: one cell for you, one
// for each other device in the call and one for a shared screen. A cell
// names the video tile that shows its camera, or none, and then the window
// shows a placeholder. User IDs are untrusted text for text rendering only.
import type { CallStatus } from '../../crates/contracts/generated/contracts';
import { deviceKey, inCall } from './calls.ts';
import { SCREEN_TILE } from './video.ts';

/** The heading of the participant list. */
export const ROSTER_LABEL = 'Participants';
/** How long the window shows the end of a call before it closes itself. */
export const CALL_WINDOW_CLOSE_MS = 4000;
/**
 * When the call window closes itself (ADR-0060): `delay` after a call that
 * it saw ends. A window brought to the front in that time stays open, so a
 * click on the call bar's "Call window" is not lost; it then waits for the
 * user's Close or a next call. A first status that is already `ended` is
 * a call the window was opened for that ended before the window could
 * read it, so it closes too.
 */
export class SelfClose {
  #seen = false;
  #first = true;
  #timer: ReturnType<typeof setTimeout> | undefined;
  readonly #close: () => void;
  readonly #delay: number;

  constructor(close: () => void, delay = CALL_WINDOW_CLOSE_MS) {
    this.#close = close;
    this.#delay = delay;
  }

  /**
   * Follows the call: `running` is true while it joins or runs, `ended`
   * when a call has ended (not idle).
   */
  status(running: boolean, ended = false) {
    if (this.#first && ended) this.#seen = true;
    this.#first = false;
    if (running) {
      this.#seen = true;
      this.#cancel();
    } else if (this.#seen && this.#timer === undefined) {
      this.#timer = setTimeout(() => {
        this.#timer = undefined;
        this.#seen = false;
        this.#close();
      }, this.#delay);
    }
  }

  /** The window came to the front. Only a close that waits is dropped. */
  shown() {
    if (this.#timer === undefined) return;
    this.#cancel();
    this.#seen = false;
  }

  stop() {
    this.#cancel();
  }

  #cancel() {
    clearTimeout(this.#timer);
    this.#timer = undefined;
  }
}

/**
 * Whether the technical details panel stays open when `next` replaces
 * `previous` (ADR-0066). It stays only while the same room's call goes on:
 * both statuses are in a call, in the same room. The panel reads once, when
 * it opens, so a call in another room, or a new call after an ended one in
 * the same room, must close it: it would keep the old call's last reading.
 */
export function statsStayOpen(previous: CallStatus, next: CallStatus): boolean {
  return inCall(previous) && inCall(next) && next.roomId === previous.roomId;
}

/** Shown in the main window when the call window cannot open. */
export const CALL_WINDOW_UNAVAILABLE_TEXT = 'The call window could not open. The call goes on in this window.';

/**
 * Opens the call window (ADR-0060) and reports whether the main window must
 * show the call itself: `report(true)` after a failed open, `report(false)`
 * after one that worked. Opens can overlap, as the automatic open and a
 * "Call window" click do, and their replies can come in any order:
 *
 * - An open that worked always counts, because the call page then works:
 *   the host replies only when the page has said it is ready.
 * - A failure counts only for the newest open, and only when no open has
 *   worked since it started.
 * - `forget()` ends the call: no reply from before it counts.
 */
export class CallWindowOpener {
  #request = 0;
  #opened = 0;
  #epoch = 0;
  readonly #open: () => Promise<void>;
  readonly #report: (failed: boolean) => void;

  constructor(open: () => Promise<void>, report: (failed: boolean) => void) {
    this.#open = open;
    this.#report = report;
  }

  open() {
    const request = ++this.#request;
    const opened = this.#opened;
    const epoch = this.#epoch;
    // A synchronous throw is a failed open too.
    new Promise<void>((resolve) => resolve(this.#open())).then(
      () => {
        if (epoch !== this.#epoch) return;
        this.#opened += 1;
        this.#report(false);
      },
      () => {
        if (epoch !== this.#epoch || request !== this.#request || opened !== this.#opened) return;
        this.#report(true);
      }
    );
  }

  forget() {
    this.#epoch += 1;
  }
}

export type CallCell = {
  // Unique in the grid.
  key: string;
  own: boolean;
  userId: string | null;
  // The video tile that shows this cell's picture, or null for a placeholder.
  tile: number | null;
  screen: boolean;
  muted: boolean;
  deafened: boolean;
  speaking: boolean;
};

/**
 * The cells of the grid: you first, then each other device in the order
 * the core lists them, then the shared screen, if any. A device whose
 * camera has no tile gets a placeholder cell.
 */
export function callGrid(status: CallStatus): CallCell[] {
  const ownTile = status.video.find((tile) => tile.own && tile.tile !== SCREEN_TILE);
  const cells: CallCell[] = [
    {
      key: 'own',
      own: true,
      userId: null,
      tile: ownTile?.tile ?? null,
      screen: false,
      muted: status.muted,
      deafened: status.deafened,
      speaking: false
    }
  ];
  for (const person of status.participants) {
    const tile = status.video.find(
      (video) =>
        !video.own &&
        video.tile !== SCREEN_TILE &&
        video.participant?.userId === person.userId &&
        video.participant?.deviceId === person.deviceId
    );
    cells.push({
      key: deviceKey(person.userId, person.deviceId),
      own: false,
      userId: person.userId,
      tile: tile?.tile ?? null,
      screen: false,
      muted: person.muted,
      deafened: person.deafened,
      speaking: person.speaking
    });
  }
  const screen = status.video.find((tile) => tile.tile === SCREEN_TILE);
  if (screen) {
    cells.push({
      key: 'screen',
      own: false,
      userId: screen.participant?.userId ?? null,
      tile: SCREEN_TILE,
      screen: true,
      muted: false,
      deafened: false,
      speaking: false
    });
  }
  return cells;
}
