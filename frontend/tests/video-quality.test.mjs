import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';
import { DevicePanel } from '../src/callDevicePanel.ts';
import {
  DEVICES_NOTE_TEXT,
  DEVICE_SAVED_TEXT,
  DEVICE_FAILURE_TEXT,
  NO_VIDEO_EFFECT,
  VIDEO_QUALITY_HELP,
  VIDEO_QUALITY_OPTIONS,
  VIDEO_QUALITY_SAVE_TEXT,
  videoQualityValue,
  withVideoQuality
} from '../src/calls.ts';

let vite;
let render;
let CallVideoQuality;
let CallWindowDevices;
let CallDevices;

before(async () => {
  // Its own dependency cache, as in render.test.mjs: test files run in
  // parallel.
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/video-quality',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: CallVideoQuality } = await vite.ssrLoadModule('/src/CallVideoQuality.svelte'));
  ({ default: CallWindowDevices } = await vite.ssrLoadModule('/src/CallWindowDevices.svelte'));
  ({ default: CallDevices } = await vite.ssrLoadModule('/src/CallDevices.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

const DEVICES = {
  supported: true,
  recording: [],
  playout: [],
  cameras: [{ id: 'cam-1', name: 'Camera' }],
  selectedRecording: null,
  selectedPlayout: null,
  selectedCamera: 'cam-1',
  volume: 100,
  autoLevel: true,
  micGain: 100,
  effect: NO_VIDEO_EFFECT,
  mirror: true,
  videoQuality: 'auto',
  backgroundEffects: false,
  unreadable: false
};

/** A panel that tells what it showed about the send resolution. */
function panel(bridge) {
  const seen = { quality: [], back: [] };
  const made = new DevicePanel(
    { list: async () => DEVICES, save: async () => {}, effect: async () => {}, ...bridge },
    () => {},
    () => {},
    () => {},
    () => {},
    (state) => seen.quality.push(state),
    (saved) => seen.back.push(saved)
  );
  return { panel: made, seen };
}

test('GIVEN the menu WHEN read THEN Auto comes first, then 1440p down to 480p, and any other value is Auto', () => {
  assert.deepEqual(
    VIDEO_QUALITY_OPTIONS.map((option) => option.value),
    ['auto', '1440p', '1080p', '720p', '480p']
  );
  assert.equal(VIDEO_QUALITY_OPTIONS[0].label, 'Auto (best for the connection)');
  for (const { value } of VIDEO_QUALITY_OPTIONS) assert.equal(videoQualityValue(value), value);
  for (const bad of ['4k', '360p', 'Auto', '', null, undefined, 720, {}]) {
    assert.equal(videoQualityValue(bad), 'auto', String(bad));
  }
  assert.equal(withVideoQuality(DEVICES, '720p').videoQuality, '720p');
  assert.equal(DEVICES.videoQuality, 'auto', 'the old record is not changed');
});

test('GIVEN the camera texts WHEN read THEN a camera change applies at once and none says it waits for the next start', () => {
  for (const text of [DEVICES_NOTE_TEXT, DEVICE_SAVED_TEXT]) {
    assert.match(text, /camera change restarts a camera that is on/);
    assert.doesNotMatch(text, /next camera start|next time you turn the camera on/);
  }
});

test('GIVEN the select WHEN rendered THEN it has the label, the five choices, the saved one chosen and the hint', () => {
  const html = (props) =>
    render(CallVideoQuality, { props: { disabled: false, prefix: 'q', onsave() {}, ...props } }).body;
  const body = html({ quality: '1080p' });
  assert.match(body, /<label for="q-quality">Video quality<\/label>/);
  assert.match(body, /id="q-quality"/);
  for (const { value, label } of VIDEO_QUALITY_OPTIONS) assert.ok(body.includes(`>${label}</option>`), value);
  assert.match(body, /<option value="1080p" selected/);
  assert.ok(body.includes(VIDEO_QUALITY_HELP));
  assert.equal(
    VIDEO_QUALITY_HELP,
    'Your camera and computer limit the highest resolution. Auto changes it with the connection.'
  );
  assert.match(html({ quality: 'auto', disabled: true }), /<select[^>]* disabled/);
});

test('GIVEN the two windows WHEN rendered THEN each has the select with its own ID and the hint', () => {
  const window = render(CallWindowDevices, { props: {} }).body;
  assert.match(window, /id="call-window-quality"/);
  assert.ok(window.includes(VIDEO_QUALITY_HELP));
  const settings = render(CallDevices, { props: {} }).body;
  assert.match(settings, /id="call-quality"/);
  assert.ok(settings.includes(VIDEO_QUALITY_HELP));
});

test('GIVEN a change WHEN the core accepts it THEN the panel sends the value and keeps it in its copy', async () => {
  const sent = [];
  const { panel: made, seen } = panel({ quality: async (quality) => sent.push(quality) });
  await made.load();
  await made.saveQuality('720p');
  assert.deepEqual(sent, ['720p']);
  assert.equal(made.state.devices.videoQuality, '720p');
  assert.deepEqual(seen.quality, [
    { saving: true, failure: null },
    { saving: false, failure: null }
  ]);
  assert.deepEqual(seen.back, []);
});

test('GIVEN a change WHEN the core refuses it THEN the panel shows the failure and puts the menu back to the saved value', async () => {
  for (const [reason, text] of [
    ['locked', DEVICE_FAILURE_TEXT.locked],
    ['unknown', VIDEO_QUALITY_SAVE_TEXT]
  ]) {
    const { panel: made, seen } = panel({ quality: () => Promise.reject(reason) });
    await made.load();
    await made.saveQuality('1440p');
    assert.deepEqual(seen.quality.at(-1), { saving: false, failure: text }, reason);
    assert.deepEqual(seen.back, ['auto'], 'the saved value, not the refused one');
    assert.equal(made.state.devices.videoQuality, 'auto');
  }
});

test('GIVEN a save that runs WHEN the quality changes twice THEN the changes go after it in order and only the newest sets the result', async () => {
  let release;
  const gate = new Promise((resolve) => (release = resolve));
  const order = [];
  const { panel: made, seen } = panel({
    save: () => gate.then(() => order.push('devices')),
    quality: async (quality) => {
      order.push(quality);
      if (quality === '1080p') throw 'invalid_argument';
    }
  });
  await made.load();
  const save = made.save({ recording: '', playout: '', camera: 'cam-1' });
  const first = made.saveQuality('1080p');
  const second = made.saveQuality('480p');
  release();
  await Promise.all([save, first, second]);
  assert.deepEqual(order, ['devices', '1080p', '480p'], 'one at a time, in the order asked');
  assert.deepEqual(seen.quality.at(-1), { saving: false, failure: null }, 'the older failure is not the result');
  assert.deepEqual(seen.back, [], 'only the newest change puts the menu back');
  assert.equal(made.state.devices.videoQuality, '480p');
});

test('GIVEN a closed panel WHEN an answer comes THEN it changes nothing, and a closed panel sends nothing', async () => {
  let release;
  const gate = new Promise((resolve) => (release = resolve));
  const sent = [];
  const { panel: made, seen } = panel({
    quality: (quality) => {
      sent.push(quality);
      return gate;
    }
  });
  await made.load();
  const pending = made.saveQuality('720p');
  made.close();
  release();
  await pending;
  assert.equal(seen.quality.length, 1, 'only the start was shown');
  await made.saveQuality('480p');
  assert.deepEqual(sent, ['720p']);
});
