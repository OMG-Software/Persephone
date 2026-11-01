import type { TimelineItem, TimelinePage } from '../../crates/contracts/generated/contracts';

export const SEARCH_CONTEXT_PAGES = 5;
export const SEARCH_CONTEXT_ITEMS = 500;

/** Locate an event with bounded history reads. Never apply a stale room's pages. */
export async function findSearchContext(
  roomId: string,
  eventId: string,
  initial: TimelineItem[],
  cursor: string | null,
  page: (roomId: string, cursor: string | null) => Promise<TimelinePage>,
  current: () => boolean
): Promise<{ items: TimelineItem[]; cursor: string | null; found: boolean } | null> {
  let items = initial.slice(0, SEARCH_CONTEXT_ITEMS);
  const found = () => items.some((item) => item.kind !== 'gap' && item.event_id === eventId);
  const seenCursors = new Set<string>();
  for (let count = 0; current() && !found() && cursor !== null && count < SEARCH_CONTEXT_PAGES; count++) {
    if (seenCursors.has(cursor) || items.length >= SEARCH_CONTEXT_ITEMS) break;
    seenCursors.add(cursor);
    const older = await page(roomId, cursor);
    if (!current()) return null;
    const seen = new Set<string>();
    items = [...older.items, ...items].filter((item) => {
      if (item.kind === 'gap') return true;
      if (seen.has(item.event_id)) return false;
      seen.add(item.event_id);
      return true;
    }).slice(0, SEARCH_CONTEXT_ITEMS);
    cursor = older.nextCursor;
  }
  return current() ? { items, cursor, found: found() } : null;
}
