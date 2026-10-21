import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let mergeNewest;
let MAX_RETAINED_ITEMS;

before(async () => {
  vite = await createServer({
    configFile: './vite.config.js',
    server: { middlewareMode: true, hmr: false },
    appType: 'custom'
  });
  ({ mergeNewest, MAX_RETAINED_ITEMS } = await vite.ssrLoadModule('/src/timeline.ts'));
});

after(async () => {
  await vite?.close();
});

const message = (n) => ({ kind: 'message', event_id: `$${n}`, sender: '@a:example.invalid', body: `m${n}` });
const range = (from, to) => Array.from({ length: to - from }, (_, i) => message(from + i));
const ids = (items) => items.map((item) => (item.kind === 'message' ? item.event_id : 'gap'));

test('new messages after the newest shown one are appended once', () => {
  const merged = mergeNewest(range(0, 10), range(5, 13));
  assert.deepEqual(ids(merged), ids(range(0, 13)));
  // Nothing new: unchanged.
  assert.deepEqual(ids(mergeNewest(range(0, 10), range(0, 10))), ids(range(0, 10)));
});

test('an empty timeline takes the whole page', () => {
  assert.deepEqual(ids(mergeNewest([], range(0, 3))), ids(range(0, 3)));
});

test('when more than a page arrived, a gap marks the missing messages', () => {
  const merged = mergeNewest(range(0, 10), range(200, 300));
  assert.deepEqual(ids(merged), [...ids(range(0, 10)), 'gap', ...ids(range(200, 300))]);
});

test('the timeline is capped, with one leading gap in place of the oldest', () => {
  let items = range(0, 100);
  for (let start = 100; start < 2000; start += 50) {
    items = mergeNewest(items, range(start - 10, start + 50));
    assert.ok(items.length <= MAX_RETAINED_ITEMS, `${items.length} items`);
  }
  assert.equal(items.length, MAX_RETAINED_ITEMS);
  assert.equal(items[0].kind, 'gap');
  assert.equal(items.filter((item) => item.kind === 'gap').length, 1);
  assert.equal(items.at(-1).event_id, '$1999');
  assert.equal(items[1].event_id, `$${2000 - (MAX_RETAINED_ITEMS - 1)}`);
});
