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

test('GIVEN an account whose settings have not loaded THEN Enter sends, sizes are normal and nothing extra is sent or shown', () => {
  assert.deepEqual(preferences.DEFAULT_SETTINGS, {
    enterSends: true,
    chatTextPercent: 100,
    interfaceTextPercent: 100,
    desktopNotifications: false,
    sendTyping: false,
    sendReadReceipts: false
  });
});

test('GIVEN text sizes WHEN scaled THEN the interface zooms and message text keeps its own size', () => {
  const settings = (chat, ui) => ({ ...preferences.DEFAULT_SETTINGS, chatTextPercent: chat, interfaceTextPercent: ui });
  assert.deepEqual(preferences.textScale(settings(100, 100)), { interface: 1, chat: 1 });
  // Message text at 150% inside a 125% interface is 150% of normal, not 187.5%.
  const both = preferences.textScale(settings(150, 125));
  assert.equal(both.interface, 1.25);
  assert.ok(Math.abs(both.chat * both.interface - 1.5) < 1e-9);
  // Out of bounds, or not a number, is held to the bounds the core keeps.
  assert.deepEqual(preferences.textScale(settings(5000, 10)), { interface: 0.8, chat: 2 / 0.8 });
  assert.deepEqual(preferences.textScale(settings(NaN, NaN)), { interface: 1, chat: 1 });
  assert.deepEqual(
    [preferences.CHAT_TEXT_SIZES.at(0), preferences.CHAT_TEXT_SIZES.at(-1)],
    [80, 200]
  );
  assert.deepEqual(
    [preferences.INTERFACE_TEXT_SIZES.at(0), preferences.INTERFACE_TEXT_SIZES.at(-1)],
    [80, 150]
  );
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

// The account's stored settings in these tests: everything differs from the
// defaults, so a default written over a stored setting shows.
const STORED = {
  enterSends: false,
  chatTextPercent: 150,
  interfaceTextPercent: 125,
  desktopNotifications: true
};
const { DEFAULT_SETTINGS } = preferences;

test('GIVEN a signed-in account WHEN its settings load THEN they are shown', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve(STORED);
  await loading;
  assert.deepEqual(shown.settings, STORED);
  assert.equal(shown.failure, null);
});

test('GIVEN a change made while the settings load WHEN the load finishes THEN only that setting changes and the stored ones are kept', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  const changing = kept.change({ chatTextPercent: 90 });
  assert.equal(shown.settings.chatTextPercent, 90, 'shown at once');
  await tick();
  assert.equal(waiting.save.length, 0, 'nothing is saved before the stored settings are known');
  waiting.load[0].resolve(STORED);
  await loading;
  assert.deepEqual(shown.settings, { ...STORED, chatTextPercent: 90 });
  await tick();
  assert.deepEqual(waiting.save[0].settings, { ...STORED, chatTextPercent: 90 }, 'the stored settings are not overwritten');
  waiting.save[0].resolve();
  await changing;
  assert.deepEqual(shown.settings, { ...STORED, chatTextPercent: 90 });
  assert.equal(shown.failure, null);
});

test('GIVEN a change WHEN the core refuses to save it THEN only that setting returns, with a failure', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve(STORED);
  await loading;
  const changing = kept.change({ enterSends: true });
  await tick();
  assert.deepEqual(waiting.save[0].settings, { ...STORED, enterSends: true }, 'every setting is sent');
  waiting.save[0].reject('locked');
  await changing;
  assert.deepEqual(shown.settings, STORED);
  assert.equal(shown.failure, preferences.SAVE_FAILED);
});

test('GIVEN a change made while the settings load WHEN its save fails THEN the stored value returns, not the default', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  const changing = kept.change({ chatTextPercent: 90 });
  waiting.load[0].resolve(STORED);
  await loading;
  await tick();
  waiting.save[0].reject('internal');
  await changing;
  assert.deepEqual(shown.settings, STORED);
  assert.equal(shown.failure, preferences.SAVE_FAILED);
});

test('GIVEN a saved change WHEN a later save of the same setting fails THEN the saved value returns', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve(STORED);
  await loading;
  const first = kept.change({ chatTextPercent: 90 });
  await tick();
  waiting.save[0].resolve();
  await first;
  const second = kept.change({ chatTextPercent: 200 });
  await tick();
  waiting.save[1].reject('internal');
  await second;
  assert.deepEqual(shown.settings, { ...STORED, chatTextPercent: 90 });
});

