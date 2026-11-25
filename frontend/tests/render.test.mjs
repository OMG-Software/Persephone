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
  assert.match(html, /disabled="" aria-label="Unpin: [^"]*">Unpin/);
  assert.match(html, /aria-label="View in timeline: Message from &lt;img/);
  assert.match(html, /aria-label="Unpin: Message from &lt;img/);
});

test('GIVEN unavailable pinned content WHEN rendered THEN retain a view control and explain absence', async () => {
  const { default: Row } = await vite.ssrLoadModule('/src/PinnedMessageRow.svelte');
  const { body: html } = render(Row, { props: { canManage: true, busy: false, onview() {}, onunpin() {} } });
  assert.match(html, /Message content is unavailable/);
  assert.match(html, /View in timeline/);
  assert.match(html, /aria-label="View in timeline: Unavailable pinned message"/);
  assert.match(html, /aria-label="Unpin: Unavailable pinned message"/);
  assert.doesNotMatch(html, /disabled/);
});


test('GIVEN a timeline pin action WHEN rendered THEN its accessible name identifies the isolated sender', async () => {
  const { default: Timeline } = await vite.ssrLoadModule('/src/Timeline.svelte');
  const { body: html } = render(Timeline, { props: {
    title: 'Room', roomId: '!r:s', items: [{ kind: 'message', event_id: '$e', sender: '<sender>', body: 'body', origin_server_ts: 0 }],
    hasOlder: false, loading: false, error: null, onolder() {}, onpin() {}
  } });
  const shown = html.replace(/<!--.*?-->/g, '');
  assert.match(shown, /<span class="visually-hidden">Pin message from <bdi>&lt;sender><\/bdi><\/span><\/button> <span class="action-tip" aria-hidden="true">Pin message<\/span>/);
});


test('GIVEN identical pinned messages WHEN controls render THEN both action names include distinct event IDs', async () => {
  const { default: Row } = await vite.ssrLoadModule('/src/PinnedMessageRow.svelte');
  const rendered = ['$one', '$two'].map((eventId) => render(Row, { props: {
    item: { kind: 'message', event_id: eventId, sender: '@alice:example.org', body: 'Same text' },
    eventId, canManage: true, busy: false, onview() {}, onunpin() {}
  } }).body);
  for (const [index, id] of ['$one', '$two'].entries()) {
    assert.ok(rendered[index].includes(`aria-label="View in timeline: Message from @alice:example.org: Same text; event ${id}"`));
    assert.ok(rendered[index].includes(`aria-label="Unpin: Message from @alice:example.org: Same text; event ${id}"`));
  }
});

test('GIVEN hostile caller and room names WHEN the ring prompt renders THEN they stay inert and isolated, and video needs a camera', async () => {
  const { default: RingPrompt } = await vite.ssrLoadModule('/src/RingPrompt.svelte');
  const hostile = '<img src=x onerror=alert(1)><script>alert(2)</script>';
  const rings = [{ ringId: 1, roomId: '!r:s', caller: hostile, intent: 'video', secondsLeft: 30, endsAt: 30_000 }];
  const props = { rings, roomNames: new Map([['!r:s', hostile]]), busy: new Set(), onanswer() {}, ondecline() {} };
  const { body: withCamera } = render(RingPrompt, { props: { ...props, camera: true } });
  assert.match(withCamera, /Incoming call/);
  assert.match(withCamera, /<bdi>&lt;img/);
  assert.doesNotMatch(withCamera, /<(?:img|script)\b/i);
  assert.match(withCamera, />Answer</);
  assert.match(withCamera, />Answer with video</);
  assert.match(withCamera, />Decline</);
  const { body: noCamera } = render(RingPrompt, { props: { ...props, camera: false } });
  assert.match(noCamera, />Answer</);
  assert.doesNotMatch(noCamera, /Answer with video/);
  const { body: busy } = render(RingPrompt, { props: { ...props, camera: true, busy: new Set(['!r:s']) } });
  assert.equal((busy.match(/disabled/g) ?? []).length, 3);
  const { body: none } = render(RingPrompt, { props: { ...props, rings: [], camera: true } });
  assert.doesNotMatch(none, /Answer|Decline/);
});

test('GIVEN a room with a call and one without WHEN the room list renders THEN only the first shows the call mark', async () => {
  const { default: RoomList } = await vite.ssrLoadModule('/src/RoomList.svelte');
  const room = (roomId, hasCall) => ({ roomId, displayName: roomId, unreadCount: 0, encryption: 'encrypted', contact: null, pictureId: null, hasCall });
  const { body: html } = render(RoomList, { props: {
    rooms: [room('!a:s', true), room('!b:s', false)], selected: null, truncated: false, onselect() {}
  } });
  assert.equal((html.match(/Call in progress/g) ?? []).length, 1);
});

