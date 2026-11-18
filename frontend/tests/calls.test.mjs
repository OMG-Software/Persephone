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
  cameraButton,
  hasVideo,
  videoCallButton,
  deviceChoice,
  deviceOptions,
  deviceValue,
  devicesNote,
  showCallBar
} from '../src/calls.ts';

const status = (over) => ({ ...IDLE_CALL, ...over });
const ready = (over) => ({ available: true, reason: null, activeMembers: 0, ...over });

test('GIVEN every call code in the contract WHEN its text is read THEN each is plain, non-empty text, except a normal leave', () => {
  const codes = {
    unavailable: ['unsupported_platform', 'not_joined', 'not_encrypted', 'no_call_server', 'server_unreachable'],
    join: ['already_in_call', 'not_joined', 'not_encrypted', 'no_call_server', 'declined', 'unsupported_platform', 'containment', 'no_worker', 'no_delayed_events', 'membership', 'call_server', 'keys', 'proxy_unreadable', 'device_setting_unreadable'],
    end: ['connection_failed', 'audio_device', 'device_not_found', 'containment', 'worker_stopped', 'membership_lost', 'keys_unavailable', 'session_ended', 'internal']
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

test('GIVEN readiness not yet known or a join waiting WHEN the button is built THEN it is disabled without a reason', () => {
  assert.deepEqual([callButton('!r:s', null, IDLE_CALL, false).disabled, callButton('!r:s', null, IDLE_CALL, false).reason], [true, null]);
  const pending = callButton('!r:s', ready(), IDLE_CALL, true);
  assert.deepEqual([pending.disabled, pending.reason], [true, null]);
});

test('GIVEN this account is in a call WHEN buttons are built THEN that room hides it and other rooms are disabled', () => {
  for (const phase of ['joining', 'active']) {
    const current = status({ phase, roomId: '!a:s' });
    assert.equal(callButton('!a:s', ready(), current, false).hidden, true);
    const other = callButton('!b:s', ready({ activeMembers: 1 }), current, false);
    assert.deepEqual([other.hidden, other.disabled, other.reason], [false, true, IN_CALL_TEXT]);
  }
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
  operations.begin();
  operations.complete();
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
  operations.begin();
  operations.complete();
  operations.complete();
  assert.equal(operations.completed, 2);
});

test('GIVEN one operation WHEN no later one starts THEN its reply is current', () => {
  const operations = new CallOperations();
  assert.equal(operations.isCurrent(operations.begin()), true);
});
