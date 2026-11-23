import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  CallOperations,
  CallRefresher,
  END_TEXT,
  IDLE_CALL,
  DEFAULT_DEVICE_LABEL,
  DEVICES_NOTE_TEXT,
  DEVICES_UNREADABLE_TEXT,
  DEVICES_UNSUPPORTED_TEXT,
  IN_CALL_TEXT,
  MISSING_DEVICE_LABEL,
  UNNAMED_DEVICE_LABEL,
  JOIN_ERROR_TEXT,
  UNAVAILABLE_TEXT,
  callAnnouncement,
  CAMERA_TEXT,
  VIDEO_CALL_LABEL,
  callButton,
  callReasonNote,
  keptReasonRoom,
  STARTING_TEXT,
  cameraButton,
  screenButton,
  SCREEN_TEXT,
  hasVideo,
  videoCallButton,
  deviceChoice,
  deviceOptions,
  deviceValue,
  devicesNote,
  showCallBar,
  MAX_RINGS,
  RING_ANSWER_LABEL,
  RING_ANSWER_VIDEO_LABEL,
  RING_DECLINE_LABEL,
  RING_TITLE,
  cameraAvailable,
  liveRings,
  nextRingEnd,
  ringButtons,
  ringsAnnouncement,
  ringsKey,
  shownRings,
  LatestOnly,
  volumeText,
  volumeValue
} from '../src/calls.ts';

const status = (over) => ({ ...IDLE_CALL, ...over });
const ready = (over) => ({ available: true, reason: null, activeMembers: 0, ...over });

test('GIVEN every call code in the contract WHEN its text is read THEN each is plain, non-empty text, except a normal leave', () => {
  const codes = {
    unavailable: ['unsupported_platform', 'not_joined', 'not_encrypted', 'not_permitted', 'no_call_server', 'call_server_unsupported', 'server_unreachable'],
    join: ['already_in_call', 'not_joined', 'not_encrypted', 'no_call_server', 'call_server_unsupported', 'declined', 'unsupported_platform', 'containment', 'no_worker', 'no_delayed_events', 'membership', 'call_server', 'keys', 'proxy_unreadable', 'device_setting_unreadable'],
    end: ['connection_failed', 'audio_device', 'device_not_found', 'containment', 'worker_stopped', 'worker_crashed', 'membership_lost', 'keys_unavailable', 'session_ended', 'internal']
  };
  for (const [table, list] of [[UNAVAILABLE_TEXT, codes.unavailable], [JOIN_ERROR_TEXT, codes.join], [END_TEXT, codes.end]]) {
    assert.deepEqual(Object.keys(table).filter((key) => key !== 'left').sort(), [...list].sort());
    for (const code of list) assert.match(table[code], /^[A-Z][^<>]*\.$/);
  }
  assert.equal(END_TEXT.left, '');
});

test('GIVEN no call and a ready room WHEN the button is built THEN it starts a call, or joins with the count', () => {
  assert.deepEqual(callButton('!r:s', ready(), IDLE_CALL, false), { hidden: false, disabled: false, label: 'Start call', reason: null });
  const join = callButton('!r:s', ready({ activeMembers: 3 }), IDLE_CALL, false);
  assert.equal(join.label, 'Join call (3)');
  assert.equal(join.disabled, false);
});

test('GIVEN an unavailable room WHEN the button is built THEN it is disabled with the reason, and keeps the count', () => {
  const view = callButton('!r:s', ready({ available: false, reason: 'not_encrypted', activeMembers: 2 }), IDLE_CALL, false);
  assert.equal(view.disabled, true);
  assert.equal(view.reason, 'Calls need an encrypted room.');
  assert.equal(view.label, 'Join call (2)');
});

test('GIVEN readiness not yet known WHEN the button is built THEN it is disabled without a reason', () => {
  assert.deepEqual([callButton('!r:s', null, IDLE_CALL, false).disabled, callButton('!r:s', null, IDLE_CALL, false).reason], [true, null]);
});

test('GIVEN a join waiting for the core WHEN the buttons are built THEN they are disabled and say that a proxy warning may be waiting', () => {
  for (const view of [callButton('!r:s', ready(), IDLE_CALL, true), videoCallButton('!r:s', ready(), IDLE_CALL, true)]) {
    assert.deepEqual([view.disabled, view.reason], [true, STARTING_TEXT]);
  }
  assert.match(STARTING_TEXT, /proxy warning/);
});

