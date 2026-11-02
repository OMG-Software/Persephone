/**
 * Keyboard movement inside an open menu (WAI-ARIA menu button pattern).
 * Returns the index of the item to focus, or null when the key does not
 * move focus or no item can take it. Disabled items are skipped.
 */
export function menuTarget(key: string, current: number, enabled: readonly boolean[]): number | null {
  const count = enabled.length;
  if (!enabled.some(Boolean)) return null;
  /** Step from `start` in `step` direction, wrapping, to the next enabled item. */
  const seek = (start: number, step: 1 | -1) => {
    for (let moved = 0, at = start; moved < count; moved += 1) {
      at = (at + step + count) % count;
      if (enabled[at]) return at;
    }
    return null;
  };
  switch (key) {
    case 'ArrowDown':
      return seek(current < 0 || current >= count ? -1 : current, 1);
    case 'ArrowUp':
      return seek(current < 0 || current >= count ? count : current, -1);
    case 'Home':
      return seek(-1, 1);
    case 'End':
      return seek(count, -1);
    default:
      return null;
  }
}
