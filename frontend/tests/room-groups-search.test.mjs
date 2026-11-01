import assert from 'node:assert/strict';
import { test } from 'node:test';
import { groupRooms } from '../src/roomGroups.ts';
import { findSearchContext, SEARCH_CONTEXT_ITEMS, SEARCH_CONTEXT_PAGES } from '../src/searchNavigation.ts';

const room = (roomId) => ({ roomId, displayName: roomId, unreadCount: 0, encryption: 'encrypted', contact: false });
const message = (event_id) => ({ kind: 'message', event_id, sender: '@a:s', body: event_id, origin_server_ts: 0, reply_to: null });

test('GIVEN favourites and sections WHEN grouped THEN each room appears once in stable group order', () => {
  const rooms = ['a', 'b', 'c', 'd', 'e', 'f'].map(room);
  const preferences = [
    { roomId: 'a', favourite: false, section: 'Work' },
    { roomId: 'b', favourite: true, section: 'Work' },
    { roomId: 'c', favourite: false, section: 'Art' },
    { roomId: 'e', favourite: true, section: null },
    { roomId: 'f', favourite: false, section: 'Work' }
  ];
  const groups = groupRooms(rooms, preferences);
  assert.deepEqual(groups.map((group) => [group.name, group.rooms.map((item) => item.roomId)]), [
    ['Favourites', ['b', 'e']], ['Art', ['c']], ['Work', ['a', 'f']], ['Rooms', ['d']]
  ]);
  assert.equal(new Set(groups.flatMap((group) => group.rooms.map((item) => item.roomId))).size, rooms.length);
  assert.deepEqual(rooms.map((item) => item.roomId), ['a', 'b', 'c', 'd', 'e', 'f']);
});

test('GIVEN overlapping pages WHEN locating a cached hit THEN pages are deduplicated and the matching context is returned', async () => {
  const calls = [];
  const result = await findSearchContext('!r:s', '$target', [message('$new')], 'older', async (roomId, cursor) => {
    calls.push([roomId, cursor]);
    return { items: [message('$target'), message('$new')], nextCursor: 'before' };
  }, () => true);
  assert.equal(result.found, true);
  assert.deepEqual(result.items.map((item) => item.event_id), ['$target', '$new']);
  assert.deepEqual(calls, [['!r:s', 'older']]);
});

test('GIVEN stale room selection WHEN a history page arrives THEN no result is returned', async () => {
  let current = true;
  const result = await findSearchContext('!old:s', '$target', [], 'older', async () => {
    current = false;
    return { items: [message('$target')], nextCursor: null };
  }, () => current);
  assert.equal(result, null);
});

test('GIVEN missing context WHEN search navigation runs THEN reads and memory remain bounded', async () => {
  let calls = 0;
  const result = await findSearchContext('!r:s', '$missing', [], 'start', async () => {
    calls += 1;
    return { items: [message('$' + calls)], nextCursor: 'cursor-' + calls };
  }, () => true);
  assert.equal(calls, SEARCH_CONTEXT_PAGES);
  assert.equal(result.found, false);
  let bulkCalls = 0;
  const bulk = await findSearchContext('!r:s', '$missing', [], 'start', async () => {
    bulkCalls += 1;
    return { items: Array.from({ length: 200 }, (_, index) => message('$' + bulkCalls + '-' + index)), nextCursor: 'cursor-' + bulkCalls };
  }, () => true);
  assert.equal(bulk.items.length, 400);
  assert.ok(bulk.items.length < SEARCH_CONTEXT_ITEMS);
  assert.equal(bulkCalls, 3);
  assert.equal(bulk.cursor, 'cursor-2');
});

test('GIVEN a repeated cursor WHEN locating a result THEN the bounded reader stops', async () => {
  let calls = 0;
  const result = await findSearchContext('!r:s', '$missing', [], 'repeat', async () => {
    calls += 1;
    return { items: [message('$one')], nextCursor: 'repeat' };
  }, () => true);
  assert.equal(calls, 1);
  assert.equal(result.found, false);
});

