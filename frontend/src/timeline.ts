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
 *
 * An event already shown is replaced by its form in `newest`, so a notice that
 * becomes readable (for example once its key arrives) is updated in place.
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
  const fresh = new Map<string, TimelineItem>();
  for (const item of newest) {
    if (item.kind !== 'gap') fresh.set(item.event_id, item);
  }
  items = items.map((item) => (item.kind === 'gap' ? item : (fresh.get(item.event_id) ?? item)));
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

/** Most characters of a quoted message shown above a reply. */
export const MAX_QUOTE_CHARS = 140;

/** What a reply shows of the message it answers. */
export type Quote = { sender: string; text: string };

/**
 * The quote of the event `eventId` from the loaded `items`, or `null` when
 * it is not loaded. Only what this page shows of the event is quoted (its
 * first line, cut to `MAX_QUOTE_CHARS`), never a sender's copy of it.
 */
export function quoteOf(items: ShownItem[], eventId: string): Quote | null {
  const item = items.find((entry) => entry.kind !== 'gap' && entry.event_id === eventId);
  if (!item || item.kind === 'gap') return null;
  switch (item.kind) {
    case 'message': {
      const line = item.body.split('\n').find((part) => part.trim() !== '') ?? '';
      const chars = Array.from(line.trim());
      const text =
        chars.length > MAX_QUOTE_CHARS ? chars.slice(0, MAX_QUOTE_CHARS).join('') + '…' : chars.join('');
      return { sender: item.sender, text };
    }
    case 'image':
      return { sender: item.sender, text: 'Image' };
    case 'notice':
      return { sender: item.sender, text: 'A message that is not shown' };
    case 'state':
      return { sender: item.sender, text: 'A change to the room' };
  }
}

/** A timeline item as shown: a message may carry its newest edit. */
export type ShownItem = Exclude<TimelineItem, { kind: 'edit' }> & { edited?: boolean };

/**
 * The items to show: edits are not lines of their own, and each message
 * shows the text of its newest edit (the last in the room's order), marked
 * as edited. The core sends only edits by the message's own sender; an edit
 * whose sender differs from the message's here is still ignored.
 */
export function applyEdits(items: TimelineItem[]): ShownItem[] {
  const newest = new Map<string, { sender: string; body: string }>();
  for (const item of items) {
    if (item.kind === 'edit') newest.set(item.replaces, { sender: item.sender, body: item.body });
  }
  const shown: ShownItem[] = [];
  for (const item of items) {
    if (item.kind === 'edit') continue;
    const edit = item.kind === 'message' ? newest.get(item.event_id) : undefined;
    shown.push(
      item.kind === 'message' && edit && edit.sender === item.sender
        ? { ...item, body: edit.body, edited: true }
        : item
    );
  }
  return shown;
}
