import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let RoomList;
let Timeline;
let SignIn;
let render;

before(async () => {
  vite = await createServer({
    configFile: './vite.config.js',
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: RoomList } = await vite.ssrLoadModule('/src/RoomList.svelte'));
  ({ default: Timeline } = await vite.ssrLoadModule('/src/Timeline.svelte'));
  ({ default: SignIn } = await vite.ssrLoadModule('/src/SignIn.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

const hostileName = '<img src=x onerror=alert(1)>';

test('room names render as isolated, escaped text with capped unread counts', () => {
  const { body: html } = render(RoomList, {
    props: {
      rooms: [
        { roomId: '!a:example.invalid', displayName: hostileName, unreadCount: 4294967295 },
        { roomId: '!b:example.invalid', displayName: null, unreadCount: 0 }
      ],
      truncated: true,
      selected: '!a:example.invalid',
      onselect: () => {}
    }
  });
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)>/);
  assert.match(html, /<bdi class="room-name">&lt;img/);
  // A room without a name falls back to its ID.
  assert.match(html, /<bdi class="room-name">!b:example\.invalid<\/bdi>/);
  assert.match(html, />999\+</);
  assert.match(html, /aria-current="true"/);
  assert.equal((html.match(/aria-current/g) ?? []).length, 1);
  assert.match(html, /Showing the first 2 rooms/);
  assert.match(html, /<nav aria-label="Rooms">/);
});

test('timeline bodies keep their text, are direction-isolated and never parsed as HTML', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: hostileName,
      items: [
        { kind: 'message', event_id: '$1', sender: '@m:example.invalid', body: '<script>alert(2)</script>' },
        { kind: 'message', event_id: '$2', sender: hostileName, body: 'Bidi ‮gnp.exe' },
        { kind: 'gap', next_cursor: null }
      ],
      hasOlder: true,
      loading: false,
      error: null,
      onolder: () => {}
    }
  });
  assert.doesNotMatch(html, /<script/);
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /&lt;script>alert\(2\)&lt;\/script>/);
  assert.equal((html.match(/<p dir="auto">/g) ?? []).length, 2);
  assert.match(html, /‮gnp\.exe/, 'bidi text is kept, isolated by dir=auto');
  assert.match(html, /Some messages are not shown here/);
  assert.match(html, /Load older messages/);
  assert.match(html, /<h2 id="timeline-heading"><bdi>&lt;img/);
});

test('timeline controls reflect loading, end of history and errors', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      items: [],
      hasOlder: false,
      loading: false,
      error: 'The Persephone core is unavailable.',
      onolder: () => {}
    }
  });
  assert.match(html, /<button type="button" disabled[^>]*>Start of history/);
  assert.match(html, /role="alert"/);
});

test('notices render as fixed text with an isolated sender, never as content', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      items: [
        { kind: 'notice', event_id: '$1', sender: hostileName, notice: 'undecryptable' },
        { kind: 'notice', event_id: '$2', sender: '@b:example.invalid', notice: 'unsupported' }
      ],
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {}
    }
  });
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /<bdi>&lt;img/);
  assert.match(html, /Encrypted message\. This device has no key for it\./);
  assert.match(html, /A kind of message Persephone does not show yet\./);
});

test('the sign-in form asks only for the homeserver; the password goes to a native window', () => {
  const signedOut = { state: 'signed_out', userId: null, error: null };
  const { body: html } = render(SignIn, {
    props: { account: signedOut, busy: false, onsignin: () => {} }
  });
  assert.match(html, /<label for="homeserver">Homeserver address<\/label>/);
  assert.match(html, /type="url"/);
  assert.doesNotMatch(html, /type="password"/i, 'no password field in the page');
  assert.equal((html.match(/<input/g) ?? []).length, 1, 'one field: the homeserver');
  assert.match(html, /This page never sees your password/);

  const failed = render(SignIn, {
    props: {
      account: { state: 'sign_in_failed', userId: null, error: 'invalid_credentials' },
      busy: false,
      onsignin: () => {}
    }
  }).body;
  assert.match(failed, /role="alert"/);
  assert.match(failed, /did not accept that username and password/);

  const waiting = render(SignIn, {
    props: { account: { state: 'signing_in', userId: null, error: null }, busy: false, onsignin: () => {} }
  }).body;
  assert.match(waiting, /Waiting for the Windows sign-in window/);
  assert.match(waiting, /<button type="submit" disabled/);
});