test('GIVEN this account is in a call WHEN buttons are built THEN that room hides it and other rooms are disabled', () => {
  for (const phase of ['joining', 'active']) {
    const current = status({ phase, roomId: '!a:s' });
    assert.equal(callButton('!a:s', ready(), current, false).hidden, true);
    const other = callButton('!b:s', ready({ activeMembers: 1 }), current, false);
    assert.deepEqual([other.hidden, other.disabled, other.reason], [false, true, IN_CALL_TEXT]);
  }
});

test('GIVEN a click on a call button that cannot start a call WHEN the note is built THEN it shows the reason as text', () => {
  const noServer = callButton('!r:s', ready({ available: false, reason: 'no_call_server' }), IDLE_CALL, false);
  assert.equal(callReasonNote('!r:s', '!r:s', noServer), UNAVAILABLE_TEXT.no_call_server);
  const elsewhere = callButton('!r:s', ready(), status({ phase: 'active', roomId: '!a:s' }), false);
  assert.equal(callReasonNote('!r:s', '!r:s', elsewhere), IN_CALL_TEXT);
});

test('GIVEN no click, a click in another room, or no view WHEN the note is built THEN there is none', () => {
  const view = callButton('!r:s', ready({ available: false, reason: 'not_encrypted' }), IDLE_CALL, false);
  assert.equal(callReasonNote('!r:s', null, view), null);
  assert.equal(callReasonNote('!r:s', '!other:s', view), null);
  assert.equal(callReasonNote('!r:s', '!r:s', null), null);
});

test('GIVEN a shown note WHEN the room becomes ready, the button hides, or no reason is known THEN the note goes', () => {
  assert.equal(callReasonNote('!r:s', '!r:s', callButton('!r:s', ready(), IDLE_CALL, false)), null);
  const here = callButton('!r:s', ready(), status({ phase: 'active', roomId: '!r:s' }), false);
  assert.equal(callReasonNote('!r:s', '!r:s', here), null);
  assert.equal(callReasonNote('!r:s', '!r:s', callButton('!r:s', null, IDLE_CALL, false)), null);
});

test('GIVEN a click on a blocked room WHEN it becomes ready and then blocked again THEN the note does not come back without a new click', () => {
  const blocked = callButton('!r:s', ready({ available: false, reason: 'server_unreachable' }), IDLE_CALL, false);
  const open = callButton('!r:s', ready(), IDLE_CALL, false);
  let asked = keptReasonRoom('!r:s', '!r:s', blocked);
  assert.equal(asked, '!r:s');
  asked = keptReasonRoom('!r:s', asked, open);
  assert.equal(asked, null);
  asked = keptReasonRoom('!r:s', asked, blocked);
  assert.equal(callReasonNote('!r:s', asked, blocked), null);
});

test('GIVEN a click on a blocked room WHEN another room or no room is shown THEN the click is forgotten', () => {
  const blocked = callButton('!b:s', ready({ available: false, reason: 'not_encrypted' }), IDLE_CALL, false);
  assert.equal(keptReasonRoom('!b:s', '!r:s', blocked), null);
  assert.equal(keptReasonRoom(null, '!r:s', blocked), null);
});

test('GIVEN an ended call WHEN the button is built THEN calls can start again', () => {
  const view = callButton('!a:s', ready(), status({ phase: 'ended', roomId: '!a:s', ended: 'internal' }), false);
  assert.deepEqual([view.hidden, view.disabled], [false, false]);
});

test('GIVEN each phase WHEN the bar is decided THEN it shows while joining, active, and ended until dismissed', () => {
  assert.equal(showCallBar(IDLE_CALL, false), false);
  assert.equal(showCallBar(status({ phase: 'joining' }), false), true);
  assert.equal(showCallBar(status({ phase: 'active' }), false), true);
  const ended = status({ phase: 'ended', ended: 'audio_device' });
  assert.equal(showCallBar(ended, false), true);
  assert.equal(showCallBar(ended, true), false);
  assert.equal(showCallBar(status({ phase: 'ended', ended: 'left' }), false), false);
});

