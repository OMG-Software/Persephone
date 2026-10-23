// Message times for display. Pure functions, no host calls. Times come from
// the core as numbers it does not vouch for: a homeserver sets a message's
// time, so it can be wrong, and any value may be outside what a Date shows.

/** Where and how to format; the reader's locale and time zone by default. */
export type TimeOptions = { locale?: string; timeZone?: string };

/** A Date for `millis`, or null when it is not a time JavaScript can show. */
export function toDate(millis: number | null | undefined): Date | null {
  if (typeof millis !== 'number' || !Number.isSafeInteger(millis) || millis < 0) return null;
  const date = new Date(millis);
  return Number.isNaN(date.getTime()) ? null : date;
}

/** The time of day, such as "14:02". */
export function clockTime(date: Date, options: TimeOptions = {}): string {
  return new Intl.DateTimeFormat(options.locale, {
    hour: '2-digit',
    minute: '2-digit',
    timeZone: options.timeZone
  }).format(date);
}

/** The full date and time, for a tooltip. */
export function fullTime(date: Date, options: TimeOptions = {}): string {
  return new Intl.DateTimeFormat(options.locale, {
    dateStyle: 'full',
    timeStyle: 'medium',
    timeZone: options.timeZone
  }).format(date);
}

/** The calendar day of `date` where it is read, as "YYYY-MM-DD". */
export function dayKey(date: Date, options: TimeOptions = {}): string {
  const parts = new Intl.DateTimeFormat('en-CA', {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    timeZone: options.timeZone
  }).formatToParts(date);
  const part = (type: string) => parts.find((item) => item.type === type)?.value ?? '';
  return `${part('year')}-${part('month')}-${part('day')}`;
}

/** "Today", "Yesterday", or the date, for a day divider. */
export function dayLabel(date: Date, now: Date, options: TimeOptions = {}): string {
  const day = dayKey(date, options);
  const today = dayKey(now, options);
  // Whole calendar days between the two, from their keys, so daylight-saving
  // changes cannot shift the answer.
  const days = (Date.parse(today) - Date.parse(day)) / (24 * 60 * 60 * 1000);
  if (days === 0) return 'Today';
  if (days === 1) return 'Yesterday';
  return new Intl.DateTimeFormat(options.locale, {
    weekday: 'long',
    day: 'numeric',
    month: 'long',
    year: day.slice(0, 4) === today.slice(0, 4) ? undefined : 'numeric',
    timeZone: options.timeZone
  }).format(date);
}

/**
 * The day divider label before each item, or null where the day is the same
 * as the previous item's with a time. Items without a time get none.
 */
export function dayDividers(
  times: (number | null | undefined)[],
  now: Date,
  options: TimeOptions = {}
): (string | null)[] {
  let previous: string | null = null;
  return times.map((millis) => {
    const date = toDate(millis);
    if (!date) return null;
    const day = dayKey(date, options);
    if (day === previous) return null;
    previous = day;
    return dayLabel(date, now, options);
  });
}
