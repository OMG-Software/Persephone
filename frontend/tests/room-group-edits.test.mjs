import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  MAX_GROUP_ENTRIES, MAX_GROUP_NAME_BYTES, MAX_GROUPS, afterWrites, applyWrites, capacityProblem,
  groupNameProblem, groupNames, groupWrites, removeGroupWrites
} from '../src/roomGroupEdits.ts';

const pref = (roomId, section = null, favourite = false) => ({ roomId, favourite, section });

test('GIVEN preferences WHEN group names are read THEN each name appears once in exact label order', () => {
  const names = groupNames([pref('a', 'Work'), pref('b', 'art'), pref('c', 'Work'), pref('d', null, true), pref('e', 'Art')]);
  assert.deepEqual(names, ['Art', 'Work', 'art']);
});

test('GIVEN group names WHEN checked THEN blank, oversized, control-bearing and reserved names are refused', () => {
  assert.equal(groupNameProblem('Work'), null);
  assert.equal(groupNameProblem('é'.repeat(MAX_GROUP_NAME_BYTES / 2)), null);
  assert.match(groupNameProblem(''), /Enter a group name/);
  assert.match(groupNameProblem('é'.repeat(MAX_GROUP_NAME_BYTES / 2 + 1)), /exceeds 64 UTF-8 bytes/);
  for (const name of ['a\nb', 'a\u0007', 'a\u009f']) assert.match(groupNameProblem(name), /control characters/, JSON.stringify(name));
  for (const name of ['Favourites', 'rooms', 'ROOMS', 'favourites']) assert.match(groupNameProblem(name), /already headings/, name);
});

test('GIVEN a group edit WHEN writes are planned THEN joined rooms join or leave, favourites stay and stale entries are untouched', () => {
  const preferences = [
    pref('!a:s', 'Work', true),
    pref('!b:s', 'Work'),
    pref('!c:s', 'Home', true),
    pref('!stale:s', 'Work')
  ];
  const joined = ['!a:s', '!b:s', '!c:s', '!d:s'];
  const writes = groupWrites(preferences, 'Work', ['!a:s', '!c:s', '!d:s'], joined);
  assert.deepEqual(writes, [pref('!b:s', null), pref('!c:s', 'Work', true), pref('!d:s', 'Work')]);
  assert.equal(writes.some((write) => write.roomId === '!stale:s'), false);
  assert.deepEqual(groupWrites(preferences, 'Work', ['!a:s', '!b:s'], joined), [], 'no change means no writes');
});

test('GIVEN a group with joined and stale rooms WHEN removed THEN every entry loses the group and keeps its favourite', () => {
  const preferences = [pref('!a:s', 'Work', true), pref('!b:s', 'Work'), pref('!c:s', 'Home'), pref('!stale:s', 'Work')];
  const writes = removeGroupWrites(preferences, 'Work');
  assert.deepEqual(writes, [pref('!a:s', null, true), pref('!b:s', null), pref('!stale:s', null)]);
  assert.deepEqual(afterWrites(preferences, writes), [pref('!a:s', null, true), pref('!c:s', 'Home')],
    'an entry with no favourite and no group is dropped, as the core does');
});

test('GIVEN the core limits WHEN writes would exceed them THEN a capacity problem is reported', () => {
  const full = Array.from({ length: MAX_GROUPS }, (_, n) => pref(`!r${n}:s`, `G${n}`));
  assert.equal(capacityProblem(full, [pref('!r0:s', 'G1')]), null, 'moving into an existing group fits');
  assert.match(capacityProblem(full, [pref('!x:s', 'New')]), /at most 16 groups/);
  assert.equal(capacityProblem(full, [pref('!r0:s', 'New')]), null, 'a group emptied by the same write frees its place');
  const many = Array.from({ length: MAX_GROUP_ENTRIES }, (_, n) => pref(`!m${n}:s`, 'Work'));
  assert.equal(capacityProblem(many, [pref('!m0:s', 'Home')]), null);
  assert.match(capacityProblem(many, [pref('!new:s', 'Work')]), /at most 100 rooms/);
  assert.equal(capacityProblem(many, [pref('!m0:s', null), pref('!new:s', 'Work')]), null, 'a cleared entry frees its slot');
});

test('GIVEN writes WHEN one fails THEN saving stops there and only saved writes are reported', async () => {
  const writes = [pref('!a:s', 'W'), pref('!b:s', 'W'), pref('!c:s', 'W')];
  const sent = [];
  const shown = [];
  const result = await applyWrites(writes, async (write) => {
    sent.push(write.roomId);
    if (write.roomId === '!b:s') throw new Error('limit_exceeded');
  }, (write) => shown.push(write.roomId));
  assert.deepEqual(result, { saved: 1, failed: true });
  assert.deepEqual(sent, ['!a:s', '!b:s'], 'nothing is sent after the failure');
  assert.deepEqual(shown, ['!a:s']);
});

test('GIVEN writes WHEN all are saved THEN they are sent one at a time in order', async () => {
  const writes = [pref('!a:s', 'W'), pref('!b:s', null)];
  let running = 0;
  const order = [];
  const result = await applyWrites(writes, async (write) => {
    running += 1;
    assert.equal(running, 1, 'one write at a time');
    await new Promise((resolve) => setTimeout(resolve, 1));
    order.push(write.roomId);
    running -= 1;
  }, () => {});
  assert.deepEqual(result, { saved: 2, failed: false });
  assert.deepEqual(order, ['!a:s', '!b:s']);
});