test('GIVEN phase changes WHEN announcements are built THEN phases and end texts are read, and speaking changes are not', () => {
  assert.equal(callAnnouncement(status({ phase: 'joining' }), false), 'Connecting…');
  const speaker = { userId: '@a:s', deviceId: 'D', speaking: false, reachable: true, keyed: true };
  const quiet = callAnnouncement(status({ phase: 'active', participants: [speaker] }), false);
  const loud = callAnnouncement(status({ phase: 'active', participants: [{ ...speaker, speaking: true }] }), false);
  assert.equal(quiet, 'In call');
  assert.equal(loud, quiet);
  assert.equal(callAnnouncement(status({ phase: 'ended', ended: 'membership_lost' }), false), END_TEXT.membership_lost);
  assert.equal(callAnnouncement(status({ phase: 'ended', ended: 'membership_lost' }), true), '');
});

function deferred() {
  let resolve;
  const promise = new Promise((done) => (resolve = done));
  return { promise, resolve };
}

test('GIVEN a refresh in flight WHEN more requests arrive THEN exactly one more run follows and shows the newest reply', async () => {
  const gates = [];
  const shown = [];
  let runs = 0;
  const refresher = new CallRefresher(() => { runs += 1; const gate = deferred(); gates.push(gate); return gate.promise; }, (next) => shown.push(next.phase));
  refresher.request();
  refresher.request();
  refresher.request();
  assert.equal(runs, 1);
  gates[0].resolve(status({ phase: 'joining' }));
  await new Promise((done) => setImmediate(done));
  assert.equal(runs, 2);
  gates[1].resolve(status({ phase: 'active' }));
  await new Promise((done) => setImmediate(done));
  assert.equal(runs, 2);
  assert.deepEqual(shown, ['joining', 'active']);
  refresher.request();
  assert.equal(runs, 3);
});

test('GIVEN a failed refresh WHEN requested again THEN the shown call is kept and the next run works', async () => {
  const shown = [];
  let fail = true;
  const refresher = new CallRefresher(async () => { if (fail) throw new Error('down'); return status({ phase: 'active' }); }, (next) => shown.push(next.phase));
  refresher.request();
  await new Promise((done) => setImmediate(done));
  assert.deepEqual(shown, []);
  fail = false;
  refresher.request();
  await new Promise((done) => setImmediate(done));
  assert.deepEqual(shown, ['active']);
});

test('GIVEN a call shown and a reply in flight WHEN the session changes THEN the call UI clears and the old reply is dropped', async () => {
  const gate = deferred();
  const shown = [];
  const refresher = new CallRefresher(() => gate.promise, (next) => shown.push(next));
  refresher.request();
  refresher.reset();
  gate.resolve(status({ phase: 'active', roomId: '!old:s' }));
  await new Promise((done) => setImmediate(done));
  assert.deepEqual(shown, [IDLE_CALL]);
  assert.equal(showCallBar(shown.at(-1), false), false);
});

test('GIVEN connected devices WHEN the menu is built THEN the default comes first, names are kept as text, and an empty name is labelled', () => {
  const options = deviceOptions([{ id: 'a', name: 'Headset <b>x</b>' }, { id: 'b', name: '' }], null);
  assert.deepEqual(options, [
    { value: '', label: DEFAULT_DEVICE_LABEL },
    { value: 'a', label: 'Headset <b>x</b>' },
    { value: 'b', label: UNNAMED_DEVICE_LABEL }
  ]);
});

test('GIVEN a saved device that is connected WHEN the menu is built THEN there is no extra choice', () => {
  const options = deviceOptions([{ id: 'a', name: 'Headset' }], 'a');
  assert.equal(options.length, 2);
  assert.equal(deviceValue('a'), 'a');
});

test('GIVEN a saved device that is not connected WHEN the menu is built THEN one extra choice keeps it selected', () => {
  const options = deviceOptions([{ id: 'a', name: 'Headset' }], 'gone');
  assert.deepEqual(options.at(-1), { value: 'gone', label: MISSING_DEVICE_LABEL });
  assert.equal(options.filter((option) => option.label === MISSING_DEVICE_LABEL).length, 1);
  assert.equal(deviceValue('gone'), 'gone');
  assert.equal(deviceOptions([], null).length, 1);
});

