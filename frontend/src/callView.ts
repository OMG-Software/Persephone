// Which video feed the call window shows large (ADR-0065), as plain data. A
// feed is a cell of the call grid, named by its key (callWindow.ts). The
// viewer's pick is one of three things: no pick (the window decides), the
// grid (the viewer asked for it) or one feed. The window shows the grid
// unless a feed is large. Nothing here asks the core for another picture
// size: the pictures stay at the sizes the worker sends, and CSS scales them.

/** The key of the shared screen feed in `callGrid`. */
export const SCREEN_KEY = 'screen';

export type ViewPick =
  // The viewer chose nothing: a shared screen shows large, else the grid.
  | { kind: 'auto' }
  // The viewer chose the grid, also while a screen is shared.
  | { kind: 'grid' }
  | { kind: 'feed'; key: string };

export const AUTO_PICK: ViewPick = { kind: 'auto' };
export const GRID_PICK: ViewPick = { kind: 'grid' };

/**
 * The keys of the feeds that can show large: the cells with a picture. A
 * cell whose camera is off has no tile, so it is no feed: a pick of it
 * drops, and selecting it changes nothing. The functions below take these
 * keys.
 */
export function feedKeys(cells: readonly { key: string; tile: number | null }[]): string[] {
  return cells.filter((cell) => cell.tile !== null).map((cell) => cell.key);
}

/**
 * The pick after the feeds changed. A feed that went away (the person left,
 * the camera went off, the share ended) drops its pick, so the window goes
 * back to the grid, or to the screen share by the automatic rule. A choice
 * of the grid lasts as long as a screen is shared; the next share is shown
 * large again.
 */
export function keepPick(pick: ViewPick, keys: readonly string[]): ViewPick {
  if (pick.kind === 'feed' && !keys.includes(pick.key)) return AUTO_PICK;
  if (pick.kind === 'grid' && !keys.includes(SCREEN_KEY)) return AUTO_PICK;
  return pick;
}

/**
 * The key of the feed to show large, or null for the grid. A pick of a feed
 * that is in the call always wins. Without a pick, a shared screen is large.
 */
export function largeKey(pick: ViewPick, keys: readonly string[]): string | null {
  const kept = keepPick(pick, keys);
  if (kept.kind === 'feed') return kept.key;
  if (kept.kind === 'auto' && keys.includes(SCREEN_KEY)) return SCREEN_KEY;
  return null;
}

/**
 * The pick after the viewer selects feed `key`. The feed that is large goes
 * back to the grid; any other feed becomes large, and the one that was large
 * joins the row again. A key that is not in the call changes nothing.
 */
export function selectFeed(pick: ViewPick, key: string, keys: readonly string[]): ViewPick {
  if (!keys.includes(key)) return keepPick(pick, keys);
  return largeKey(pick, keys) === key ? GRID_PICK : { kind: 'feed', key };
}

/** The "Show all" pick: the grid. */
export function showAll(): ViewPick {
  return GRID_PICK;
}

export type Layout<T> = {
  // The feed that shows large, or null for the grid.
  large: T | null;
  // The other feeds in grid order: the cells of the grid, or the row of small
  // tiles along the bottom.
  rest: T[];
};

/** Splits the cells into the large feed and the others, keeping their order. */
export function splitFeeds<T extends { key: string }>(cells: readonly T[], large: string | null): Layout<T> {
  const chosen = large === null ? undefined : cells.find((cell) => cell.key === large);
  return {
    large: chosen ?? null,
    rest: cells.filter((cell) => cell !== chosen)
  };
}