test('GIVEN a call whose ring was declined WHEN the call bar renders THEN it says Declined and the call goes on', async () => {
  const { default: CallBar } = await vite.ssrLoadModule('/src/CallBar.svelte');
  const status = { phase: 'active', roomId: '!r:s', muted: false, participants: [], ended: null, joinError: null, camera: 'off', video: [], ringDeclined: true };
  const props = { dismissed: false, full: true, roomName: 'Plans', names: new Map(), busy: false, cameraBusy: false, onmute() {}, oncamera() {}, onleave() {}, ondismiss() {} };
  const { body: declined } = render(CallBar, { props: { ...props, status } });
  assert.match(declined, />Declined</);
  assert.match(declined, />Leave</);
  const { body: plain } = render(CallBar, { props: { ...props, status: { ...status, ringDeclined: false } } });
  assert.doesNotMatch(plain, /Declined/);
});

test('GIVEN a call window that opens WHEN the call bar renders THEN it shows the room, the phase, the call window button and Leave only', async () => {
  const { default: CallBar } = await vite.ssrLoadModule('/src/CallBar.svelte');
  const status = {
    phase: 'active', roomId: '!r:s', muted: true, deafened: false, volume: 100, ended: null, joinError: null,
    camera: 'not_found', screen: 'on', video: [], ringDeclined: true, deviceChange: null,
    participants: [{ userId: '@a:s', deviceId: 'D', speaking: true, reachable: false, keyed: false, sharing: true, muted: true }]
  };
  const props = { dismissed: false, full: false, roomName: 'Plans', names: new Map(), busy: false, cameraBusy: false, screenBusy: false, volume: 100, soundLevel: 80, onmute() {}, ondeafen() {}, onvolume() {}, oncamera() {}, onscreen() {}, onleave() {}, ondismiss() {} };
  const { body: short } = render(CallBar, { props: { ...props, status } });
  assert.match(short, /<bdi>Plans<\/bdi>/);
  assert.match(short, />In call</);
  assert.match(short, /class="secondary call-window-open"/);
  assert.match(short, />Declined</);
  // The camera and screen share notes still show: the call window may be closed.
  assert.match(short, /class="call-camera-note note"[^>]*>The chosen camera is not connected</);
  assert.match(short, /class="call-screen-note note"[^>]*>You are sharing your screen</);
  // Leave stays, in case the call page stops working.
  assert.match(short, /class="secondary call-leave"[^>]*>Leave</);
  const controls = /call-mute|call-deafen|class="secondary call-camera"|class="secondary call-screen"|type="range"|role="meter"|call-people|@a:s/;
  assert.doesNotMatch(short, controls);
  assert.doesNotMatch(short, /call-device-note/);
  // A device change still shows, and the bar stays short.
  const { body: changed } = render(CallBar, { props: { ...props, status: { ...status, deviceChange: 'failed' } } });
  assert.match(changed, /class="call-device-note note"/);
  assert.match(changed, /rejoin/);
  assert.doesNotMatch(changed, controls);
  assert.doesNotMatch(short, /call-window-open"[^>]*disabled/);
  // While an open waits for the call page, the button says so and is off.
  const { body: opening } = render(CallBar, { props: { ...props, status, opening: true } });
  assert.match(opening, /class="secondary call-window-open"[^>]*disabled[^>]*>[\s\S]*?Opening the call window…/);
  assert.match(opening, /class="secondary call-leave"/);
  // An ended call still says why, and can be dismissed.
  const { body: ended } = render(CallBar, { props: { ...props, status: { ...status, phase: 'ended', ended: 'internal' } } });
  assert.match(ended, /call-dismiss/);
  assert.doesNotMatch(ended, /call-window-open/);
});

test('GIVEN an active call and a sound level WHEN the call bar renders THEN a meter follows the volume slider, loud near full scale', async () => {
  const { default: CallBar } = await vite.ssrLoadModule('/src/CallBar.svelte');
  const status = { phase: 'active', roomId: '!r:s', muted: false, deafened: false, volume: 100, participants: [], ended: null, joinError: null, camera: 'off', screen: 'off', video: [], ringDeclined: false };
  const props = { dismissed: false, full: true, roomName: 'Plans', names: new Map(), busy: false, cameraBusy: false, screenBusy: false, volume: 100, onmute() {}, ondeafen() {}, onvolume() {}, oncamera() {}, onscreen() {}, onleave() {}, ondismiss() {} };
  const { body: quiet } = render(CallBar, { props: { ...props, status, soundLevel: 50 } });
  const slider = quiet.indexOf('type="range"');
  const meter = quiet.indexOf('role="meter"');
  assert.ok(slider >= 0 && meter > slider, 'the meter comes right after the volume slider');
  assert.match(quiet, /aria-label="Your microphone"/);
  assert.match(quiet, /aria-valuenow="50"/);
  assert.match(quiet, /aria-valuetext="−30 dB"/);
  assert.match(quiet, /scaleX\(0\.5\)/);
  assert.doesNotMatch(quiet, /class="call-level[^"]*loud/);
  const { body: loud } = render(CallBar, { props: { ...props, status, soundLevel: 95 } });
  assert.match(loud, /class="call-level[^"]*loud/);
  const { body: ended } = render(CallBar, { props: { ...props, status: { ...status, phase: 'ended', ended: 'internal' }, soundLevel: 95 } });
  assert.doesNotMatch(ended, /role="meter"/);
});

