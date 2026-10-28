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

/** Most different reactions shown under one message; the most used first. */
export const MAX_REACTION_KEYS = 12;

/** The quick reactions offered; any key the core accepts can be received. */
export const QUICK_REACTIONS: readonly string[] = ['👍', '❤️', '😂', '😮', '😢', '🎉'];

/**
 * One reaction under a message: how many people reacted with `key`, and the
 * event of this account's own reaction with it, if any.
 */
export type ReactionCount = { key: string; count: number; mine: string | null };

/**
 * A timeline item as shown: a message may carry its newest edit, and a
 * message or image the reactions to it.
 */
export type ShownItem = Exclude<
  TimelineItem,
  { kind: 'edit' } | { kind: 'redaction' } | { kind: 'reaction' }
> & {
  edited?: boolean;
  reactions?: ReactionCount[];
};

/** The reactions to each event: one per sender and key, deleted ones left out. */
function countReactions(
  items: TimelineItem[],
  deleted: Set<string>,
  ownUserId: string | null
): Map<string, ReactionCount[]> {
  const byTarget = new Map<string, Map<string, { senders: Set<string>; mine: string | null }>>();
  for (const item of items) {
    if (item.kind !== 'reaction' || deleted.has(item.event_id)) continue;
    let keys = byTarget.get(item.reacts_to);
    if (!keys) byTarget.set(item.reacts_to, (keys = new Map()));
    let entry = keys.get(item.key);
    if (!entry) keys.set(item.key, (entry = { senders: new Set(), mine: null }));
    entry.senders.add(item.sender);
    if (ownUserId !== null && item.sender === ownUserId) entry.mine = item.event_id;
  }
  const counted = new Map<string, ReactionCount[]>();
  for (const [target, keys] of byTarget) {
    const list = Array.from(keys, ([key, entry]) => ({
      key,
      count: entry.senders.size,
      mine: entry.mine
    }));
    // A stable sort keeps first-seen order among equal counts.
    list.sort((a, b) => b.count - a.count);
    counted.set(target, list.slice(0, MAX_REACTION_KEYS));
  }
  return counted;
}

/**
 * The items to show. Edits and deletions are not lines of their own: a
 * deleted message shows as deleted, and each other message shows the text
 * of its newest edit (the last in the room's order), marked as edited. The
 * core sends only edits by the message's own sender; an edit whose sender
 * differs from the message's here is still ignored.
 */
export function applyRelations(items: TimelineItem[], ownUserId: string | null = null): ShownItem[] {
  const newest = new Map<string, { sender: string; body: string }>();
  const deleted = new Set<string>();
  for (const item of items) {
    if (item.kind === 'edit') newest.set(item.replaces, { sender: item.sender, body: item.body });
    if (item.kind === 'redaction') deleted.add(item.redacts);
  }
  const reactions = countReactions(items, deleted, ownUserId);
  const shown: ShownItem[] = [];
  for (const item of items) {
    if (item.kind === 'edit' || item.kind === 'redaction' || item.kind === 'reaction') continue;
    if (item.kind !== 'gap' && item.kind !== 'state' && deleted.has(item.event_id)) {
      shown.push({
        kind: 'notice',
        event_id: item.event_id,
        sender: item.sender,
        notice: 'deleted',
        origin_server_ts: item.origin_server_ts
      });
      continue;
    }
    const edit = item.kind === 'message' ? newest.get(item.event_id) : undefined;
    const edited: ShownItem =
      item.kind === 'message' && edit && edit.sender === item.sender
        ? { ...item, body: edit.body, edited: true }
        : item;
    const counts =
      edited.kind === 'message' || edited.kind === 'image' ? reactions.get(edited.event_id) : undefined;
    shown.push(counts ? { ...edited, reactions: counts } : edited);
  }
  return shown;
}

/** Kinds that count as messages for the unread count and the read marker. */
const MESSAGE_KINDS = new Set<ShownItem['kind']>(['message', 'image', 'notice']);

/**
 * Where the "New messages" line goes when a room with `unread` unread
 * messages opens: before the `unread`-th newest message from someone else,
 * or before the oldest loaded one when fewer are loaded. `null` when there
 * is nothing unread or nothing to mark.
 */
export function firstUnread(
  items: ShownItem[],
  unread: number,
  ownUserId: string | null
): string | null {
  if (!(unread > 0)) return null;
  let found: string | null = null;
  let counted = 0;
  for (let index = items.length - 1; index >= 0 && counted < unread; index--) {
    const item = items[index];
    if (!MESSAGE_KINDS.has(item.kind) || item.kind === 'gap') continue;
    if (ownUserId !== null && item.sender === ownUserId) continue;
    found = item.event_id;
    counted += 1;
  }
  return found;
}

/** The newest event in `items` that a read marker may point at, or `null`. */
export function newestEvent(items: TimelineItem[]): string | null {
  for (let index = items.length - 1; index >= 0; index--) {
    const item = items[index];
    if (item.kind !== 'gap') return item.event_id;
  }
  return null;
}