test('GIVEN menu values WHEN turned into a choice THEN the default is null and a device ID is kept', () => {
  assert.equal(deviceChoice(''), null);
  assert.equal(deviceChoice('{0.0.1}.{a}'), '{0.0.1}.{a}');
  assert.equal(deviceValue(null), '');
});

test('GIVEN device replies WHEN the note is chosen THEN unsupported, unreadable and normal texts differ', () => {
  const base = { supported: true, recording: [], playout: [], selectedRecording: null, selectedPlayout: null, unreadable: false };
  assert.equal(devicesNote(base), DEVICES_NOTE_TEXT);
  assert.equal(devicesNote({ ...base, unreadable: true }), DEVICES_UNREADABLE_TEXT);
  assert.equal(devicesNote({ ...base, supported: false }), DEVICES_UNSUPPORTED_TEXT);
});

test('GIVEN each camera state WHEN its text is read THEN failures say what happened and working states say nothing', () => {
  assert.deepEqual(Object.keys(CAMERA_TEXT).sort(), ['not_found', 'off', 'on', 'starting', 'unavailable']);
  assert.equal(CAMERA_TEXT.not_found, 'The chosen camera is not connected');
  assert.equal(CAMERA_TEXT.unavailable, 'The camera could not start');
  assert.equal(CAMERA_TEXT.starting, 'Starting the camera…');
  assert.equal(CAMERA_TEXT.off, '');
  assert.equal(CAMERA_TEXT.on, '');
});

test('GIVEN each camera state WHEN the toggle is built THEN it offers to turn off only a camera that is on or starting', () => {
  assert.deepEqual(cameraButton('off'), { label: 'Turn camera on', pressed: false });
  assert.deepEqual(cameraButton('on'), { label: 'Turn camera off', pressed: true });
  assert.deepEqual(cameraButton('starting'), { label: 'Turn camera off', pressed: true });
  for (const failed of ['not_found', 'unavailable']) {
    assert.deepEqual(cameraButton(failed), { label: 'Turn camera on', pressed: false });
  }
});

test('GIVEN each screen state WHEN the button is built THEN it offers to stop only a share that is on or starting', () => {
  assert.deepEqual(screenButton('off'), { label: 'Share screen', pressed: false });
  assert.deepEqual(screenButton('on'), { label: 'Stop sharing', pressed: true });
  assert.deepEqual(screenButton('starting'), { label: 'Stop sharing', pressed: true });
  for (const failed of ['not_found', 'unavailable', 'refused']) {
    assert.deepEqual(screenButton(failed), { label: 'Share screen', pressed: false });
  }
  assert.equal(SCREEN_TEXT.off, '');
  for (const state of ['starting', 'on', 'not_found', 'unavailable', 'refused']) {
    assert.notEqual(SCREEN_TEXT[state], '', state);
  }
  assert.equal(IDLE_CALL.screen, 'off');
});

test('GIVEN the same room states WHEN both buttons are built THEN the video button follows the call button and only the label differs', () => {
  const cases = [
    [ready(), IDLE_CALL, false],
    [ready({ activeMembers: 2 }), IDLE_CALL, false],
    [ready({ available: false, reason: 'not_encrypted' }), IDLE_CALL, false],
    [null, IDLE_CALL, false],
    [ready(), IDLE_CALL, true],
    [ready(), status({ phase: 'active', roomId: '!r:s' }), false],
    [ready(), status({ phase: 'joining', roomId: '!other:s' }), false]
  ];
  for (const [readiness, call, pending] of cases) {
    const { label: callLabel, ...expected } = callButton('!r:s', readiness, call, pending);
    const { label, ...video } = videoCallButton('!r:s', readiness, call, pending);
    assert.deepEqual(video, expected);
    assert.notEqual(label, callLabel);
  }
  assert.equal(videoCallButton('!r:s', ready(), IDLE_CALL, false).label, VIDEO_CALL_LABEL);
  assert.equal(videoCallButton('!r:s', ready({ activeMembers: 3 }), IDLE_CALL, false).label, 'Join with video (3)');
});

