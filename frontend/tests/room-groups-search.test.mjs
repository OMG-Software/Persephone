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
  assert.equal(bulk.items.length, SEARCH_CONTEXT_ITEMS);
  assert.equal(bulkCalls, 3);
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
