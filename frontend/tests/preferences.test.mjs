import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as preferences from '../src/preferences.ts';

const key = (key, more = {}) => ({
  key,
  shiftKey: false,
  ctrlKey: false,
  metaKey: false,
  isComposing: false,
  ...more
});

test('GIVEN Enter to send WHEN keys are pressed THEN Enter sends and Shift+Enter starts a line', () => {
  const { composerKey } = preferences;
  assert.equal(composerKey(key('Enter'), true), 'send');
  assert.equal(composerKey(key('Enter', { shiftKey: true }), true), 'none');
  assert.equal(composerKey(key('Enter', { ctrlKey: true }), true), 'send');
  assert.equal(composerKey(key('Enter', { isComposing: true }), true), 'none', 'not while composing');
  assert.equal(composerKey(key('a'), true), 'none');
});

test('GIVEN Enter to send turned off WHEN keys are pressed THEN Enter starts a line and Ctrl or Cmd+Enter sends', () => {
  const { composerKey } = preferences;
  assert.equal(composerKey(key('Enter'), false), 'none');
  assert.equal(composerKey(key('Enter', { shiftKey: true }), false), 'none');
  assert.equal(composerKey(key('Enter', { ctrlKey: true }), false), 'send');
  assert.equal(composerKey(key('Enter', { metaKey: true }), false), 'send');
  assert.equal(
    composerKey(key('Enter', { ctrlKey: true, isComposing: true }), false),
    'none',
    'not while composing'
  );
});

test('GIVEN an account whose settings have not loaded THEN Enter sends', () => {
  assert.deepEqual(preferences.DEFAULT_SETTINGS, { enterSends: true });
});

// Lets queued promise callbacks run, such as a save waiting its turn.
const tick = () => new Promise((resolve) => setImmediate(resolve));

// A core whose replies the test settles by hand, in any order.
function keeper() {
  const waiting = { load: [], save: [] };
  const core = {
    load: () => new Promise((resolve, reject) => waiting.load.push({ resolve, reject })),
    save: (settings) =>
      new Promise((resolve, reject) => waiting.save.push({ settings, resolve, reject }))
  };
  const shown = { settings: null, failure: null };
  const kept = new preferences.SettingsKeeper(core, (settings, failure) => {
    shown.settings = settings;
    shown.failure = failure;
  });
  return { kept, waiting, shown };
}

test('GIVEN a signed-in account WHEN its settings load THEN they are shown', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve({ enterSends: false });
  await loading;
  assert.deepEqual(shown.settings, { enterSends: false });
  assert.equal(shown.failure, null);
});

test('GIVEN a change made while the settings load WHEN the load finishes later THEN the change stays', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  const changing = kept.change({ enterSends: false });
  assert.deepEqual(shown.settings, { enterSends: false }, 'shown at once');
  await tick();
  waiting.save[0].resolve();
  waiting.load[0].resolve({ enterSends: true });
  await Promise.all([loading, changing]);
  assert.deepEqual(shown.settings, { enterSends: false });
});

test('GIVEN a change WHEN the core refuses to save it THEN the old setting returns with a failure', async () => {
  const { kept, waiting, shown } = keeper();
  const changing = kept.change({ enterSends: false });
  await tick();
  assert.deepEqual(waiting.save[0].settings, { enterSends: false }, 'every setting is sent');
  waiting.save[0].reject('locked');
  await changing;
  assert.deepEqual(shown.settings, preferences.DEFAULT_SETTINGS);
  assert.equal(shown.failure, preferences.SAVE_FAILED);
});

test('GIVEN an account signed out WHEN its load finishes afterwards THEN the defaults stay', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  kept.clear();
  waiting.load[0].resolve({ enterSends: false });
  await loading;
  assert.deepEqual(shown.settings, preferences.DEFAULT_SETTINGS);
});

test('GIVEN settings that cannot be read WHEN they load THEN the defaults stay and nothing is reported', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].reject('internal');
  await loading;
  assert.equal(shown.settings, null, 'nothing replaced the defaults');
  assert.equal(shown.failure, null);
});

test('GIVEN two quick changes WHEN saved THEN the second save starts only after the first, in order', async () => {
  const { kept, waiting, shown } = keeper();
  const first = kept.change({ enterSends: false });
  const second = kept.change({ enterSends: true });
  await tick();
  assert.equal(waiting.save.length, 1, 'one save at a time');
  assert.deepEqual(waiting.save[0].settings, { enterSends: false });
  waiting.save[0].reject('internal');
  await first;
  await tick();
  assert.equal(waiting.save.length, 2, 'a failed save does not stop the next');
  assert.deepEqual(waiting.save[1].settings, { enterSends: true });
  waiting.save[1].resolve();
  await second;
  assert.deepEqual(shown.settings, { enterSends: true }, 'the newer change stays');
  assert.equal(shown.failure, null);
});
