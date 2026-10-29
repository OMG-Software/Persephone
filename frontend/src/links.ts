// Links in message text. Pure functions, no host calls: the page only finds
// links and hands them to the core, which asks the user before anything
// opens (ADR-0024).

/** Most links found in one message; the rest stay plain text. */
export const MAX_LINKS = 20;

/** One piece of a message body: plain text, or a link's text. */
export type BodyPart = { text: string } | { link: string };

// An http or https address up to the next space. Trailing punctuation that
// usually ends a sentence is taken off below.
const LINK = /https?:\/\/[^\s<>"]+/giu;
// Punctuation taken off the end of a link. Trimmed with a loop rather than
// a `[…]+$` pattern, which backtracks quadratically on a long run of it.
const TRAILING = new Set(['.', ',', ';', ':', '!', '?', "'", '"', ')', ']', '}']);

/** `text` without the sentence punctuation at its end. */
function trimTrailing(text: string): string {
  let end = text.length;
  while (end > 0 && TRAILING.has(text[end - 1])) end -= 1;
  return text.slice(0, end);
}

/**
 * Splits `body` into text and links. A link keeps the exact text the sender
 * wrote; the core checks it again, and the page never opens it itself.
 */
export function bodyParts(body: string): BodyPart[] {
  const parts: BodyPart[] = [];
  let at = 0;
  let found = 0;
  for (const match of body.matchAll(LINK)) {
    if (found === MAX_LINKS) break;
    const start = match.index ?? 0;
    const link = trimTrailing(match[0]);
    if (!/^https?:\/\/[^/?#]/iu.test(link)) continue;
    if (start > at) parts.push({ text: body.slice(at, start) });
    parts.push({ link });
    at = start + link.length;
    found += 1;
  }
  if (at < body.length) parts.push({ text: body.slice(at) });
  return parts;
}
