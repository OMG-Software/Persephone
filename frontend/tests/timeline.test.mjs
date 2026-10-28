import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mergeNewest, MAX_RETAINED_ITEMS } from '../src/timeline.ts';

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

test('notices count as known events when merging', () => {
  const notice = { kind: 'notice', event_id: '$n', sender: '@a:example.invalid', notice: 'undecryptable' };
  const merged = mergeNewest([...range(0, 3), notice], [...range(1, 3), notice, message(3)]);
  assert.deepEqual(
    merged.map((item) => (item.kind === 'gap' ? 'gap' : item.event_id)),
    ['$0', '$1', '$2', '$n', '$3']
  );
});

test('a shown notice is replaced when the same event comes back readable', () => {
  const notice = { kind: 'notice', event_id: '$1', sender: '@a:example.invalid', notice: 'undecryptable' };
  const merged = mergeNewest([message(0), notice], [message(0), message(1), message(2)]);
  assert.deepEqual(
    merged.map((item) => `${item.event_id}:${item.kind}`),
    ['$0:message', '$1:message', '$2:message']
  );
});


test('GIVEN a loaded message WHEN quoted THEN its first line shows, cut to the limit', async () => {
  const { quoteOf, MAX_QUOTE_CHARS } = await import('../src/timeline.ts');
  const items = [
    { kind: 'message', event_id: '$1', sender: '@a:s', body: '\n\nfirst line\nsecond', origin_server_ts: null },
    { kind: 'message', event_id: '$2', sender: '@b:s', body: 'x'.repeat(500), origin_server_ts: null },
    { kind: 'image', event_id: '$3', sender: '@c:s', name: 'cat.png', origin_server_ts: null },
    { kind: 'notice', event_id: '$4', sender: '@d:s', notice: 'undecryptable', origin_server_ts: null },
    { kind: 'gap', next_cursor: null }
  ];
  assert.deepEqual(quoteOf(items, '$1'), { sender: '@a:s', text: 'first line' });
  const long = quoteOf(items, '$2');
  assert.equal(Array.from(long.text).length, MAX_QUOTE_CHARS + 1);
  assert.ok(long.text.endsWith('…'));
  assert.deepEqual(quoteOf(items, '$3'), { sender: '@c:s', text: 'Image' });
  assert.deepEqual(quoteOf(items, '$4'), { sender: '@d:s', text: 'A message that is not shown' });
  assert.equal(quoteOf(items, '$missing'), null);
});

test('GIVEN edits WHEN applied THEN each message shows its newest edit, marked, and edits are not lines', async () => {
  const { applyRelations } = await import('../src/timeline.ts');
  const msg = (id, sender, body) => ({ kind: 'message', event_id: id, sender, body, origin_server_ts: null, reply_to: null });
  const edit = (id, sender, replaces, body) => ({ kind: 'edit', event_id: id, sender, replaces, body, origin_server_ts: null });
  const shown = applyRelations([
    msg('$1', '@a:s', 'first'),
    edit('$e1', '@a:s', '$1', 'second'),
    msg('$2', '@b:s', 'untouched'),
    edit('$e2', '@a:s', '$1', 'third'),
    // Not the message's sender here: ignored, even though the core checks.
    edit('$e3', '@c:s', '$2', 'forged')
  ]);
  assert.deepEqual(
    shown.map((item) => [item.event_id, item.body, item.edited === true]),
    [
      ['$1', 'third', true],
      ['$2', 'untouched', false]
    ]
  );
});

test('GIVEN a deleted edit WHEN applied THEN its text is not shown and the message keeps its previous text', async () => {
  const { applyRelations } = await import('../src/timeline.ts');
  const msg = (id, body) => ({ kind: 'message', event_id: id, sender: '@a:s', body, origin_server_ts: null, reply_to: null });
  const edit = (id, replaces, body) => ({ kind: 'edit', event_id: id, sender: '@a:s', replaces, body, origin_server_ts: null });
  const shown = applyRelations([
    msg('$1', 'first'),
    edit('$e1', '$1', 'second'),
    edit('$e2', '$1', 'regretted'),
    msg('$2', 'original'),
    edit('$e3', '$2', 'also regretted'),
    { kind: 'redaction', event_id: '$d1', sender: '@a:s', redacts: '$e2', origin_server_ts: null },
    { kind: 'redaction', event_id: '$d2', sender: '@a:s', redacts: '$e3', origin_server_ts: null }
  ]);
  assert.deepEqual(
    shown.map((item) => [item.event_id, item.kind, item.body, item.edited === true]),
    [
      ['$1', 'message', 'second', true],
      ['$2', 'message', 'original', false]
    ]
  );
  assert.ok(!JSON.stringify(shown).includes('regretted'));
});

