import assert from 'node:assert/strict';
import { test } from 'node:test';
import { menuTarget } from '../src/menu.ts';
import {
  SIDEBAR_DEFAULT,
  SIDEBAR_MAX,
  SIDEBAR_MIN,
  SIDEBAR_STEP,
  clampSidebar,
  draggedSidebar,
  keyedSidebar
} from '../src/layout.ts';
import { directRoomOf, memberMark, memberRows } from '../src/members.ts';

test('GIVEN an open menu WHEN arrow keys move focus THEN focus wraps and skips disabled commands', () => {
  const enabled = [true, false, true, true];
  assert.equal(menuTarget('ArrowDown', 0, enabled), 2, 'skips the disabled command');
  assert.equal(menuTarget('ArrowDown', 3, enabled), 0, 'wraps to the first');
  assert.equal(menuTarget('ArrowUp', 0, enabled), 3, 'wraps to the last');
  assert.equal(menuTarget('ArrowUp', 2, enabled), 0, 'skips the disabled command upwards');
  assert.equal(menuTarget('Home', 3, enabled), 0);
  assert.equal(menuTarget('End', 0, [true, true, false]), 1, 'the last enabled command');
  assert.equal(menuTarget('ArrowDown', -1, enabled), 0, 'from no focus, the first');
  assert.equal(menuTarget('ArrowUp', -1, enabled), 3, 'from no focus, the last');
});

test('GIVEN an open menu WHEN a key does not move focus or nothing can take it THEN no target is given', () => {
  assert.equal(menuTarget('a', 0, [true, true]), null);
  assert.equal(menuTarget('Enter', 0, [true, true]), null);
  assert.equal(menuTarget('ArrowDown', 0, [false, false]), null);
  assert.equal(menuTarget('Home', 0, []), null);
});

test('GIVEN a room list width WHEN it is clamped THEN it stays in range and a value that is not a number gives the default', () => {
  assert.equal(clampSidebar(10), SIDEBAR_MIN);
  assert.equal(clampSidebar(10_000), SIDEBAR_MAX);
  assert.equal(clampSidebar(300.4), 300);
  assert.equal(clampSidebar(Number.NaN), SIDEBAR_DEFAULT);
  assert.equal(clampSidebar(Number.POSITIVE_INFINITY), SIDEBAR_DEFAULT);
});

test('GIVEN the focused resize handle WHEN keys are pressed THEN arrows step the width and Home and End go to the limits', () => {
  assert.equal(keyedSidebar(300, 'ArrowRight'), 300 + SIDEBAR_STEP);
  assert.equal(keyedSidebar(300, 'ArrowLeft'), 300 - SIDEBAR_STEP);
  assert.equal(keyedSidebar(SIDEBAR_MIN, 'ArrowLeft'), SIDEBAR_MIN, 'never below the minimum');
  assert.equal(keyedSidebar(SIDEBAR_MAX, 'ArrowRight'), SIDEBAR_MAX, 'never above the maximum');
  assert.equal(keyedSidebar(300, 'Home'), SIDEBAR_MIN);
  assert.equal(keyedSidebar(300, 'End'), SIDEBAR_MAX);
  assert.equal(keyedSidebar(300, 'ArrowUp'), null, 'a key that does not resize');
});

test('GIVEN a drag of the resize handle WHEN the interface is zoomed THEN the edge follows the pointer', () => {
  assert.equal(draggedSidebar(292, 100, 150, 1), 342);
  // At 125% zoom, 50 pointer pixels are 40 CSS pixels.
  assert.equal(draggedSidebar(292, 100, 150, 1.25), 332);
  assert.equal(draggedSidebar(292, 100, 150, 0), 342, 'an invalid ratio counts as 1');
  assert.equal(draggedSidebar(292, 100, 150, Number.NaN), 342);
  assert.equal(draggedSidebar(292, 500, 0, 1), SIDEBAR_MIN, 'clamped');
});

const reader = (userId, trust, own = false) => ({ userId, trust, receiving: 1, refused: 0, own });
const member = (userId, displayName, membership = 'join') => ({
  userId, displayName, membership, level: 0, own: false, canKick: false, canBan: false, canUnban: false, canChangeLevel: false
});

test('GIVEN readers and members WHEN joined THEN names and invitations come from the member list, own first, then by name', () => {
  const rows = memberRows(
    [reader('@zed:s', 'verified'), reader('@me:s', 'verified', true), reader('@amy:s', 'unverified'), reader('@bob:s', 'no_identity')],
    [member('@zed:s', 'Aaron'), member('@bob:s', null, 'invite')],
    null
  );
  assert.deepEqual(rows.map((row) => row.userId), ['@me:s', '@zed:s', '@amy:s', '@bob:s']);
  assert.equal(rows[1].name, 'Aaron');
  assert.equal(rows[2].name, null, 'no member entry: the ID only');
  assert.equal(rows[3].invited, true);
  assert.equal(rows[1].invited, false);
});

test('GIVEN no member list WHEN joined THEN the readers still show by ID', () => {
  const rows = memberRows([reader('@b:s', 'unverified'), reader('@a:s', 'verified')], null, null);
  assert.deepEqual(rows.map((row) => [row.userId, row.name]), [['@a:s', null], ['@b:s', null]]);
});

test('GIVEN a direct-message contact status WHEN joined THEN it replaces only that reader\'s trust', () => {
  const rows = memberRows(
    [reader('@bob:s', 'no_identity'), reader('@eve:s', 'no_identity')],
    null,
    { roomId: '!dm:s', userId: '@bob:s', trust: 'unverified' }
  );
  assert.equal(rows.find((row) => row.userId === '@bob:s').trust, 'unverified');
  assert.equal(rows.find((row) => row.userId === '@eve:s').trust, 'no_identity');
});

test('GIVEN a member\'s trust WHEN marked THEN only unverified offers Verify, and an unknown level never reads as verified', () => {
  const mark = (trust, own = false) => memberMark({ userId: '@a:s', name: null, trust, own, invited: false });
  assert.equal(mark('verified'), 'verified');
  assert.equal(mark('unverified'), 'verify');
  assert.equal(mark('changed'), 'changed');
  assert.equal(mark('verified_changed'), 'changed');
  assert.equal(mark('no_identity'), 'no_identity');
  assert.equal(mark('unknown'), 'unknown');
  assert.equal(mark('trusted_anyway'), 'unknown');
  assert.equal(mark('unverified', true), 'own', 'this account is never offered for verification');
});

test('GIVEN the room list WHEN a member\'s direct room is looked up THEN only an exact contact match is used', () => {
  const rooms = [
    { roomId: '!group:s', displayName: 'Group', unreadCount: 0, encryption: 'encrypted', contact: null },
    { roomId: '!dm:s', displayName: 'Bob', unreadCount: 0, encryption: 'encrypted', contact: '@bob:s' }
  ];
  assert.equal(directRoomOf(rooms, '@bob:s'), '!dm:s');
  assert.equal(directRoomOf(rooms, '@BOB:s'), null);
  assert.equal(directRoomOf(rooms, '@eve:s'), null);
  assert.equal(directRoomOf([], '@bob:s'), null);
});
