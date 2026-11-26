import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';
import { DevicePanel } from '../src/callDevicePanel.ts';
import {
  BACKGROUND_EFFECTS_UNAVAILABLE_TEXT,
  BLUR_DEFAULT,
  BLUR_MAX,
  BLUR_MIN,
  BLUR_STEP,
  CAMERA_EFFECT_UNAVAILABLE_TEXT,
  CAMERA_TEXT,
  EFFECT_REFUSED_TEXT,
  EFFECT_SAVE_TEXT,
  IDLE_CALL,
  NO_VIDEO_EFFECT,
  backgroundOptions,
  backgroundValue,
  blurText,
  blurValue,
  cameraButton,
  filterOptions,
  filterValue,
  plainEffect
} from '../src/calls.ts';

let vite;
let render;
let CallVideoEffects;
let CallWindowDevices;

before(async () => {
  // Its own dependency cache, as in render.test.mjs: test files run in
  // parallel.
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/video-effects',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: CallVideoEffects } = await vite.ssrLoadModule('/src/CallVideoEffects.svelte'));
  ({ default: CallWindowDevices } = await vite.ssrLoadModule('/src/CallWindowDevices.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

test('GIVEN slider values WHEN read as a blur strength THEN they are whole percents from 10 to 100 in steps of 10, and a bad value is the default', () => {
  assert.deepEqual([BLUR_MIN, BLUR_MAX, BLUR_STEP, BLUR_DEFAULT], [10, 100, 10, 50]);
  assert.equal(blurValue(10), 10);
  assert.equal(blurValue(54), 50);
  assert.equal(blurValue(55), 60);
  assert.equal(blurValue(100), 100);
  assert.equal(blurValue(0), 10, 'no blur is not a blur strength');
  assert.equal(blurValue(-30), 10);
  assert.equal(blurValue(250), 100);
  assert.equal(blurValue(Number.NaN), 50);
  assert.equal(blurValue(Number.POSITIVE_INFINITY), 50);
  assert.equal(blurText(70), '70%');
  assert.equal(blurText(Number.NaN), '50%');
});

test('GIVEN no person mask WHEN the background menu is made THEN only none can be chosen, and the saved background stays chosen', () => {
  const choosable = (options) => options.filter((option) => !option.disabled).map((option) => option.value);
  const all = ['none', 'blur', 'dusk', 'sea', 'forest', 'studio'];
  assert.deepEqual(
    backgroundOptions(true, 'none').map((option) => option.value),
    all,
    'every background is in the menu, in order'
  );
  assert.deepEqual(choosable(backgroundOptions(true, 'none')), all);
  assert.deepEqual(choosable(backgroundOptions(false, 'none')), ['none']);
  assert.deepEqual(choosable(backgroundOptions(false, 'sea')), ['none', 'sea'], 'the menu never shows another than the saved');
  assert.deepEqual(
    filterOptions().map((option) => [option.value, option.disabled]),
    [
      ['none', false],
      ['greyscale', false],
      ['warm', false],
      ['cool', false],
      ['brighten', false]
    ],
    'the filters need no mask'
  );
});

test('GIVEN menu values WHEN read THEN only the closed set is taken, also against names that every object has', () => {
  assert.equal(backgroundValue('studio', 'none'), 'studio');
  assert.equal(filterValue('warm', 'none'), 'warm');
  for (const bad of ['moon', 'Blur', '', '__proto__', 'toString', 'constructor']) {
    assert.equal(backgroundValue(bad, 'sea'), 'sea', bad);
    assert.equal(filterValue(bad, 'cool'), 'cool', bad);
  }
});

test('GIVEN an effect with more fields WHEN made plain THEN only the three the host takes are left, and the blur is a valid one', () => {
  assert.deepEqual(plainEffect({ background: 'blur', blur: 33, filter: 'cool', url: 'https://x' }), {
    background: 'blur',
    blur: 30,
    filter: 'cool'
  });
  assert.deepEqual(NO_VIDEO_EFFECT, { background: 'none', blur: 50, filter: 'none' });
});

test('GIVEN a camera that stays off for its effect WHEN shown THEN the call bar says why and offers to turn it on again', () => {
  assert.equal(CAMERA_TEXT.effect_unavailable, CAMERA_EFFECT_UNAVAILABLE_TEXT);
  assert.match(CAMERA_EFFECT_UNAVAILABLE_TEXT, /stays off/);
  assert.equal(cameraButton('effect_unavailable').pressed, false);
  assert.equal(IDLE_CALL.mirror, true, 'the own preview is mirrored by default');
  assert.equal(IDLE_CALL.effectRefused, false);
});

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

const never = () => new Promise(() => {});
const menus = (recording) => ({ recording, playout: '', camera: '' });
const sea = { background: 'sea', blur: 50, filter: 'cool' };

/** A panel with the given bridge, and what it said about the effect. */
function panel(bridge) {
  const effects = [];
  const made = new DevicePanel(
    bridge,
    () => {},
    () => {},
    (state) => effects.push(state)
  );
  return { panel: made, effects, last: () => effects.at(-1) };
}

test('GIVEN a device save that runs WHEN the effect changes THEN the effect is sent after it, with only its three fields', async () => {
  const first = later();
  const order = [];
  const { panel: made, last } = panel({
    list: never,
    save: (...choice) => {
      order.push(['devices', ...choice]);
      return first.promise;
    },
    effect: async (effect, mirror) => {
      order.push(['effect', effect, mirror]);
      return { ...IDLE_CALL, effectRefused: false };
    }
  });
  const devices = made.save(menus('mic-1'));
  const effect = made.saveEffect({ ...sea, extra: 'x' }, false);
  await Promise.resolve();
  assert.deepEqual(order, [['devices', 'mic-1', null, null]], 'the effect waits for the device save');
  assert.equal(last().saving, true);
  first.resolve();
  await Promise.all([devices, effect]);
  assert.deepEqual(order, [
    ['devices', 'mic-1', null, null],
    ['effect', sea, false]
  ]);
  assert.deepEqual(last(), { saving: false, failure: null });
});

test('GIVEN effect answers WHEN the core failed THEN the newest says so, and an older failure is old news; a refusal in a reply is not read', async () => {
  const answers = [later(), later()];
  let calls = 0;
  const { panel: made, last } = panel({
    list: never,
    save: never,
    effect: () => answers[calls++].promise
  });
  const one = made.saveEffect(sea, true);
  const two = made.saveEffect(NO_VIDEO_EFFECT, true);
  answers[0].reject('core_unavailable');
  await Promise.resolve();
  await Promise.resolve();
  // The reply comes before the worker answers, so its refusal is old.
  answers[1].resolve({ ...IDLE_CALL, phase: 'active', effectRefused: true });
  await Promise.all([one, two]);
  assert.deepEqual(last(), { saving: false, failure: null });

  const { panel: failing, last: lastFailing } = panel({
    list: never,
    save: never,
    effect: () => Promise.reject('something_else')
  });
  await failing.saveEffect(sea, false);
  assert.deepEqual(lastFailing(), { saving: false, failure: EFFECT_SAVE_TEXT });
});

test('GIVEN an effect save that runs WHEN a refresh is asked THEN it does not start; WHEN the panel closes THEN a late answer changes nothing', async () => {
  const answer = later();
  let listed = 0;
  const { panel: made, effects } = panel({
    list: async () => {
      listed += 1;
      return null;
    },
    save: never,
    effect: () => answer.promise
  });
  const saving = made.saveEffect(sea, true);
  await made.load();
  assert.equal(listed, 0, 'a refresh would replace the controls under the save');
  made.close();
  const seen = effects.length;
  answer.resolve({ ...IDLE_CALL, effectRefused: true });
  await saving;
  assert.equal(effects.length, seen, 'a closed panel takes no late answer');
});

test('GIVEN no person mask WHEN the controls are rendered THEN blur and the pictures cannot be chosen and a note says why; WHEN blur is saved THEN its strength shows', () => {
  const html = (props) =>
    render(CallVideoEffects, {
      props: { mirror: true, disabled: false, prefix: 'fx', onsave() {}, ...props }
    }).body;
  const plain = html({ effect: NO_VIDEO_EFFECT, available: false });
  assert.match(plain, /Video effects/);
  assert.ok(plain.includes(BACKGROUND_EFFECTS_UNAVAILABLE_TEXT));
  for (const value of ['blur', 'dusk', 'sea', 'forest', 'studio']) {
    assert.match(plain, new RegExp(`<option value="${value}" disabled`), value);
  }
  assert.doesNotMatch(plain, /type="range"/, 'no blur strength without blur');
  assert.match(plain, /Mirror my picture/);

  const able = html({ effect: { background: 'blur', blur: 70, filter: 'warm' }, available: true });
  assert.ok(!able.includes(BACKGROUND_EFFECTS_UNAVAILABLE_TEXT));
  assert.doesNotMatch(able, /<option value="[a-z]+" disabled/);
  assert.match(able, /id="fx-blur"/);
  assert.match(able, /70%/);
});

test('GIVEN the call status WHEN its worker refused the effect THEN the call window panel says so; WHEN not THEN no note shows', () => {
  const html = (props) => render(CallWindowDevices, { props }).body;
  assert.ok(html({ effectRefused: true }).includes(EFFECT_REFUSED_TEXT));
  assert.ok(!html({ effectRefused: false }).includes(EFFECT_REFUSED_TEXT));
  assert.ok(!html({}).includes(EFFECT_REFUSED_TEXT), 'no status, no refusal');
});
