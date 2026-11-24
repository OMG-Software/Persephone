import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import { mainMenuItems, menuTarget, roomMenuItems } from '../src/menu.ts';
import {
  SIDEBAR_DEFAULT,
  SIDEBAR_MAX,
  SIDEBAR_MIN,
  SIDEBAR_STEP,
  MEMBERS,
  MEMBERS_DEFAULT,
  MEMBERS_MAX,
  MEMBERS_MIN,
  ROOM_MIN,
  clampSidebar,
  clampWidth,
  draggedSidebar,
  draggedWidth,
  keyedSidebar,
  keyedWidth,
  membersFit,
  membersLimit,
  sidebarLimit
} from '../src/layout.ts';
import { memberMark, memberRows } from '../src/members.ts';

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

const chosen = [];
const actions = Object.fromEntries(
  ['explore', 'groups', 'search', 'drafts', 'ignored', 'health', 'settings', 'lock', 'exit'].map((name) => [name, () => chosen.push(name)])
);
const mainMenu = (state) =>
  mainMenuItems({ signedIn: true, canLock: true, switching: false, confirmingSignOut: false, ...state }, actions);

test('GIVEN a signed-in account WHEN the main menu opens THEN Settings and Lock Persephone follow the account tools, and Exit ends it', () => {
  const items = mainMenu({});
  assert.deepEqual(items.map((item) => item.label), [
    'Explore public rooms', 'Room groups', 'Search cached messages', 'Saved drafts', 'Ignored users', 'Connection health',
    'Settings', 'Lock Persephone', 'Exit Persephone'
  ]);
  assert.equal(items[6].separated, true, 'a line sets Settings and Lock apart');
  assert.equal(items.at(-1).separated, true, 'a line sets Exit apart');
  chosen.length = 0;
  for (const item of items.slice(-3)) item.onselect();
  assert.deepEqual(chosen, ['settings', 'lock', 'exit']);
  assert.equal(items.at(-2).disabled, false);
});

test('GIVEN no account WHEN the main menu opens THEN Settings and Exit are still there and nothing else is', () => {
  const items = mainMenu({ signedIn: false, canLock: false });
  assert.deepEqual(items.map((item) => [item.label, Boolean(item.separated)]), [['Settings', false], ['Exit Persephone', true]]);
});

test('GIVEN the session is changing or the sign-out question is open WHEN the main menu opens THEN Lock is disabled', () => {
  const working = mainMenu({ switching: true }).at(-2);
  assert.deepEqual([working.label, working.disabled], ['Working…', true]);
  const asking = mainMenu({ confirmingSignOut: true }).at(-2);
  assert.deepEqual([asking.label, asking.disabled], ['Lock Persephone', true]);
});

const roomChosen = [];
const roomActions = Object.fromEntries(
  ['invite', 'jump', 'pins', 'attachments', 'members', 'group', 'settings', 'leave'].map((name) => [name, () => roomChosen.push(name)])
);
const roomMenu = (state) =>
  roomMenuItems({ direct: false, membersRoom: true, membersShown: true, leaving: false, ...state }, roomActions);

test('GIVEN a joined room WHEN its menu opens THEN Leave room is the last command, set apart, and asks to leave', () => {
  const items = roomMenu({});
  assert.deepEqual(items.map((item) => item.label), [
    'Invite people', 'Jump to date', 'Pinned messages', 'Attachments', 'Hide members', 'Room group', 'Room settings', 'Leave room'
  ]);
  const leave = items.at(-1);
  assert.equal(leave.separated, true, 'a line sets Leave room apart from Room settings');
  assert.equal(leave.disabled, false);
  roomChosen.length = 0;
  leave.onselect();
  assert.deepEqual(roomChosen, ['leave'], 'choosing it only asks; it does not leave by itself');
});

test('GIVEN a direct conversation or a leave being sent WHEN the room menu opens THEN Invite people is absent and Leave room is disabled', () => {
  const direct = roomMenu({ direct: true });
  assert.equal(direct.some((item) => item.label === 'Invite people'), false);
  assert.equal(direct.at(-1).label, 'Leave room', 'a direct conversation can be left too');
  assert.equal(roomMenu({ leaving: true }).at(-1).disabled, true);
});

