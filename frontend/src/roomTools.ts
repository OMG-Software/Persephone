import type { LocalSearchFilters, TimelineItem } from './bridge/index.ts';

/** Parse an explicit local date and time. Reject normalized, invalid dates. */
export function localTimestamp(value: string): number | null {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(value)) return null;
  const date = new Date(value);
  const parts = value.split(/[-T:]/).map(Number);
  if (!Number.isSafeInteger(date.getTime()) || date.getTime() < 0 || date.getFullYear() !== parts[0] ||
      date.getMonth() + 1 !== parts[1] || date.getDate() !== parts[2] ||
      date.getHours() !== parts[3] || date.getMinutes() !== parts[4]) return null;
  return date.getTime();
}

/** Validate an exact Matrix sender and an inclusive/exclusive local date range. */
export function searchFilters(sender: string, from: string, before: string): LocalSearchFilters | null {
  const fromMs = from ? localTimestamp(from) : null;
  const beforeMs = before ? localTimestamp(before) : null;
  const exact = sender.trim();
  if ((exact && (!/^@[^\s:]+:[^\s]+$/.test(exact) || new TextEncoder().encode(exact).length > 255)) ||
      (from && fromMs === null) || (before && beforeMs === null) ||
      (fromMs !== null && beforeMs !== null && fromMs >= beforeMs)) return null;
  return { sender: exact || null, fromMs, beforeMs };
}

/** Keep metadata bounded and deduplicate overlapping pages. */
export function appendAttachments(previous: TimelineItem[], page: TimelineItem[]): TimelineItem[] {
  const ids = new Set<string>();
  return [...previous, ...page].filter((item) => {
    if (item.kind !== 'file' && item.kind !== 'image') return false;
    if (ids.has(item.event_id)) return false;
    ids.add(item.event_id);
    return true;
  }).slice(0, 500);
}

/** Show the local zone used by date inputs without storing a preference. */
export const localZone = () => Intl.DateTimeFormat().resolvedOptions().timeZone;
