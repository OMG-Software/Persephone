import type { TimelineItem, TimelinePage } from '../../crates/contracts/generated/contracts';

export const SEARCH_CONTEXT_PAGES = 5;
export const SEARCH_CONTEXT_ITEMS = 500;

/** Merge chronological pages without duplicate event IDs. Preserve visible gaps. */
function mergeContext(older: TimelineItem[], newer: TimelineItem[]): TimelineItem[] {
  const seen = new Set<string>();
  /** Keep every gap and the first occurrence of each chronological event ID. */
  function keepItem(item: TimelineItem): boolean {
    if (item.kind === 'gap') return true;
    if (seen.has(item.event_id)) return false;
    seen.add(item.event_id);
    return true;
  }
  return [...older, ...newer].filter(keepItem);
}

/** Refuse a page that cannot fit in the bounded context without losing its cursor. */
function checkPage(page: TimelinePage): void {
  if (page.items.length > SEARCH_CONTEXT_ITEMS) throw new RangeError('Search context page is too large.');
}

/**
 * Locate an event in at most five reads and 500 retained items. An oversized initial
 * context is replaced by a fresh newest page. Stop before a merge exceeds the item
 * limit, preserving the input cursor so that the unretained page can be read again.
 * Never apply a stale room's pages.
 */
export async function findSearchContext(
  roomId: string,
  eventId: string,
  initial: TimelineItem[],
  cursor: string | null,
  page: (roomId: string, cursor: string | null) => Promise<TimelinePage>,
  current: () => boolean
): Promise<{ items: TimelineItem[]; cursor: string | null; found: boolean } | null> {
  if (!current()) return null;
  let items: TimelineItem[];
  let count = 0;
  if (initial.length > SEARCH_CONTEXT_ITEMS) {
    const newest = await page(roomId, null);
    if (!current()) return null;
    checkPage(newest);
    items = mergeContext([], newest.items);
    cursor = newest.nextCursor;
    count++;
  } else {
    items = mergeContext([], initial);
  }
  /** Gaps have no event identity and cannot match a cached search result. */
  function isTarget(item: TimelineItem): boolean {
    return item.kind !== 'gap' && item.event_id === eventId;
  }
  /** Only an event in the retained context can satisfy the search. */
  function found(): boolean {
    return items.some(isTarget);
  }
  const seenCursors = new Set<string>();
  while (current() && !found() && cursor !== null && count < SEARCH_CONTEXT_PAGES) {
    if (seenCursors.has(cursor) || items.length >= SEARCH_CONTEXT_ITEMS) break;
    seenCursors.add(cursor);
    const older = await page(roomId, cursor);
    if (!current()) return null;
    count++;
    checkPage(older);
    const merged = mergeContext(older.items, items);
    if (merged.length > SEARCH_CONTEXT_ITEMS) break;
    items = merged;
    cursor = older.nextCursor;
  }
  return current() ? { items, cursor, found: found() } : null;
}
