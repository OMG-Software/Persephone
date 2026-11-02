import type { LocalSearchFilters, TimelineItem } from '../../crates/contracts/generated/contracts';

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

/** Check the hostname and optional u16 port accepted by the locked Ruma validator. */
function validMatrixServerName(server: string): boolean {
  let hostEnd: number;
  if (server.startsWith('[')) {
    hostEnd = server.indexOf(']') + 1;
    if (hostEnd === 0) return false;
    try {
      // URL parsing validates bracketed IPv6 locally. It does not make a request.
      new URL('http://' + server.slice(0, hostEnd));
    } catch { return false; }
  } else {
    const colon = server.indexOf(':');
    hostEnd = colon === -1 ? server.length : colon;
    if (!/^[A-Za-z0-9.-]+$/.test(server.slice(0, hostEnd))) return false;
  }
  if (hostEnd === server.length) return true;
  const port = server.slice(hostEnd + 1);
  return server[hostEnd] === ':' && /^\+?\d+$/.test(port) && Number(port) <= 65535;
}

/** Validate an exact Matrix sender and an inclusive/exclusive local date range. */
export function searchFilters(sender: string, from: string, before: string): LocalSearchFilters | null {
  const fromMs = from ? localTimestamp(from) : null;
  const beforeMs = before ? localTimestamp(before) : null;
  const exact = sender.trim();
  if (/[\u0000-\u001f\u007f-\u009f]/u.test(sender) ||
      (exact && (!/^@[^\s:]+:[^\s]+$/.test(exact) || new TextEncoder().encode(exact).length > 255 ||
        !validMatrixServerName(exact.slice(exact.indexOf(':') + 1)))) ||
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

/** Bound accessible control names without exposing a whole message in each button. */
export function pinSummary(item?: TimelineItem, eventId = ''): string {
  // Core event IDs contain at most 255 UTF-8 bytes. Keep the complete valid ID,
  // including its suffix, so identical text and unavailable pins remain distinct.
  const target = (eventId || (item && item.kind !== 'gap' ? item.event_id : '') || '').slice(0, 255);
  if (!item || item.kind === 'gap') return 'Unavailable pinned message' + (target ? ': ' + target : '');
  const content = item.kind === 'message' ? item.body : item.kind === 'file' || item.kind === 'image' ? item.name : 'Message';
  const bounded = (value: string, limit: number) => [...value].slice(0, limit).join('');
  return `Message from ${bounded(item.sender, 80)}: ${bounded(content, 120)}` + (target ? '; event ' + target : '');
}

/** Reject old attachment responses, including a change away and back to the same filters. */
export function createAttachmentQueryGuard() {
  let selected: string | null = null;
  let revision = 0;
  return {
    select(query: string): boolean {
      if (selected === query) return false;
      selected = query; revision += 1;
      return true;
    },
    capture(query: string): () => boolean {
      const asked = revision;
      return () => selected === query && revision === asked;
    }
  };
}
