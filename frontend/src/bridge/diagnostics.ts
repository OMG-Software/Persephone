// The page's records for the core's diagnostic log (ADR-0058). Off until the
// core says the log is on. While on, the bridge notes each host call's name,
// outcome code and time, never its arguments or reply, and the page's
// uncaught errors. Records leave in batches of at most MAX_BATCH, at most one
// batch a second; at most MAX_WAITING wait, and more are counted.
import { invoke } from '@tauri-apps/api/core';

export type RecordLevel = 'error' | 'warn' | 'info' | 'debug';

export interface PageRecord {
  level: RecordLevel;
  target: string;
  message: string;
}

/** The host's and the core's limits (`MAX_DIAGNOSTIC_*`). */
export const MAX_BATCH = 64;
export const MAX_TARGET_BYTES = 64;
export const MAX_MESSAGE_BYTES = 1024;
export const MAX_WAITING = 256;
const FLUSH_MS = 1000;

/** Calls whose success is too frequent to note: only failures are kept. */
const QUIET_CALLS = new Set(['call_video_frame', 'image_band', 'diagnostic_records']);

let on = false;
let waiting: PageRecord[] = [];
let dropped = 0;
let timer: ReturnType<typeof setTimeout> | null = null;
const listeners = new Set<(on: boolean) => void>();

const encoder = new TextEncoder();

/** `text` cut to at most `limit` UTF-8 bytes, on a character boundary. */
export function cut(text: string, limit: number): string {
  if (encoder.encode(text).length <= limit) return text;
  let out = '';
  let bytes = 0;
  for (const character of text) {
    const size = encoder.encode(character).length;
    if (bytes + size > limit) break;
    out += character;
    bytes += size;
  }
  return out;
}

/** Only short error codes are kept; anything else is just `error`. */
export function outcomeCode(reason: unknown): string {
  return typeof reason === 'string' && /^[a-z_]{1,48}$/.test(reason) ? reason : 'error';
}

export function diagnosticsOn(): boolean {
  return on;
}

/** Calls `listener` whenever the switch changes; returns the unsubscribe. */
export function onDiagnosticsChange(listener: (on: boolean) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Follows the core's switch. Off drops what waits. */
export function setPageDiagnostics(value: boolean): void {
  if (on === value) return;
  on = value;
  if (!on) {
    waiting = [];
    dropped = 0;
    if (timer !== null) clearTimeout(timer);
    timer = null;
  }
  for (const listener of listeners) listener(on);
}

/** Queues one record while on. */
export function note(level: RecordLevel, target: string, message: string): void {
  if (!on) return;
  if (waiting.length >= MAX_WAITING) {
    dropped += 1;
    return;
  }
  waiting.push({
    level,
    target: cut(target, MAX_TARGET_BYTES),
    message: cut(message, MAX_MESSAGE_BYTES)
  });
  if (timer === null) timer = setTimeout(flush, FLUSH_MS);
}

/** Notes one host call: its name, outcome code and time in milliseconds. */
export function noteCall(command: string, outcome: string, started: number): void {
  if (!on || (outcome === 'ok' && QUIET_CALLS.has(command))) return;
  const ms = Math.round(performance.now() - started);
  note(outcome === 'ok' ? 'debug' : 'warn', 'page::bridge', `${command} ${outcome} ${ms} ms`);
}

/** The next batch, with a count of dropped records first. */
export function takeBatch(): PageRecord[] {
  const batch: PageRecord[] = [];
  if (dropped > 0) {
    batch.push({ level: 'warn', target: 'page', message: `${dropped} records dropped: over the limit` });
    dropped = 0;
  }
  const room = MAX_BATCH - batch.length;
  batch.push(...waiting.slice(0, room));
  waiting = waiting.slice(room);
  return batch;
}

async function flush(): Promise<void> {
  timer = null;
  if (!on) return;
  const records = takeBatch();
  if (records.length > 0) {
    // Best effort: a refused batch is dropped.
    await invoke('diagnostic_records', { records }).catch(() => {});
  }
  if (on && waiting.length > 0 && timer === null) timer = setTimeout(flush, FLUSH_MS);
}

let installed = false;

/**
 * Notes the page's uncaught errors (message and place) and rejections (a
 * short code only) while on. Call once.
 */
export function installPageDiagnostics(target: Window = window): void {
  if (installed) return;
  installed = true;
  target.addEventListener('error', (event) => {
    const where = event.filename ? ` at ${event.filename}:${event.lineno}:${event.colno}` : '';
    note('error', 'page::error', `${event.message}${where}`);
  });
  // A rejection's reason is the page's own free text (an Error's message can
  // hold a URL or an ID), so only a short code is kept, as for host calls.
  target.addEventListener('unhandledrejection', (event) => {
    note('error', 'page::rejection', outcomeCode(event.reason));
  });
}
