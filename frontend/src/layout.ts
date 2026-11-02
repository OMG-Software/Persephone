// The room list width. Kept in memory only: the page may not use web
// storage, and the account settings have no field for it.
export const SIDEBAR_DEFAULT = 292;
export const SIDEBAR_MIN = 200;
export const SIDEBAR_MAX = 480;
export const SIDEBAR_STEP = 16;

/** Keep a width inside the allowed range; a value that is not a number gives the default. */
export function clampSidebar(width: number): number {
  if (!Number.isFinite(width)) return SIDEBAR_DEFAULT;
  return Math.round(Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, width)));
}

/** The width after a key press on the resize handle, or null for a key that does not resize. */
export function keyedSidebar(width: number, key: string): number | null {
  switch (key) {
    case 'ArrowLeft':
      return clampSidebar(width - SIDEBAR_STEP);
    case 'ArrowRight':
      return clampSidebar(width + SIDEBAR_STEP);
    case 'Home':
      return SIDEBAR_MIN;
    case 'End':
      return SIDEBAR_MAX;
    default:
      return null;
  }
}

/**
 * The width while the handle is dragged. `ratio` is pointer pixels per CSS
 * pixel, measured when the drag starts, so the interface zoom does not make
 * the edge move faster or slower than the pointer.
 */
export function draggedSidebar(startWidth: number, startX: number, x: number, ratio: number): number {
  const scale = Number.isFinite(ratio) && ratio > 0 ? ratio : 1;
  return clampSidebar(startWidth + (x - startX) / scale);
}