test('GIVEN 1 to 99 recent events WHEN older pages overflow THEN all retained events and the retry cursor survive', async () => {
  for (let recentCount = 1; recentCount < 100; recentCount++) {
    const recent = Array.from({ length: recentCount }, (_, index) => message('$recent-' + index));
    const calls = [];
    const page = async (_roomId, cursor) => {
      calls.push(cursor);
      const index = Number(cursor.slice('cursor-'.length)) + 1;
      return {
        items: Array.from({ length: 100 }, (_, item) => message('$old-' + index + '-' + item)),
        nextCursor: 'cursor-' + index
      };
    };
    const result = await findSearchContext('!r:s', '$old-5-0', recent, 'cursor-0', page, () => true);
    assert.equal(result.items.length, recentCount + 400);
    assert.deepEqual(result.items.slice(-recentCount), recent);
    assert.equal(result.items[0].event_id, '$old-4-0');
    assert.equal(result.cursor, 'cursor-4');
    assert.equal(result.found, false, 'a hit in an unretained page is not found');
    assert.equal(calls.length, SEARCH_CONTEXT_PAGES);

    const retry = await findSearchContext('!r:s', '$old-5-0', result.items, result.cursor, page, () => true);
    assert.equal(calls.at(-1), 'cursor-4', 'retry requests the unretained page again');
    assert.equal(calls.length, SEARCH_CONTEXT_PAGES + 1);
    assert.deepEqual(retry, result);
  }
});

test('GIVEN overlapping pages near the item limit WHEN locating a hit THEN duplicates do not cause false overflow and gaps remain', async () => {
  const recent = Array.from({ length: 498 }, (_, index) => message('$recent-' + index));
  const gap = { kind: 'gap' };
  const result = await findSearchContext('!r:s', '$target', recent, 'older', async () => ({
    items: [gap, message('$target'), message('$target'), recent[0]], nextCursor: 'before'
  }), () => true);
  assert.equal(result.items.length, SEARCH_CONTEXT_ITEMS);
  assert.equal(result.found, true);
  assert.equal(result.items[0], gap);
  assert.equal(result.items.filter((item) => item.event_id === '$target').length, 1);
  assert.deepEqual(result.items.slice(2), recent);
  assert.equal(result.cursor, 'before');
});

test('GIVEN more than 500 initial events WHEN locating context THEN the newest page establishes the items and cursor', async () => {
  const initial = Array.from({ length: 501 }, (_, index) => message('$initial-' + index));
  const calls = [];
  const result = await findSearchContext('!r:s', '$target', initial, 'obsolete', async (_roomId, cursor) => {
    calls.push(cursor);
    return cursor === null
      ? { items: [message('$newest')], nextCursor: 'fresh' }
      : { items: [message('$target')], nextCursor: 'before-target' };
  }, () => true);
  assert.deepEqual(calls, [null, 'fresh']);
  assert.deepEqual(result.items.map((item) => item.event_id), ['$target', '$newest']);
  assert.equal(result.cursor, 'before-target');
  assert.equal(result.found, true);
});

test('GIVEN an oversized initial context WHEN the target is absent THEN the fresh-page request counts toward the read limit', async () => {
  const calls = [];
  const result = await findSearchContext('!r:s', '$missing', Array(501).fill(message('$initial')), 'obsolete', async (_roomId, cursor) => {
    calls.push(cursor);
    return { items: [message('$page-' + calls.length)], nextCursor: 'cursor-' + calls.length };
  }, () => true);
  assert.deepEqual(calls, [null, 'cursor-1', 'cursor-2', 'cursor-3', 'cursor-4']);
  assert.equal(calls.length, SEARCH_CONTEXT_PAGES);
  assert.equal(result.items.length, SEARCH_CONTEXT_PAGES);
  assert.equal(result.cursor, 'cursor-5');
});

test('GIVEN a stale room selection WHEN the newest context page arrives THEN no result is returned', async () => {
  let current = true;
  const calls = [];
  const result = await findSearchContext('!r:s', '$target', Array(501).fill(message('$initial')), 'obsolete', async (_roomId, cursor) => {
    calls.push(cursor);
    current = false;
    return { items: [message('$target')], nextCursor: 'fresh' };
  }, () => current);
  assert.deepEqual(calls, [null]);
  assert.equal(result, null);
});

test('GIVEN a page larger than the context limit WHEN supplied by the reader THEN it is rejected without slicing', async () => {
  const oversized = Array(SEARCH_CONTEXT_ITEMS + 1).fill(message('$event'));
  const page = async () => ({ items: oversized, nextCursor: 'invalid' });
  await assert.rejects(findSearchContext('!r:s', '$missing', [], 'older', page, () => true), RangeError);
  await assert.rejects(findSearchContext('!r:s', '$missing', oversized, 'obsolete', page, () => true), RangeError);
});
