import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let render;
let MemberList;
let RoomList;
let Menu;
let Message;
let Timeline;

before(async () => {
  // A cache of its own: test files run in parallel (see views.test.mjs).
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/room-controls',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: MemberList } = await vite.ssrLoadModule('/src/MemberList.svelte'));
  ({ default: RoomList } = await vite.ssrLoadModule('/src/RoomList.svelte'));
  ({ default: Menu } = await vite.ssrLoadModule('/src/Menu.svelte'));
  ({ default: Message } = await vite.ssrLoadModule('/src/Message.svelte'));
  ({ default: Timeline } = await vite.ssrLoadModule('/src/Timeline.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

const hostile = '<img src=x onerror=alert(1)>';
const shown = (html) => html.replace(/<!--.*?-->/g, '');
const row = (userId, trust, extra = {}) => ({ userId, name: null, trust, own: false, invited: false, ...extra });
const members = (props) =>
  shown(
    render(MemberList, {
      props: { onverify() {}, onmessage() {}, oncancelask() {}, onretry() {}, ...props }
    }).body
  );

test('GIVEN room members WHEN listed THEN each unverified person has a Verify icon and each verified person a verified icon', () => {
  const html = members({
    rows: [
      row('@me:s', 'verified', { own: true }),
      row('@amy:s', 'unverified', { name: hostile }),
      row('@bob:s', 'verified'),
      row('@cat:s', 'changed'),
      row('@dan:s', 'no_identity'),
      row('@eve:s', 'unknown')
    ],
    canVerify: true
  });
  assert.doesNotMatch(html, /<img/, 'names are escaped');
  assert.match(html, /<bdi class="member-name">&lt;img src=x onerror=alert\(1\)><\/bdi>/);
  assert.equal((html.match(/class="icon-action member-verify"/g) ?? []).length, 1, 'only the unverified person');
  assert.match(html, /<span class="visually-hidden">Verify <bdi>&lt;img src=x onerror=alert\(1\)><\/bdi><\/span>/);
  assert.match(html, /aria-hidden="true">Verify<\/span>/, 'the label is a tooltip');
  assert.match(html, /class="member-mark verified" role="img" aria-label="Verified"/);
  assert.match(html, /class="member-mark changed" role="img" aria-label="Identity changed"/);
  assert.match(html, /class="member-mark no_identity" role="img" aria-label="Has not set up encryption"/);
  assert.equal((html.match(/member-mark verified/g) ?? []).length, 1, 'own and unknown trust show no verified icon');
  assert.match(html, /<span class="member-own">You<\/span>/);
  assert.doesNotMatch(html, /Verify this device to verify other people/);
});

test('GIVEN this device is not verified, or a verification runs WHEN members are listed THEN Verify is disabled', () => {
  const unverifiedDevice = members({ rows: [row('@amy:s', 'unverified')], canVerify: false });
  assert.match(unverifiedDevice, /class="icon-action member-verify" disabled/);
  assert.match(unverifiedDevice, /Verify this device to verify other people\./);
  const busy = members({ rows: [row('@amy:s', 'unverified')], canVerify: true, verifying: true });
  assert.match(busy, /class="icon-action member-verify" disabled/);
});

test('GIVEN a member with no direct conversation WHEN Verify was pressed THEN the panel offers to start one, with its failure', () => {
  const html = members({
    rows: [row('@amy:s', 'unverified', { name: hostile }), row('@bob:s', 'unverified')],
    canVerify: true,
    asking: '@amy:s',
    messageFailure: 'The conversation could not be started.'
  });
  assert.equal((html.match(/class="member-ask"/g) ?? []).length, 1);
  assert.match(html, /Start one with <bdi>&lt;img src=x onerror=alert\(1\)><\/bdi>, then verify them there\./);
  assert.match(html, />Start conversation<\/button>/);
  assert.match(html, /role="alert">The conversation could not be started\./);
  assert.match(members({ rows: [row('@amy:s', 'unverified')], asking: '@amy:s', messageBusy: true }), /disabled[^>]*>Opening…/);
});

test('GIVEN the member list WHEN loading, failed or cut short THEN it says so', () => {
  assert.match(members({ rows: null }), /role="status">Loading members…/);
  const failed = members({ rows: null, failed: true });
  assert.match(failed, /The members could not be loaded\./);
  assert.match(failed, />Try again<\/button>/);
  assert.doesNotMatch(failed, /Loading members/);
  const cut = members({ rows: [row('@a:s', 'unverified'), row('@b:s', 'verified', { invited: true })], truncated: true });
  assert.match(cut, /<span class="member-count">2\+<\/span>/);
  assert.match(cut, /Showing the first 2 members\./);
  assert.match(cut, /<span class="member-id">Invited<\/span>/);
});

const room = (roomId, displayName) => ({ roomId, displayName, unreadCount: 0, encryption: 'encrypted', contact: null });

test('GIVEN rooms WHEN a favourite action is given THEN each room has a star toggle with a stable name and a tooltip', () => {
  const html = shown(
    render(RoomList, {
      props: {
        rooms: [room('!a:s', hostile), room('!b:s', 'Plain')],
        truncated: false,
        selected: null,
        preferences: [{ roomId: '!a:s', favourite: true, section: null }],
        onselect() {},
        onfavourite() {}
      }
    }).body
  );
  assert.doesNotMatch(html, /<img/);
  assert.equal((html.match(/favourite-toggle/g) ?? []).length, 2);
  assert.match(html, /aria-pressed="true"><svg class="icon filled"[\s\S]*?<span class="visually-hidden">Favourite <bdi>&lt;img/);
  assert.match(html, /aria-hidden="true">Remove from favourites</);
  assert.match(html, /aria-pressed="false"><svg class="icon "[\s\S]*?<span class="visually-hidden">Favourite <bdi>Plain<\/bdi>/);
  assert.match(html, /aria-hidden="true">Add to favourites</);
  // The star is a sibling of the room button, never nested in it.
  assert.doesNotMatch(html, /class="room-row"[^]*?<button[^]*?<\/button>\s*<\/button>/);
});

test('GIVEN rooms WHEN no favourite action is given or a change is saving THEN no star shows, or the stars are disabled', () => {
  const props = { rooms: [room('!a:s', 'A')], truncated: false, selected: null, onselect() {} };
  assert.doesNotMatch(render(RoomList, { props }).body, /favourite-toggle/);
  assert.match(render(RoomList, { props: { ...props, onfavourite() {}, favouriteBusy: true } }).body, /favourite-toggle" disabled/);
});

test('GIVEN a closed menu WHEN rendered THEN the icon trigger is a labelled menu button and no commands are in the page', () => {
  const html = shown(
    render(Menu, { props: { id: 'room-menu', label: 'Room menu', items: [{ label: 'Room settings', onselect() {} }] } }).body
  );
  assert.match(html, /<button id="room-menu-button" type="button" class="icon-button menu-trigger" aria-haspopup="menu" aria-expanded="false">/);
  assert.match(html, /<span class="visually-hidden">Room menu<\/span>/);
  assert.match(html, /aria-hidden="true">Room menu<\/span>/);
  assert.doesNotMatch(html, /role="menu"/);
  assert.doesNotMatch(html, /Room settings/);
});

test('GIVEN a message WHEN its actions show THEN each is an icon with its label as a tooltip and the sender in its name', () => {
  const html = shown(
    render(Message, {
      props: {
        name: hostile,
        body: 'hello',
        onreact() {},
        onreply() {},
        onedit() {},
        ondelete() {},
        onpin() {},
        onreport() {}
      }
    }).body
  );
  assert.doesNotMatch(html, /<img/);
  const actions = [
    ['React', 'React to <bdi>&lt;img src=x onerror=alert\\(1\\)><\\/bdi>'],
    ['Reply', 'Reply to <bdi>&lt;img src=x onerror=alert\\(1\\)><\\/bdi>'],
    ['Edit', 'Edit your message'],
    ['Delete', 'Delete your message'],
    ['Pin message', 'Pin message from <bdi>&lt;img src=x onerror=alert\\(1\\)><\\/bdi>'],
    ['Report message', 'Report message from <bdi>&lt;img src=x onerror=alert\\(1\\)><\\/bdi>']
  ];
  for (const [tip, name] of actions) {
    assert.match(html, new RegExp(`<span class="visually-hidden">${name}</span></button> <span class="action-tip" aria-hidden="true">${tip}</span>`), tip);
  }
  assert.match(html, /aria-expanded="false"><svg/, 'React says whether the quick reactions are open');
  assert.doesNotMatch(html, /class="link"/, 'no text buttons');
});

test('GIVEN a message without action handlers WHEN rendered THEN no action bar shows', () => {
  assert.doesNotMatch(render(Message, { props: { name: '@a:s', body: 'hi' } }).body, /message-actions/);
});

test('GIVEN images, files and notices WHEN pin and report are offered THEN they are icons, and a notice cannot be pinned', () => {
  const html = shown(
    render(Timeline, {
      props: {
        title: 'Room',
        roomId: '!r:s',
        items: [
          { kind: 'notice', event_id: '$n', sender: '@b:s', notice: 'undecryptable', origin_server_ts: 0 },
          { kind: 'notice', event_id: '$own', sender: '@me:s', notice: 'undecryptable', origin_server_ts: 0 }
        ],
        hasOlder: false,
        loading: false,
        error: null,
        onolder() {},
        onpin() {},
        onreport() {},
        ownUserId: '@me:s'
      }
    }).body
  );
  assert.equal((html.match(/class="item-actions"/g) ?? []).length, 2);
  assert.doesNotMatch(html, /Pin message/, 'a notice cannot be pinned');
  assert.equal((html.match(/<span class="visually-hidden">Report message from <bdi>@b:s<\/bdi>/g) ?? []).length, 1);
  assert.doesNotMatch(html, /Report message from <bdi>@me:s/, 'own events cannot be reported');
});