test('GIVEN settings that cannot be read WHEN a change is made THEN they are read again first, and without them nothing is saved', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].reject('internal');
  await loading;
  const changing = kept.change({ desktopNotifications: true });
  await tick();
  assert.equal(waiting.load.length, 2, 'read again before saving');
  waiting.load[1].reject('internal');
  await changing;
  assert.equal(waiting.save.length, 0, 'the defaults are never saved over unread settings');
  assert.deepEqual(shown.settings, DEFAULT_SETTINGS);
  assert.equal(shown.failure, preferences.SAVE_FAILED);
});

test('GIVEN an account signed out WHEN its load finishes afterwards THEN the defaults stay', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  kept.clear();
  waiting.load[0].resolve(STORED);
  await loading;
  assert.deepEqual(shown.settings, DEFAULT_SETTINGS);
});

test('GIVEN settings that cannot be read WHEN they load THEN the defaults stay and nothing is reported', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].reject('internal');
  await loading;
  assert.equal(shown.settings, null, 'nothing replaced the defaults');
  assert.equal(shown.failure, null);
});

test('GIVEN two quick changes WHEN saved THEN the second save starts only after the first, and the first failure is undone before it', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve(STORED);
  await loading;
  const first = kept.change({ enterSends: true });
  const second = kept.change({ chatTextPercent: 80 });
  await tick();
  assert.equal(waiting.save.length, 1, 'one save at a time');
  assert.deepEqual(waiting.save[0].settings, { ...STORED, enterSends: true, chatTextPercent: 80 });
  waiting.save[0].reject('internal');
  await first;
  await tick();
  assert.equal(waiting.save.length, 2, 'a failed save does not stop the next');
  assert.deepEqual(
    waiting.save[1].settings,
    { ...STORED, chatTextPercent: 80 },
    'the refused value is not sent again'
  );
  waiting.save[1].resolve();
  await second;
  assert.deepEqual(shown.settings, { ...STORED, chatTextPercent: 80 }, 'the newer change stays');
  assert.equal(shown.failure, preferences.SAVE_FAILED, 'the first failure is reported');
});

test('GIVEN two changes of different settings WHEN both saves fail THEN both return to the stored values, and a later save sends neither', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve(STORED);
  await loading;
  const first = kept.change({ enterSends: true });
  const second = kept.change({ chatTextPercent: 80 });
  await tick();
  waiting.save[0].reject('internal');
  await first;
  await tick();
  waiting.save[1].reject('internal');
  await second;
  assert.deepEqual(shown.settings, STORED);
  const third = kept.change({ desktopNotifications: false });
  await tick();
  assert.deepEqual(waiting.save[2].settings, { ...STORED, desktopNotifications: false });
  waiting.save[2].resolve();
  await third;
});

test('GIVEN two changes setting the same value WHEN the first save fails THEN the second keeps it and saves it', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve(STORED);
  await loading;
  const first = kept.change({ enterSends: true, chatTextPercent: 90 });
  const second = kept.change({ enterSends: true });
  await tick();
  waiting.save[0].reject('internal');
  await first;
  await tick();
  assert.equal(shown.settings.enterSends, true, 'the second change still owns it');
  assert.equal(shown.settings.chatTextPercent, STORED.chatTextPercent, 'the first change is undone');
  assert.deepEqual(waiting.save[1].settings, { ...STORED, enterSends: true });
  waiting.save[1].resolve();
  await second;
});

test('GIVEN a value an earlier save confirmed WHEN the later change that set it fails THEN the confirmed value stays', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve(STORED);
  await loading;
  const first = kept.change({ enterSends: true });
  const second = kept.change({ chatTextPercent: 90 });
  await tick();
  // The first save carries the second change's value too, and succeeds.
  assert.equal(waiting.save[0].settings.chatTextPercent, 90);
  waiting.save[0].resolve();
  await first;
  await tick();
  waiting.save[1].reject('internal');
  await second;
  assert.equal(shown.settings.chatTextPercent, 90, 'what the core holds, not the value before the change');
  assert.equal(shown.failure, preferences.SAVE_FAILED);
});

test('GIVEN a save waiting behind another WHEN the account is signed out meanwhile THEN it is never sent', async () => {
  const { kept, waiting, shown } = keeper();
  const loading = kept.load();
  waiting.load[0].resolve(STORED);
  await loading;
  const first = kept.change({ enterSends: true });
  const second = kept.change({ chatTextPercent: 80 });
  await tick();
  kept.clear();
  waiting.save[0].resolve();
  await Promise.all([first, second]);
  await tick();
  assert.equal(waiting.save.length, 1, 'the queued save was skipped');
  assert.deepEqual(shown.settings, DEFAULT_SETTINGS);
});