test('GIVEN a call WHEN video tiles exist or not THEN the grid shows only during a call with tiles', () => {
  const tile = { tile: 0, participant: null, own: true };
  assert.equal(hasVideo(IDLE_CALL), false);
  assert.equal(hasVideo(status({ phase: 'active' })), false);
  assert.equal(hasVideo(status({ phase: 'active', video: [tile] })), true);
  assert.equal(hasVideo(status({ phase: 'joining', video: [tile] })), true);
  assert.equal(hasVideo(status({ phase: 'ended', video: [tile] })), false);
});

test('GIVEN a camera request pending WHEN leave starts and finishes THEN the late camera reply is not current', () => {
  const operations = new CallOperations();
  const camera = operations.begin();
  const leave = operations.begin();
  assert.equal(operations.isCurrent(leave), true);
  assert.equal(operations.isCurrent(camera), false, 'the late camera reply is ignored');
});

test('GIVEN a status read in flight WHEN a call command finishes before it THEN the read is dropped and the next read shows', async () => {
  const gates = [];
  const shown = [];
  const operations = new CallOperations();
  const refresher = new CallRefresher(
    () => { const gate = deferred(); gates.push(gate); return gate.promise; },
    (next) => shown.push(next.camera),
    () => operations.completed
  );
  refresher.request();
  // A camera command starts and finishes while the read runs; its reply
  // already showed `on`, and it asks for a fresh read.
  operations.complete(operations.begin());
  refresher.request();
  gates[0].resolve(status({ phase: 'active', camera: 'off' }));
  await new Promise((done) => setImmediate(done));
  assert.deepEqual(shown, [], 'the older read does not overwrite the reply');
  assert.equal(gates.length, 2, 'a fresh read follows');
  gates[1].resolve(status({ phase: 'active', camera: 'on' }));
  await new Promise((done) => setImmediate(done));
  assert.deepEqual(shown, ['on']);
});

test('GIVEN finished operations WHEN counted THEN each finish counts once', () => {
  const operations = new CallOperations();
  assert.equal(operations.completed, 0);
  operations.complete(operations.begin());
  operations.complete(operations.begin());
  assert.equal(operations.completed, 2);
});

test('GIVEN a command of the old session WHEN it finishes while the new session reads status THEN the read still shows', async () => {
  const gates = [];
  const shown = [];
  const operations = new CallOperations();
  const refresher = new CallRefresher(
    () => { const gate = deferred(); gates.push(gate); return gate.promise; },
    (next) => shown.push(next.phase),
    () => operations.completed
  );
  const old = operations.begin();
  // The session changes: the page resets and reads the new session's status.
  operations.reset();
  refresher.reset();
  refresher.request();
  assert.equal(operations.isCurrent(old), false, 'the old command is not current');
  // The old command finishes during that read; it asks for no new read.
  operations.complete(old);
  assert.equal(operations.completed, 0, 'the old command is not counted');
  gates[gates.length - 1].resolve(status({ phase: 'active' }));
  await new Promise((done) => setImmediate(done));
  assert.deepEqual(shown.slice(-1), ['active'], 'the new session read is shown');
  const next = operations.begin();
  assert.equal(operations.isCurrent(next), true);
  operations.complete(next);
  assert.equal(operations.completed, 1, 'a new session command counts');
});

test('GIVEN one operation WHEN no later one starts THEN its reply is current', () => {
  const operations = new CallOperations();
  assert.equal(operations.isCurrent(operations.begin()), true);
});

const ring = (over) => ({ ringId: 1, roomId: '!r:s', caller: '@alice:s', intent: 'audio', secondsLeft: 30, ...over });

test('GIVEN the core\'s list of rings WHEN the page stamps them THEN each ends at its seconds left, newest first as sent', () => {
  const list = { rings: [ring({ roomId: '!a:s', secondsLeft: 10 }), ring({ roomId: '!b:s', secondsLeft: 90, intent: 'video' })] };
  const shown = shownRings(list, 1_000);
  assert.deepEqual(shown.map((r) => [r.roomId, r.endsAt, r.intent]), [['!a:s', 11_000, 'audio'], ['!b:s', 91_000, 'video']]);
});