test('GIVEN a call with a camera on and one off WHEN the call window renders THEN each device has a cell and a roster row with a meter and a mute mark', async () => {
  const { default: CallWindow } = await vite.ssrLoadModule('/src/CallWindow.svelte');
  const person = (userId, deviceId, muted) => ({ userId, deviceId, speaking: false, reachable: true, keyed: true, sharing: false, muted });
  const hostile = '@<img src=x onerror=alert(1)>:s';
  const initialStatus = {
    phase: 'active', roomId: '!r:s', muted: true, deafened: false, volume: 100, ended: null, joinError: null,
    camera: 'off', screen: 'off', ringDeclined: false, deviceChange: null,
    participants: [person('@a:s', 'A', false), person(hostile, 'B', true)],
    video: [{ tile: 2, participant: { userId: '@a:s', deviceId: 'A' }, own: false, stream: 3 }]
  };
  const { body: html } = render(CallWindow, { props: { initialStatus } });
  // Three cells: yours and B's with placeholders, A's with a canvas (hidden
  // until its first picture comes).
  assert.equal((html.match(/class="video-tile call-cell/g) ?? []).length, 3);
  assert.equal((html.match(/<canvas/g) ?? []).length, 1);
  assert.equal((html.match(/class="call-placeholder"/g) ?? []).length, 3);
  // The roster is open, with a meter for you and for each device.
  assert.match(html, /aria-expanded="true"/);
  assert.match(html, />\s*Hide participants\s*</);
  const roster = html.slice(html.indexOf('id="call-roster"'));
  assert.doesNotMatch(roster.slice(0, roster.indexOf('>')), /hidden/);
  assert.equal((roster.match(/role="meter"/g) ?? []).length, 3);
  assert.match(roster, /aria-label="Your microphone"/);
  assert.match(roster, /aria-labelledby="call-roster-name-1"/);
  // Muted: you and B, in the grid and in the roster.
  assert.equal((html.match(/aria-label="Microphone off"/g) ?? []).length, 4);
  // User IDs are inert text.
  assert.match(html, /&lt;img/);
  assert.doesNotMatch(html, /<img\b/i);
  assert.match(html, />Leave</);

  // One icon button opens the technical details; the panel is closed.
  assert.equal((html.match(/aria-label="Technical details"/g) ?? []).length, 1);
  assert.match(html, /class="secondary call-stats-button"[^>]*aria-expanded="false"[^>]*aria-controls="call-window-stats"/);
  assert.doesNotMatch(html, /class="call-stats"/);

  const { body: idle } = render(CallWindow, { props: {} });
  assert.match(idle, /No call is running\./);
  assert.doesNotMatch(idle, /role="meter"|class="video-tile/);
});

test('GIVEN the technical details panel WHEN it renders before a reading THEN it waits, has a close button and shows no table', async () => {
  const { default: CallStats } = await vite.ssrLoadModule('/src/CallStats.svelte');
  const { body } = render(CallStats, { props: { onclose() {} } });
  assert.match(body, /id="call-window-stats" class="call-stats" role="dialog" aria-label="Technical details"/);
  assert.match(body, /aria-label="Close technical details"/);
  assert.match(body, /Waiting for the first reading\./);
  assert.doesNotMatch(body, /<table/);
});

test('GIVEN statistics tables WHEN their captions render THEN a device shows its user ID and its own isolated device ID, a hostile device ID stays inert, and a part of the call shows its title', async () => {
  const { default: CallStatsCaption } = await vite.ssrLoadModule('/src/CallStatsCaption.svelte');
  const section = (more) => ({ key: 'k', title: '', userId: null, deviceId: null, rows: [], ...more });
  const caption = (more) => render(CallStatsCaption, { props: { section: section(more) } }).body;

  const device = caption({ userId: '@bob:s', deviceId: 'PHONE' });
  assert.match(device, /<bdi>@bob:s<\/bdi>/);
  assert.match(device, /class="call-stats-device">\(<span class="visually-hidden">device <\/span><bdi>PHONE<\/bdi>\)<\/span>/);

  // A right-to-left override in a device ID stays inside its own isolate.
  const hostile = caption({ userId: '@eve:s', deviceId: '‮<img src=x onerror=alert(1)>' });
  assert.match(hostile, /<bdi>‮&lt;img src=x onerror=alert\(1\)>?(&gt;)?<\/bdi>\)<\/span>/);
  assert.doesNotMatch(hostile, /<img/);

  const own = caption({ title: 'Your microphone' });
  assert.match(own, /Your microphone/);
  assert.doesNotMatch(own, /<bdi>|call-stats-device/);
});
