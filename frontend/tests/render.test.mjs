import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let Message;
let render;

before(async () => {
  // Test files run in parallel: each server keeps its own dependency cache,
  // and none discovers dependencies mid-run, so no server finds another's
  // pre-bundle outdated (ERR_OUTDATED_OPTIMIZED_DEP).
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/render',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: Message } = await vite.ssrLoadModule('/src/Message.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

test('hostile sender names and message bodies render as inert text', () => {
  const hostileName = '" onmouseover="alert(1) <img src=x onerror=alert(2)>';
  const hostileBody = '<script>alert(3)</script><svg onload=alert(4)></svg>';
  const { body: html } = render(Message, {
    props: { name: hostileName, body: hostileBody }
  });

  assert.match(html, /&lt;img/);
  assert.match(html, /&lt;script>/);
  assert.match(html, /&lt;svg/);
  assert.match(html, /aria-label="Message from &quot; onmouseover=&quot;alert\(1\) &lt;img/);
  assert.doesNotMatch(html, /<(?:img|script|svg)\b/i);
});

test('message view contains no raw HTML insertion or attribute spreading', async () => {
  const { readFile } = await import('node:fs/promises');
  const source = await readFile(new URL('../src/Message.svelte', import.meta.url), 'utf8');

  assert.doesNotMatch(source, /\{@html\b|\binnerHTML\b|\{@const\s+.*\bspread\b/i);
  assert.doesNotMatch(source, /\.\.\.\w+/);
});

test('GIVEN hostile local-search content WHEN a result renders THEN metadata and snippets remain inert', async () => {
  const { default: SearchHit } = await vite.ssrLoadModule('/src/SearchHit.svelte');
  const hostile = '<img src=x onerror=alert(1)><script>alert(2)</script>';
  const { body: html } = render(SearchHit, { props: {
    hit: { roomId: hostile, eventId: hostile, sender: hostile, body: hostile }, onselect() {}
  } });
  assert.match(html, /&lt;img/);
  assert.match(html, /&lt;script>/);
  assert.match(html, /<bdi>/);
  assert.match(html, /dir="auto"/);
  assert.doesNotMatch(html, /<(?:img|script)\b/i);
});

test('GIVEN hostile custom section names WHEN rooms render THEN names remain isolated and inert', async () => {
  const { default: RoomList } = await vite.ssrLoadModule('/src/RoomList.svelte');
  const { body: html } = render(RoomList, { props: {
    rooms: [{ roomId: '!r:s', displayName: '<script>room</script>', unreadCount: 0, encryption: 'encrypted', contact: false }],
    preferences: [{ roomId: '!r:s', favourite: false, section: '<img src=x onerror=alert(1)>' }],
    selected: null, truncated: false, onselect() {}
  } });
  assert.match(html, /<bdi>&lt;img/);
  assert.match(html, /&lt;script>/);
  assert.doesNotMatch(html, /<(?:img|script)\b/i);
});

test('GIVEN a report dialog WHEN rendered THEN report disclosure and separate ignore confirmation are visible', async () => {
  const { default: ReportAbuse } = await vite.ssrLoadModule('/src/ReportAbuse.svelte');
  const { body: html } = render(ReportAbuse, { props: {
    roomId: '!r:s', eventId: '$<script>x</script>', sender: '@<img>:s', onclose() {}, onmanageignore() {}
  } });
  assert.match(html, /own homeserver receives this room ID, event ID, and reason/);
  assert.match(html, /operator can read the reason/);
  assert.match(html, /Confirm and send report/);
  assert.match(html, /Ignoring is a separate action/);
  assert.match(html, /shared rooms/);
  assert.match(html, /Manage ignored users/);
  assert.match(html, /sender text above can be shortened/);
  assert.doesNotMatch(html, /<(?:img|script)\b/i);
});

test('GIVEN projected timeline sender text WHEN a report is offered THEN it never authorizes an ignore request', async () => {
  const { readFile } = await import('node:fs/promises');
  const source = await readFile(new URL('../src/ReportAbuse.svelte', import.meta.url), 'utf8');
  assert.doesNotMatch(source, /setIgnoredUser|ignoredUsers/);
  assert.match(source, /reportAbuse\(roomId, eventId, reason\)/);
});

test('GIVEN a hostile saved draft WHEN previewed THEN body and reply reference remain inert and isolated', async () => {
  const { default: DraftPreview } = await vite.ssrLoadModule('/src/DraftPreview.svelte');
  const hostile = '<script>alert(1)</script><img src=x onerror=alert(2)>';
  const { body: html } = render(DraftPreview, { props: { roomId: hostile, draft: { body: hostile, replyTo: hostile } } });
  assert.match(html, /<pre[^>]*dir="auto"/);
  assert.match(html, /<bdi>&lt;script>/);
  assert.doesNotMatch(html, /<(?:img|script)\b/i);
});

test('GIVEN orphaned room preferences WHEN listed THEN cleanup names only local profile changes', async () => {
  const { default: StoredRoomGroups } = await vite.ssrLoadModule('/src/StoredRoomGroups.svelte');
  const { body: html } = render(StoredRoomGroups, { props: {
    preferences: [{ roomId: '!left:<script>x</script>', favourite: true, section: '<img src=x>' }, { roomId: '!joined:s', favourite: true, section: null }],
    joinedRooms: ['!joined:s'], onchanged() {}
  } });
  assert.match(html, /Remove saved group/);
  assert.match(html, /only changes the local profile/);
  assert.doesNotMatch(html, /!joined:s/);
  assert.doesNotMatch(html, /<(?:img|script)\b/i);
});

test('GIVEN the Lock button WHEN rendered THEN help text describes it and says what Lock does', async () => {
  const { default: TipButton } = await vite.ssrLoadModule('/src/TipButton.svelte');
  const { LOCK_HELP } = await import('../src/help.ts');
  const { createRawSnippet } = await vite.ssrLoadModule('svelte');
  const { body: html } = render(TipButton, {
    props: {
      id: 'lock-help',
      help: LOCK_HELP,
      onclick() {},
      children: createRawSnippet(() => ({ render: () => '<span>Lock</span>' }))
    }
  });

  // Screen readers get the text through the button; the tip is shown on
  // hover and on keyboard focus (style.css).
  assert.match(html, /<button[^>]*aria-describedby="lock-help"/);
  assert.match(html, /<span[^>]*\bid="lock-help"[^>]*\brole="tooltip"|<span[^>]*\brole="tooltip"[^>]*\bid="lock-help"/);
  assert.ok(html.includes(LOCK_HELP), html);
  // What it closes, what it does not do, and what unlocking needs.
  assert.match(LOCK_HELP, /encrypted profile on this computer/);
  assert.match(LOCK_HELP, /clears the screen/);
  assert.match(LOCK_HELP, /does not sign you out of the server/);
  assert.match(LOCK_HELP, /passphrase/);
  assert.match(LOCK_HELP, /restart Persephone/);
});

test('GIVEN a pending dialog operation WHEN dismissal is disabled THEN its Close control is disabled', async () => {
  const { default: FeatureDialog } = await vite.ssrLoadModule('/src/FeatureDialog.svelte');
  const { createRawSnippet } = await vite.ssrLoadModule('svelte');
  for (const closeDisabled of [true, false]) {
    const { body: html } = render(FeatureDialog, { props: {
      title: 'Operation', onclose() {}, closeDisabled,
      children: createRawSnippet(() => ({ render: () => '<p>Pending operation</p>' }))
    } });
    const close = html.match(/<button\b[^>]*>Close<\/button>/)?.[0];
    assert.ok(close, html);
    assert.equal(/\bdisabled\b/.test(close), closeDisabled);
  }
});


test('GIVEN hostile pin metadata WHEN rendered THEN isolate inert text and disable unauthorized unpin', async () => {
  const { default: Row } = await vite.ssrLoadModule('/src/PinnedMessageRow.svelte');
  const hostile = '<img src=x onerror=alert(1)>';
  const { body: html } = render(Row, { props: {
    item: { kind: 'message', sender: hostile, body: hostile }, canManage: false, busy: false, onview() {}, onunpin() {}
  } });
  assert.match(html, /<bdi>&lt;img/);
  assert.match(html, /dir="auto"/);
  assert.doesNotMatch(html, /<img\b/);
  assert.match(html, /disabled[^>]*>Unpin/);
});

test('GIVEN unavailable pinned content WHEN rendered THEN retain a view control and explain absence', async () => {
  const { default: Row } = await vite.ssrLoadModule('/src/PinnedMessageRow.svelte');
  const { body: html } = render(Row, { props: { canManage: true, busy: false, onview() {}, onunpin() {} } });
  assert.match(html, /Message content is unavailable/);
  assert.match(html, /View in timeline/);
  assert.doesNotMatch(html, /disabled/);
});