test('GIVEN a list with too many, repeated or timeless rings WHEN stamped THEN at most four remain, one for each room, each with time left', () => {
  const many = { rings: ['!1:s', '!2:s', '!3:s', '!4:s', '!5:s'].map((roomId) => ring({ roomId })) };
  assert.equal(shownRings(many, 0).length, MAX_RINGS);
  const repeated = { rings: [ring({ roomId: '!a:s' }), ring({ roomId: '!a:s', caller: '@mallory:s' })] };
  const shown = shownRings(repeated, 0);
  assert.equal(shown.length, 1);
  assert.equal(shown[0].caller, '@alice:s');
  const timeless = { rings: [ring({ secondsLeft: 0 }), ring({ roomId: '!n:s', secondsLeft: -3 }), ring({ roomId: '!x:s', secondsLeft: Number.NaN })] };
  assert.deepEqual(shownRings(timeless, 0), []);
  assert.deepEqual(shownRings({ rings: [] }, 0), []);
});

test('GIVEN rings with different ends WHEN time passes THEN an ended ring is removed and the next end is the nearest one', () => {
  const rings = shownRings({ rings: [ring({ roomId: '!a:s', secondsLeft: 5 }), ring({ roomId: '!b:s', secondsLeft: 20 })] }, 0);
  assert.equal(nextRingEnd(rings, 0), 5_000);
  assert.equal(nextRingEnd(rings, 4_000), 1_000);
  assert.equal(nextRingEnd(rings, 9_000), 0);
  assert.equal(nextRingEnd([], 0), null);
  assert.deepEqual(liveRings(rings, 4_999).map((r) => r.roomId), ['!a:s', '!b:s']);
  assert.deepEqual(liveRings(rings, 5_000).map((r) => r.roomId), ['!b:s']);
  assert.deepEqual(liveRings(rings, 20_000), []);
});

test('GIVEN a camera or none WHEN the prompt buttons are built THEN answer and decline always show and answer with video only with a camera', () => {
  assert.deepEqual(ringButtons(true), { answer: RING_ANSWER_LABEL, video: RING_ANSWER_VIDEO_LABEL, decline: RING_DECLINE_LABEL });
  assert.deepEqual(ringButtons(false), { answer: 'Answer', video: null, decline: 'Decline' });
  const devices = { supported: true, recording: [], playout: [], cameras: [{ id: 'c', name: 'Camera' }], selectedRecording: null, selectedPlayout: null, selectedCamera: null, unreadable: false };
  assert.equal(cameraAvailable(devices), true);
  assert.equal(cameraAvailable({ ...devices, cameras: [] }), false);
  assert.equal(cameraAvailable({ ...devices, supported: false }), false);
  assert.equal(cameraAvailable(null), false);
});

test('GIVEN rings or none WHEN announced THEN a ring says only Incoming call, never a name', () => {
  assert.equal(ringsAnnouncement([]), '');
  const rings = shownRings({ rings: [ring({ caller: '@<script>:s' })] }, 0);
  assert.equal(ringsAnnouncement(rings), RING_TITLE);
  assert.equal(RING_TITLE, 'Incoming call');
});

test('GIVEN shown rings WHEN a ring is added or replaced THEN the announcement key changes, and the time left does not change it', () => {
  const first = shownRings({ rings: [ring({ ringId: 1, roomId: '!a:s', caller: '@bob:s', secondsLeft: 30 })] }, 0);
  const later = shownRings({ rings: [ring({ ringId: 1, roomId: '!a:s', caller: '@bob:s', secondsLeft: 12 })] }, 5_000);
  const again = shownRings({ rings: [ring({ ringId: 2, roomId: '!a:s', caller: '@bob:s' })] }, 0);
  const replaced = shownRings({ rings: [ring({ ringId: 3, roomId: '!a:s', caller: '@carol:s' })] }, 0);
  const added = shownRings({ rings: [ring({ ringId: 1, roomId: '!a:s', caller: '@bob:s' }), ring({ ringId: 4, roomId: '!b:s', caller: '@dan:s' })] }, 0);
  assert.equal(ringsKey(later), ringsKey(first));
  assert.notEqual(ringsKey(again), ringsKey(first));
  assert.notEqual(ringsKey(replaced), ringsKey(first));
  assert.notEqual(ringsKey(added), ringsKey(first));
  assert.notEqual(ringsKey([]), ringsKey(first));
});

test('GIVEN the idle call WHEN read THEN the ring was not declined', () => {
  assert.equal(IDLE_CALL.ringDeclined, false);
});

