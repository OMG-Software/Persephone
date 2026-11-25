import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DevicePanel } from '../src/callDevicePanel.ts';
import { DEVICE_FAILURE_TEXT, DEVICE_SAVE_FAILURE_TEXT } from '../src/calls.ts';

const DEVICES = {
  supported: true,
  recording: [{ id: 'mic-2', name: 'Desk microphone' }],
  playout: [{ id: 'spk-1', name: 'Speakers' }],
  cameras: [],
  selectedRecording: 'mic-2',
  selectedPlayout: null,
  selectedCamera: null,
  volume: 100,
  unreadable: false
};

/** A promise and the functions that settle it, for answers that wait. */
function later() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

/** A panel with the given bridge, and what it told its view. */
function panel(bridge) {
  const seen = { states: [], menus: [] };
  const made = new DevicePanel(
    bridge,
    (state) => seen.states.push(state),
    (menus) => seen.menus.push(menus)
  );
  return { panel: made, seen, last: () => seen.states.at(-1) };
}

const never = () => new Promise(() => {});
const menus = (recording) => ({ recording, playout: '', camera: '' });

test('GIVEN the lists WHEN the panel loads THEN it shows them and fills the menus from the saved choices', async () => {
  const { panel: made, seen, last } = panel({ list: async () => DEVICES, save: never });
  await made.load();
  assert.equal(seen.states[0].loading, true, 'it says it is loading first');
  assert.deepEqual(last(), { devices: DEVICES, loading: false, saving: false, saved: false, failure: null });
  assert.deepEqual(seen.menus, [{ recording: 'mic-2', playout: '', camera: '' }]);
});

test('GIVEN the core refuses WHEN the panel loads THEN it shows the failure and keeps no lists', async () => {
  for (const [reason, text] of [
    ['core_unavailable', DEVICE_FAILURE_TEXT.core_unavailable],
    ['something_else', DEVICE_FAILURE_TEXT.core_unavailable]
  ]) {
    const { panel: made, seen, last } = panel({ list: () => Promise.reject(reason), save: never });
    await made.load();
    assert.equal(last().failure, text, reason);
    assert.equal(last().devices, null);
    assert.equal(last().loading, false);
    assert.deepEqual(seen.menus, []);
  }
});

test('GIVEN a save WHEN the core accepts it THEN the panel says saved and sends the menus as choices', async () => {
  const sent = [];
  const { panel: made, seen, last } = panel({
    list: async () => DEVICES,
    save: async (...choice) => sent.push(choice)
  });
  await made.save({ recording: 'mic-2', playout: '', camera: 'cam-1' });
  assert.deepEqual(sent, [['mic-2', null, 'cam-1']], 'an empty menu is the system default');
  assert.equal(seen.states[0].saving, true);
  assert.deepEqual(last(), { devices: null, loading: false, saving: false, saved: true, failure: null });
});

test('GIVEN a save WHEN the core refuses it THEN the panel shows the failure and does not say saved', async () => {
  for (const [reason, text] of [
    ['invalid_argument', DEVICE_FAILURE_TEXT.invalid_argument],
    ['unknown', DEVICE_SAVE_FAILURE_TEXT]
  ]) {
    const { panel: made, last } = panel({ list: never, save: () => Promise.reject(reason) });
    await made.save(menus('mic-2'));
    assert.equal(last().failure, text, reason);
    assert.equal(last().saved, false);
    assert.equal(last().saving, false);
  }
});

test('GIVEN a save that runs WHEN another menu changes THEN that save is sent after it, none is dropped, and the newest sets the result', async () => {
  const first = later();
  const sent = [];
  const { panel: made, last } = panel({
    list: never,
    save: (...choice) => {
      sent.push(choice);
      return sent.length === 1 ? first.promise : Promise.resolve();
    }
  });
  const one = made.save(menus('mic-1'));
  const two = made.save(menus('mic-2'));
  await Promise.resolve();
  assert.deepEqual(sent, [['mic-1', null, null]], 'the second waits for the first');
  assert.equal(last().saving, true);
  first.reject('core_unavailable');
  await Promise.all([one, two]);
  assert.deepEqual(sent, [['mic-1', null, null], ['mic-2', null, null]]);
  // The second save held every change and worked, so the first's failure is old news.
  assert.deepEqual([last().saving, last().saved, last().failure], [false, true, null]);
});

test('GIVEN a save that runs WHEN a refresh is asked THEN it does not start and replace the menus', async () => {
  let lists = 0;
  const first = later();
  const { panel: made, seen } = panel({
    list: async () => {
      lists += 1;
      return DEVICES;
    },
    save: () => first.promise
  });
  const saving = made.save(menus('mic-2'));
  await made.load();
  assert.equal(lists, 0);
  first.resolve();
  await saving;
  await made.load();
  assert.equal(lists, 1, 'after the save it loads');
  assert.equal(seen.menus.length, 1);
});

test('GIVEN a closed panel WHEN a late answer comes THEN the panel does not change', async () => {
  const listed = later();
  const saved = later();
  const { panel: made, seen } = panel({ list: () => listed.promise, save: () => saved.promise });
  const loading = made.load();
  made.close();
  listed.resolve(DEVICES);
  await loading;
  const told = seen.states.length;
  assert.deepEqual(seen.menus, []);
  await made.save(menus('mic-2'));
  await made.load();
  assert.equal(seen.states.length, told, 'a closed panel starts nothing');
});

test('GIVEN a closed panel WHEN a running save ends THEN it says nothing', async () => {
  const saved = later();
  const { panel: made, seen } = panel({ list: never, save: () => saved.promise });
  const saving = made.save(menus('mic-2'));
  const told = seen.states.length;
  made.close();
  saved.reject('core_unavailable');
  await saving;
  assert.equal(seen.states.length, told);
});
