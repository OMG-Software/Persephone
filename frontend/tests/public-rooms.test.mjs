import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';
import {
  MAX_DIRECTORY_FILTER_BYTES, MAX_LISTED_ROOMS, MAX_SERVER_NAME_BYTES, addPage, filterProblem, joinFailure,
  listFailure, memberText, ownServer, roomLabel, serverProblem
} from '../src/publicRooms.ts';

let vite;
let render;
let PublicRooms;
let Sidebar;

before(async () => {
  // A cache of its own: test files run in parallel (see views.test.mjs).
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/public-rooms',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: PublicRooms } = await vite.ssrLoadModule('/src/PublicRooms.svelte'));
  ({ default: Sidebar } = await vite.ssrLoadModule('/src/Sidebar.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

const hostile = '<img src=x onerror=alert(1)>';
const shown = (html) => html.replace(/<!--.*?-->/g, '');
const room = (roomId, extra = {}) => ({
  roomId,
  name: null,
  alias: null,
  topic: null,
  members: 1,
  worldReadable: false,
  guestsCanJoin: false,
  joinable: true,
  joined: false,
  ...extra
});

test('GIVEN typed server names WHEN checked THEN empty means the own homeserver and only server names pass', () => {
  for (const valid of ['', '  ', 'matrix.org', ' matrix.org ', 'example.org:8448', '127.0.0.1', '[::1]:8448']) {
    assert.equal(serverProblem(valid), null, valid);
  }
  for (const url of ['https://matrix.org', 'matrix.org/', 'matrix.org/_matrix']) {
    assert.match(serverProblem(url), /only the server name/, url);
  }
  for (const invalid of ['@al:matrix.org', 'exa mple.org', 'exämple.org', 'a:b:c', 'a'.repeat(MAX_SERVER_NAME_BYTES + 1)]) {
    assert.match(serverProblem(invalid), /not a server name/, invalid);
  }
});

test('GIVEN typed search text WHEN checked THEN oversized or control-bearing text is refused', () => {
  assert.equal(filterProblem(''), null);
  assert.equal(filterProblem('rust'), null);
  assert.equal(filterProblem('é'.repeat(MAX_DIRECTORY_FILTER_BYTES / 2)), null);
  assert.match(filterProblem('é'.repeat(MAX_DIRECTORY_FILTER_BYTES / 2 + 1)), /exceeds 256 UTF-8 bytes/);
  assert.match(filterProblem('a\u0007b'), /control characters/);
});

test('GIVEN user IDs WHEN their server is read THEN the part after the first colon is the server', () => {
  assert.equal(ownServer('@al:example.org'), 'example.org');
  assert.equal(ownServer('@al:example.org:8448'), 'example.org:8448');
  for (const none of [null, '', '@al', '@al:', ':example.org']) assert.equal(ownServer(none), null, String(none));
});

test('GIVEN directory pages WHEN added to a listing THEN a room shows once, the newest token is kept, and the listing stops at its limit', () => {
  const first = addPage(null, { rooms: [room('!a:s'), room('!b:s')], next: 'p2', total: 5 });
  assert.deepEqual(first.rooms.map((entry) => entry.roomId), ['!a:s', '!b:s']);
  assert.equal(first.next, 'p2');
  assert.equal(first.total, 5);
  const second = addPage(first, { rooms: [room('!b:s', { name: 'later' }), room('!c:s')], next: null, total: null });
  assert.deepEqual(second.rooms.map((entry) => entry.roomId), ['!a:s', '!b:s', '!c:s']);
  assert.equal(second.rooms[1].name, null, 'a room listed again keeps its first place and data');
  assert.equal(second.next, null);
  // Past the limit: no more rooms, and no next page is offered.
  const many = Array.from({ length: MAX_LISTED_ROOMS + 5 }, (_, n) => room(`!r${n}:s`));
  const full = addPage(null, { rooms: many, next: 'p9', total: 1000 });
  assert.equal(full.rooms.length, MAX_LISTED_ROOMS);
  assert.equal(full.next, null);
});

test('GIVEN a directory WHEN it answers with the token it was asked with THEN no next page is offered', () => {
  const first = addPage(null, { rooms: [room('!a:s')], next: 'p2', total: null }, null);
  assert.equal(first.next, 'p2');
  const repeated = addPage(first, { rooms: [room('!a:s')], next: 'p2', total: null }, 'p2');
  assert.equal(repeated.next, null, 'asking again would bring nothing new');
  const advanced = addPage(first, { rooms: [room('!b:s')], next: 'p3', total: null }, 'p2');
  assert.equal(advanced.next, 'p3');
  // A loop of pages: p3 answers with p2 again.
  assert.equal(addPage(advanced, { rooms: [room('!c:s')], next: 'p2', total: null }, 'p3').next, null);
  assert.deepEqual(addPage(advanced, { rooms: [], next: 'p4', total: null }, 'p3').asked, ['p2', 'p3']);
});

test('GIVEN listed rooms WHEN labelled THEN the name, address, then ID is shown, and member counts read naturally', () => {
  assert.equal(roomLabel(room('!r:s', { name: 'Lobby', alias: '#lobby:s' })), 'Lobby');
  assert.equal(roomLabel(room('!r:s', { alias: '#lobby:s' })), '#lobby:s');
  assert.equal(roomLabel(room('!r:s')), '!r:s');
  assert.equal(memberText(1), '1 member');
  assert.equal(memberText(0), '0 members');
  assert.equal(memberText(12345), '12,345 members');
});

test('GIVEN failure codes WHEN shown THEN each failure says what happened and what to do', () => {
  assert.match(listFailure('rejected', true), /do not share their directory/);
  assert.match(listFailure('rejected', false), /Your homeserver refused/);
  assert.match(listFailure('core_unavailable', false), /could not be reached/);
  assert.match(listFailure('locked', false), /locked/);
  assert.match(listFailure('limit_exceeded', false), /still running/);
  assert.match(joinFailure('not_found'), /Browse again/);
  assert.match(joinFailure('rejected'), /refused/);
  assert.match(joinFailure('limit_exceeded'), /Try again in a moment/);
  assert.match(joinFailure('core_unavailable'), /could not be reached.*room list/);
});

test('GIVEN the directory dialog opens WHEN nothing was browsed THEN it names the own homeserver as inert text and lists nothing', () => {
  const html = shown(
    render(PublicRooms, { props: { homeserver: hostile, onopen() {}, onjoined() {}, onclose() {} } }).body
  );
  assert.match(html, /aria-label="Public rooms"/);
  assert.doesNotMatch(html, /<img/, 'the server name is escaped');
  assert.match(html, /placeholder="&lt;img src=x onerror=alert\(1\)> \(your homeserver\)"/);
  assert.match(html, /<button type="submit" class="primary">Browse<\/button>/);
  assert.doesNotMatch(html, /public-room-list/, 'nothing is listed before Browse');
  assert.match(html, /Your homeserver sees the server name and the search text/);
});

test('GIVEN the room list WHEN an account is signed in THEN Explore public rooms shows below the rooms, and not without an account', () => {
  const props = {
    rooms: { rooms: [], truncated: false },
    selected: null,
    userId: null,
    shortcut: 'Ctrl K',
    onselect() {},
    onsearch() {},
    onsettings() {}
  };
  assert.match(shown(render(Sidebar, { props: { ...props, onexplore() {} } }).body), /class="link explore-rooms"[^>]*>Explore public rooms<\/button>/);
  assert.doesNotMatch(shown(render(Sidebar, { props }).body), /Explore public rooms/);
});