test('GIVEN a narrow window WHEN the room menu opens THEN the member command is disabled and does nothing', () => {
  const members = roomMenu({ membersRoom: false }).find((item) => item.label === 'Members need a wider window');
  assert.equal(members.disabled, true);
  roomChosen.length = 0;
  members.onselect();
  assert.deepEqual(roomChosen, []);
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

test('GIVEN a narrow window WHEN the room list limit is found THEN the room keeps its minimum width, down to the list minimum', () => {
  assert.equal(sidebarLimit(1600), SIDEBAR_MAX);
  // The native minimum window: 640 pixels leaves the room 320.
  assert.equal(sidebarLimit(640), 640 - ROOM_MIN);
  assert.equal(sidebarLimit(400), SIDEBAR_MIN, 'never below the list minimum');
  assert.equal(sidebarLimit(Number.NaN), SIDEBAR_MAX);
});

test('GIVEN a lower limit WHEN the width is clamped, keyed or dragged THEN it never exceeds that limit', () => {
  assert.equal(clampSidebar(450, 320), 320);
  assert.equal(clampSidebar(Number.NaN, 250), 250, 'the default is capped too');
  assert.equal(keyedSidebar(310, 'ArrowRight', 320), 320);
  assert.equal(keyedSidebar(300, 'End', 320), 320);
  assert.equal(draggedSidebar(292, 0, 400, 1, 320), 320);
});

test('GIVEN the window and room list widths WHEN the member panel is placed THEN it shows only when the room keeps its minimum', () => {
  // The panel fits when its narrowest width does.
  const needed = SIDEBAR_DEFAULT + MEMBERS_MIN + ROOM_MIN;
  assert.equal(membersFit(needed, SIDEBAR_DEFAULT), true);
  assert.equal(membersFit(needed - 1, SIDEBAR_DEFAULT), false);
  assert.equal(membersFit(640, SIDEBAR_DEFAULT), false, 'not in the native minimum window');
  assert.equal(membersFit(Number.NaN, SIDEBAR_DEFAULT), false);
});

test('GIVEN the member panel handle WHEN keys are pressed THEN the left arrow widens it and Home and End go to the limits', () => {
  const key = (width, name, max) => keyedWidth(MEMBERS, width, name, max, 'ArrowLeft');
  assert.equal(key(248, 'ArrowLeft'), 248 + MEMBERS.step, 'the handle moves left, away from the panel');
  assert.equal(key(248, 'ArrowRight'), 248 - MEMBERS.step);
  assert.equal(key(MEMBERS_MIN, 'ArrowRight'), MEMBERS_MIN, 'never below the minimum');
  assert.equal(key(MEMBERS_MAX, 'ArrowLeft'), MEMBERS_MAX, 'never above the maximum');
  assert.equal(key(300, 'Home'), MEMBERS_MIN);
  assert.equal(key(300, 'End'), MEMBERS_MAX);
  assert.equal(key(300, 'End', 260), 260, 'End stops at the limit the room leaves');
  assert.equal(key(300, 'Enter'), null, 'a key that does not resize');
});

test('GIVEN a drag of the member panel handle WHEN the pointer moves left THEN the panel widens by the CSS distance, within its range', () => {
  const drag = (x, ratio = 1, max) => draggedWidth(MEMBERS, MEMBERS_DEFAULT, 900, x, ratio, max, -1);
  assert.equal(drag(800), MEMBERS_DEFAULT + 100);
  assert.equal(drag(920), MEMBERS_DEFAULT - 20, 'moving right narrows it');
  // At 125% zoom, 100 pointer pixels are 80 CSS pixels.
  assert.equal(drag(800, 1.25), MEMBERS_DEFAULT + 80);
  assert.equal(drag(0), MEMBERS_MAX, 'clamped to the maximum');
  assert.equal(drag(2000), MEMBERS_MIN, 'clamped to the minimum');
  assert.equal(drag(0, 1, 300), 300, 'clamped to the limit the room leaves');
  assert.equal(clampWidth(MEMBERS, Number.NaN), MEMBERS_DEFAULT, 'a width that is not a number gives the default');
});

test('GIVEN the window and room list widths WHEN the member panel limit is found THEN the room keeps its minimum, down to the panel minimum', () => {
  assert.equal(membersLimit(2000, SIDEBAR_DEFAULT), MEMBERS_MAX);
  assert.equal(membersLimit(SIDEBAR_DEFAULT + ROOM_MIN + 300, SIDEBAR_DEFAULT), 300);
  assert.equal(membersLimit(640, SIDEBAR_DEFAULT), MEMBERS_MIN, 'never below the panel minimum');
  assert.equal(membersLimit(Number.NaN, SIDEBAR_DEFAULT), MEMBERS_MAX);
});

test('GIVEN the stylesheet WHEN the timeline and tooltips are laid out THEN nothing can scroll the timeline sideways', async () => {
  const css = (await readFile(new URL('../src/style.css', import.meta.url), 'utf8')).replace(/\/\*[^]*?\*\//g, '');
  // The declarations of the one rule whose selector list is exactly `selector`.
  const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map(([, selectors, body]) => ({
    selectors: selectors.split(',').map((one) => one.trim()).join(', '),
    body
  }));
  const rule = (selector) => {
    const found = rules.filter((one) => one.selectors === selector);
    assert.equal(found.length, 1, selector);
    return found[0].body;
  };
  assert.match(rule('.scroller'), /overflow-x:\s*hidden/);
  // A hidden tip takes no space: `visibility: hidden` would still widen the
  // scroll area when a tip sits near the edge.
  assert.match(rule('.action-tip'), /display:\s*none/);
  assert.doesNotMatch(rule('.action-tip'), /visibility/);
  // Tips of the message actions end at the button's right edge.
  assert.match(
    rule('.message-actions .tip-above > .action-tip, .message-actions .tip-below > .action-tip'),
    /right:\s*0/
  );
  // The main menu's tip, at the window's left edge, starts at the button.
  assert.match(rule('.sidebar-actions > .tip-below > .action-tip'), /left:\s*0/);
});

const reader = (userId, trust, own = false, presence = null) => ({ userId, trust, receiving: 1, refused: 0, own, presence });
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

test('GIVEN readers with presence WHEN joined THEN each row keeps a known status, and anything else is not known', () => {
  const rows = memberRows(
    [
      reader('@a:s', 'verified', false, 'online'),
      reader('@b:s', 'verified', false, 'unavailable'),
      reader('@c:s', 'verified', false, 'offline'),
      reader('@d:s', 'verified', false, 'busy'),
      { userId: '@e:s', trust: 'verified', receiving: 1, refused: 0, own: false }
    ],
    null,
    null
  );
  assert.deepEqual(rows.map((row) => row.presence), ['online', 'unavailable', 'offline', null, null]);
});
