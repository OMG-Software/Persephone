// The room list width. Kept in memory only: the page may not use web
// storage, and the account settings have no field for it.
export const SIDEBAR_DEFAULT = 292;
export const SIDEBAR_MIN = 200;
export const SIDEBAR_MAX = 480;
export const SIDEBAR_STEP = 16;
// The narrowest the room itself (timeline and composer) may become, and the
// member panel's width (`.member-panel` in style.css), in CSS pixels.
export const ROOM_MIN = 320;
export const MEMBER_PANEL_WIDTH = 248;

/**
 * The widest the room list may be in a window `available` CSS pixels wide:
 * the room keeps at least ROOM_MIN, but the list never goes below its minimum.
 */
export function sidebarLimit(available: number): number {
  if (!Number.isFinite(available)) return SIDEBAR_MAX;
  return Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, Math.floor(available - ROOM_MIN)));
}

/** Whether the member panel fits beside a room list `sidebar` pixels wide, leaving the room ROOM_MIN. */
export function membersFit(available: number, sidebar: number): boolean {
  return Number.isFinite(available) && available - sidebar - MEMBER_PANEL_WIDTH >= ROOM_MIN;
}

/** Keep a width inside the allowed range; a value that is not a number gives the default. */
export function clampSidebar(width: number, max = SIDEBAR_MAX): number {
  if (!Number.isFinite(width)) return Math.min(SIDEBAR_DEFAULT, max);
  return Math.round(Math.min(max, SIDEBAR_MAX, Math.max(SIDEBAR_MIN, width)));
}

/** The width after a key press on the resize handle, or null for a key that does not resize. */
export function keyedSidebar(width: number, key: string, max = SIDEBAR_MAX): number | null {
  switch (key) {
    case 'ArrowLeft':
      return clampSidebar(width - SIDEBAR_STEP, max);
    case 'ArrowRight':
      return clampSidebar(width + SIDEBAR_STEP, max);
    case 'Home':
      return SIDEBAR_MIN;
    case 'End':
      return clampSidebar(max, max);
    default:
      return null;
  }
}

/**
 * The width while the handle is dragged. `ratio` is pointer pixels per CSS
 * pixel, measured when the drag starts, so the interface zoom does not make
 * the edge move faster or slower than the pointer.
 */
export function draggedSidebar(
  startWidth: number,
  startX: number,
  x: number,
  ratio: number,
  max = SIDEBAR_MAX
): number {
  const scale = Number.isFinite(ratio) && ratio > 0 ? ratio : 1;
  return clampSidebar(startWidth + (x - startX) / scale, max);
}
