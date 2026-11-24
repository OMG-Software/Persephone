// The call window's grid (ADR-0060), as plain data: one cell for you, one
// for each other device in the call and one for a shared screen. A cell
// names the video tile that shows its camera, or none, and then the window
// shows a placeholder. User IDs are untrusted text for text rendering only.
import type { CallStatus } from '../../crates/contracts/generated/contracts';
import { deviceKey } from './calls.ts';
import { SCREEN_TILE } from './video.ts';

/** The heading of the participant list. */
export const ROSTER_LABEL = 'Participants';
/** How long the window shows the end of a call before it closes itself. */
export const CALL_WINDOW_CLOSE_MS = 4000;
/**
 * When the call window closes itself (ADR-0060): `delay` after a call that
 * it saw ends. A window brought to the front in that time stays open, so a
 * click on the call bar's "Call window" is not lost; it then waits for the
 * user's Close or a next call.
 */
export class SelfClose {
  #seen = false;
  #timer: ReturnType<typeof setTimeout> | undefined;
  readonly #close: () => void;
  readonly #delay: number;

  constructor(close: () => void, delay = CALL_WINDOW_CLOSE_MS) {
    this.#close = close;
    this.#delay = delay;
  }

  /** Follows the call: `running` is true while it joins or runs. */
  status(running: boolean) {
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

/** Shown in the main window when the call window cannot open. */
export const CALL_WINDOW_UNAVAILABLE_TEXT = 'The call window could not open. The call goes on in this window.';

export type CallCell = {
  // Unique in the grid.
  key: string;
  own: boolean;
  userId: string | null;
  // The video tile that shows this cell's picture, or null for a placeholder.
  tile: number | null;
  screen: boolean;
  muted: boolean;
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
      speaking: false
    });
  }
  return cells;
}
