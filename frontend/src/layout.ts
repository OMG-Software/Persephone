// The widths of the room list and the member panel. Kept in memory only:
// the page may not use web storage, and the account settings have no field
// for them.

/** The range one resizable column may take, in CSS pixels. */
export type WidthRange = { min: number; max: number; fallback: number; step: number };

export const SIDEBAR_DEFAULT = 292;
export const SIDEBAR_MIN = 200;
export const SIDEBAR_MAX = 480;
export const SIDEBAR_STEP = 16;
export const SIDEBAR: WidthRange = {
  min: SIDEBAR_MIN,
  max: SIDEBAR_MAX,
  fallback: SIDEBAR_DEFAULT,
  step: SIDEBAR_STEP
};

// The member panel, at the right of the room. Its handle is on its left
// edge, so moving the handle left makes it wider.
export const MEMBERS_DEFAULT = 248;
export const MEMBERS_MIN = 200;
export const MEMBERS_MAX = 420;
export const MEMBERS: WidthRange = {
  min: MEMBERS_MIN,
  max: MEMBERS_MAX,
  fallback: MEMBERS_DEFAULT,
  step: SIDEBAR_STEP
};

// The narrowest the room itself (timeline and composer) may become.
export const ROOM_MIN = 320;

/** The widest a column may be when `free` CSS pixels are left beside the room's minimum. */
function limit(range: WidthRange, free: number): number {
  if (!Number.isFinite(free)) return range.max;
  return Math.max(range.min, Math.min(range.max, Math.floor(free)));
}

/**
 * The widest the room list may be in a window `available` CSS pixels wide:
 * the room keeps at least ROOM_MIN, but the list never goes below its minimum.
 */
export function sidebarLimit(available: number): number {
  return limit(SIDEBAR, available - ROOM_MIN);
}

/** Whether the member panel, at its narrowest, fits beside a room list `sidebar` pixels wide, leaving the room ROOM_MIN. */
export function membersFit(available: number, sidebar: number): boolean {
  return Number.isFinite(available) && available - sidebar - MEMBERS_MIN >= ROOM_MIN;
}

/** The widest the member panel may be beside a room list `sidebar` pixels wide. */
export function membersLimit(available: number, sidebar: number): number {
  return limit(MEMBERS, available - sidebar - ROOM_MIN);
}

/** Keep a width inside `range` and under `max`; a value that is not a number gives the default. */
export function clampWidth(range: WidthRange, width: number, max = range.max): number {
  if (!Number.isFinite(width)) return Math.min(range.fallback, max);
  return Math.round(Math.min(max, range.max, Math.max(range.min, width)));
}

/**
 * The width after a key press on a resize handle, or null for a key that
 * does not resize. `grow` is the arrow that moves the handle away from the
 * column: ArrowRight for the room list, ArrowLeft for the member panel.
 */
export function keyedWidth(
  range: WidthRange,
  width: number,
  key: string,
  max = range.max,
  grow: 'ArrowRight' | 'ArrowLeft' = 'ArrowRight'
): number | null {
  const shrink = grow === 'ArrowRight' ? 'ArrowLeft' : 'ArrowRight';
  switch (key) {
    case shrink:
      return clampWidth(range, width - range.step, max);
    case grow:
      return clampWidth(range, width + range.step, max);
    case 'Home':
      return range.min;
    case 'End':
      return clampWidth(range, max, max);
    default:
      return null;
  }
}

/**
 * The width while a handle is dragged. `ratio` is pointer pixels per CSS
 * pixel, measured when the drag starts, so the interface zoom does not make
 * the edge move faster or slower than the pointer. `direction` is 1 when the
 * column grows as the pointer moves right, and -1 when it grows to the left.
 */
export function draggedWidth(
  range: WidthRange,
  startWidth: number,
  startX: number,
  x: number,
  ratio: number,
  max = range.max,
  direction: 1 | -1 = 1
): number {
  const scale = Number.isFinite(ratio) && ratio > 0 ? ratio : 1;
  return clampWidth(range, startWidth + (direction * (x - startX)) / scale, max);
}

/** The room list width kept in range; see `clampWidth`. */
export function clampSidebar(width: number, max = SIDEBAR_MAX): number {
  return clampWidth(SIDEBAR, width, max);
}

/** The room list width after a key press; see `keyedWidth`. */
export function keyedSidebar(width: number, key: string, max = SIDEBAR_MAX): number | null {
  return keyedWidth(SIDEBAR, width, key, max);
}

/** The room list width while its handle is dragged; see `draggedWidth`. */
export function draggedSidebar(
  startWidth: number,
  startX: number,
  x: number,
  ratio: number,
  max = SIDEBAR_MAX
): number {
  return draggedWidth(SIDEBAR, startWidth, startX, x, ratio, max);
}
