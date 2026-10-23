import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let time;

before(async () => {
  vite = await createServer({
    configFile: './vite.config.js',
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  time = await vite.ssrLoadModule('/src/time.ts');
});

after(async () => {
  await vite?.close();
});

const utc = { locale: 'en-GB', timeZone: 'UTC' };
// 2026-01-01 09:00 UTC, the synthetic history's start.
const start = 1_767_258_000_000;
const hour = 60 * 60 * 1000;
const day = 24 * hour;

test('GIVEN untrusted times WHEN read as dates THEN only showable ones are kept', () => {
  const { toDate } = time;
  assert.equal(toDate(start)?.toISOString(), '2026-01-01T09:00:00.000Z');
  assert.equal(toDate(0)?.toISOString(), '1970-01-01T00:00:00.000Z');
  // Past the end of JavaScript's dates, though JavaScript holds the number.
  assert.equal(toDate(Number.MAX_SAFE_INTEGER), null);
  for (const bad of [null, undefined, -1, 1.5, Number.NaN, Infinity, '1', 2 ** 53]) {
    assert.equal(toDate(bad), null, String(bad));
  }
});

test('times show as the time of day, with the full date for a tooltip', () => {
  const date = time.toDate(start + 5 * 60 * 1000 + 7);
  assert.equal(time.clockTime(date, utc), '09:05');
  assert.match(time.fullTime(date, utc), /Thursday,? 1 January 2026.*09:05:00/);
  // The reader's time zone decides the day and the time.
  assert.equal(time.clockTime(date, { locale: 'en-GB', timeZone: 'Asia/Tokyo' }), '18:05');
  assert.equal(time.dayKey(time.toDate(start + 16 * hour), utc), '2026-01-02');
  assert.equal(time.dayKey(time.toDate(start + 16 * hour), { timeZone: 'America/Los_Angeles' }), '2026-01-01');
});

test('GIVEN messages over several days WHEN shown THEN a divider marks each new day', () => {
  const now = time.toDate(start + 2 * day);
  const dividers = time.dayDividers(
    [start - 400 * day, start, start + hour, null, start + day, start + 2 * day - hour, start + 2 * day],
    now,
    utc
  );
  assert.deepEqual(dividers, [
    'Wednesday, 27 November 2024',
    'Thursday 1 January',
    null,
    null,
    'Yesterday',
    'Today',
    null
  ]);
  // Items without a time get no divider and do not end a day.
  assert.deepEqual(time.dayDividers([start, null, start], now, utc), ['Thursday 1 January', null, null]);
});

test('GIVEN a daylight-saving change WHEN labelling days THEN yesterday is still yesterday', () => {
  // Europe/London moves its clocks back on 2026-10-25, a 25-hour day.
  const london = { locale: 'en-GB', timeZone: 'Europe/London' };
  const now = new Date('2026-10-26T00:30:00Z');
  assert.equal(time.dayLabel(new Date('2026-10-25T00:10:00Z'), now, london), 'Yesterday');
  assert.equal(time.dayLabel(new Date('2026-10-24T22:50:00Z'), now, london), 'Saturday 24 October');
  assert.equal(time.dayLabel(new Date('2026-10-26T00:00:00Z'), now, london), 'Today');
});