test('GIVEN slider values WHEN read as a volume THEN they are whole numbers from 0 to 100, and a bad value is full volume', () => {
  assert.equal(volumeValue(0), 0);
  assert.equal(volumeValue(42.4), 42);
  assert.equal(volumeValue(100), 100);
  assert.equal(volumeValue(140), 100);
  assert.equal(volumeValue(-5), 0);
  assert.equal(volumeValue(Number.NaN), 100);
  assert.equal(volumeValue(Number.POSITIVE_INFINITY), 100);
  assert.equal(volumeText(35), '35%');
  assert.equal(IDLE_CALL.volume, 100);
  assert.equal(IDLE_CALL.deafened, false);
});

test('GIVEN a send that waits WHEN values arrive meanwhile THEN only the newest goes next, and settled runs once at the end', async () => {
  const sent = [];
  const gates = [];
  let settled = 0;
  let lastFailed = null;
  const latest = new LatestOnly(
    (value) => {
      sent.push(value);
      return new Promise((resolve) => gates.push(resolve));
    },
    (failed) => {
      settled += 1;
      lastFailed = failed;
    }
  );
  latest.set(10);
  latest.set(20);
  latest.set(30);
  assert.deepEqual(sent, [10]);
  assert.equal(latest.busy, true);
  gates.shift()();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(sent, [10, 30], 'the values in between are dropped');
  assert.equal(settled, 0);
  gates.shift()();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(settled, 1);
  assert.equal(lastFailed, false, 'the last value was sent');
  assert.equal(latest.busy, false);
});

test('GIVEN a failing send or a cleared value WHEN the sender runs THEN it goes on, and a cleared value is not sent', async () => {
  const sent = [];
  let release;
  const latest = new LatestOnly((value) => {
    sent.push(value);
    if (value === 1) return Promise.reject(new Error('locked'));
    return new Promise((resolve) => {
      release = resolve;
    });
  });
  latest.set(1);
  await new Promise((resolve) => setTimeout(resolve, 0));
  latest.set(2);
  latest.set(3);
  latest.clear();
  release();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(sent, [1, 2], 'a failure does not stop it, and 3 was cleared');
  assert.equal(latest.busy, false);
});

test('GIVEN sends that fail WHEN they settle THEN only a failure of the last value is reported, and a later success clears an earlier failure', async () => {
  const outcomes = [];
  const gates = [];
  const latest = new LatestOnly(
    (value) =>
      new Promise((resolve, reject) => {
        gates.push(() => (value < 0 ? reject(new Error('unavailable')) : resolve()));
      }),
    (failed) => outcomes.push(failed)
  );
  const settle = async () => {
    gates.shift()();
    await new Promise((resolve) => setTimeout(resolve, 0));
  };
  latest.set(-1);
  await settle();
  assert.deepEqual(outcomes, [true], 'the last value failed');
  latest.set(-2);
  latest.set(40);
  await settle();
  await settle();
  assert.deepEqual(outcomes, [true, false], 'the failure was replaced by a saved value');
  latest.set(50);
  latest.set(-3);
  await settle();
  await settle();
  assert.deepEqual(outcomes, [true, false, true], 'a saved value before a failed last one does not hide it');
});

test('GIVEN a send that waits WHEN the sender is cleared and that send fails THEN nothing is reported, and a value set after the clear reports its own outcome', async () => {
  const outcomes = [];
  const gates = [];
  const latest = new LatestOnly(
    (value) =>
      new Promise((resolve, reject) => {
        gates.push(() => (value < 0 ? reject(new Error('locked')) : resolve()));
      }),
    (failed) => outcomes.push(failed)
  );
  const settle = async () => {
    gates.shift()();
    await new Promise((resolve) => setTimeout(resolve, 0));
  };
  latest.set(-1);
  latest.clear();
  await settle();
  assert.deepEqual(outcomes, [], 'a failure from before the clear is not reported');
  assert.equal(latest.busy, false);

  latest.set(-2);
  latest.clear();
  latest.set(30);
  await settle();
  assert.deepEqual(outcomes, [], 'the disowned send is still not reported');
  await settle();
  assert.deepEqual(outcomes, [false], 'the value set after the clear was saved');

  latest.set(-3);
  latest.clear();
  latest.set(-4);
  await settle();
  await settle();
  assert.deepEqual(outcomes, [false, true], 'a failure after the clear is reported');
});