test('GIVEN deletions WHEN applied THEN deleted messages show as deleted and edits do not bring them back', async () => {
  const { applyRelations } = await import('../src/timeline.ts');
  const shown = applyRelations([
    { kind: 'message', event_id: '$1', sender: '@a:s', body: 'secret', origin_server_ts: 5, reply_to: null },
    { kind: 'image', event_id: '$2', sender: '@b:s', name: 'x.png', origin_server_ts: null },
    { kind: 'message', event_id: '$3', sender: '@a:s', body: 'kept', origin_server_ts: null, reply_to: null },
    { kind: 'edit', event_id: '$e', sender: '@a:s', replaces: '$1', body: 'new secret', origin_server_ts: null },
    { kind: 'redaction', event_id: '$d1', sender: '@a:s', redacts: '$1', origin_server_ts: null },
    { kind: 'redaction', event_id: '$d2', sender: '@mod:s', redacts: '$2', origin_server_ts: null }
  ]);
  assert.deepEqual(
    shown.map((item) => [item.event_id, item.kind, item.kind === 'notice' ? item.notice : item.body]),
    [
      ['$1', 'notice', 'deleted'],
      ['$2', 'notice', 'deleted'],
      ['$3', 'message', 'kept']
    ]
  );
  assert.equal(shown[0].origin_server_ts, 5);
  assert.ok(!JSON.stringify(shown).includes('secret'));
});

test('GIVEN reactions WHEN applied THEN each key counts its senders once, mine is marked, and deleted ones drop out', async () => {
  const { applyRelations, MAX_REACTION_KEYS } = await import('../src/timeline.ts');
  const reaction = (id, sender, key, to = '$1') => ({
    kind: 'reaction', event_id: id, sender, reacts_to: to, key, origin_server_ts: null
  });
  const items = [
    { kind: 'message', event_id: '$1', sender: '@a:s', body: 'hi', origin_server_ts: null, reply_to: null },
    reaction('$r1', '@b:s', '👍'),
    reaction('$r2', '@c:s', '👍'),
    reaction('$r3', '@b:s', '👍'),
    reaction('$r4', '@me:s', '🎉'),
    reaction('$r5', '@c:s', '😂'),
    { kind: 'redaction', event_id: '$d', sender: '@c:s', redacts: '$r5', origin_server_ts: null }
  ];
  const [shown] = applyRelations(items, '@me:s');
  assert.deepEqual(shown.reactions, [
    { key: '👍', count: 2, mine: null },
    { key: '🎉', count: 1, mine: '$r4' }
  ]);
  const many = Array.from({ length: 20 }, (_, n) => reaction(`$k${n}`, '@b:s', `k${n}`));
  const [capped] = applyRelations([items[0], ...many]);
  assert.equal(capped.reactions.length, MAX_REACTION_KEYS);
  assert.equal(applyRelations(items).length, 1, 'reactions are not lines of their own');
});

test('GIVEN unread messages WHEN a room opens THEN the line goes before the oldest unread one from someone else', async () => {
  const { firstUnread, newestEvent } = await import('../src/timeline.ts');
  const msg = (id, sender) => ({ kind: 'message', event_id: id, sender, body: id, origin_server_ts: null, reply_to: null });
  const items = [
    msg('$1', '@b:s'),
    msg('$2', '@b:s'),
    { kind: 'state', event_id: '$s', sender: '@b:s', origin_server_ts: null, change: { type: 'encryption' } },
    msg('$3', '@me:s'),
    msg('$4', '@b:s')
  ];
  assert.equal(firstUnread(items, 2, '@me:s'), '$2', 'own messages and state changes are not counted');
  assert.equal(firstUnread(items, 1, '@me:s'), '$4');
  assert.equal(firstUnread(items, 99, '@me:s'), '$1', 'more than loaded: the oldest loaded');
  assert.equal(firstUnread(items, 0, '@me:s'), null);
  assert.equal(newestEvent([...items, { kind: 'gap', next_cursor: null }]), '$4');
  assert.equal(newestEvent([]), null);
});
