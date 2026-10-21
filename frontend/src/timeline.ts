// Merging live updates into a loaded timeline. Pure functions, no host calls.
import type { TimelineItem } from '../../crates/contracts/generated/contracts';

/** Most items kept while live updates arrive; older ones are dropped. */
export const MAX_RETAINED_ITEMS = 500;

const gap: TimelineItem = { kind: 'gap', next_cursor: null };

/**
 * Appends what is new in `newest` (the room's newest page) to `items`.
 *
 * The newest message already shown is looked up in the page; everything after
 * it is new. If it is not there, more than a page arrived at once, so a gap
 * marks the messages in between. When more than `max` items would be kept, the
 * oldest are replaced by one leading gap. The caller's older-page cursor stays
 * valid: older pages still come before the oldest message it fetched.
 */
export function mergeNewest(
  items: TimelineItem[],
  newest: TimelineItem[],
  max: number = MAX_RETAINED_ITEMS
): TimelineItem[] {
  let last: string | null = null;
  for (let index = items.length - 1; index >= 0; index--) {
    const item = items[index];
    if (item.kind !== 'gap') {
      last = item.event_id;
      break;
    }
  }
  let merged: TimelineItem[];
  if (last === null) {
    merged = [...items, ...newest];
  } else {
    const at = newest.findIndex((item) => item.kind !== 'gap' && item.event_id === last);
    merged = at >= 0 ? [...items, ...newest.slice(at + 1)] : [...items, gap, ...newest];
  }
  if (merged.length <= max) return merged;
  const kept = merged.slice(merged.length - (max - 1));
  while (kept[0]?.kind === 'gap') kept.shift();
  return [gap, ...kept];
}
