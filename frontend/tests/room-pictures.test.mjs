import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';
import { BAD_IMAGE } from '../src/image.ts';
import {
  MAX_AUTOMATIC_TRIES,
  RoomPictures,
  pictureFailureText,
  validPictureId
} from '../src/roomPictures.ts';

let vite;
let render;
let RoomList;
let Timeline;

before(async () => {
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/room-pictures',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: RoomList } = await vite.ssrLoadModule('/src/RoomList.svelte'));
  ({ default: Timeline } = await vite.ssrLoadModule('/src/Timeline.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

const PICTURE = '0123456789abcdef';
const OTHER = 'fedcba9876543210';
const HANDLE = '00112233445566778899aabbccddeeff';

const view = (side = 2) => ({ handle: HANDLE, width: side, height: side, bands: 1, bandRows: side });

/**
 * A fake core. `answers` maps a room ID to the reply (or a function giving
 * it); an Error-free string rejection stands for a host error code. Records
 * every call, and how many loads run at once.
 */
function fakeCore(answers) {
  const calls = { loads: [], bands: [], released: [], running: 0, most: 0 };
  const gates = [];
  const core = {
    calls,
    gates,
    async load(roomId) {
      calls.loads.push(roomId);
      calls.running += 1;
      calls.most = Math.max(calls.most, calls.running);
      try {
        if (core.gated) await new Promise((resolve) => gates.push(resolve));
        const answer = answers[roomId];
        const value = typeof answer === 'function' ? answer() : answer;
        if (typeof value === 'string') throw value;
        if (value === undefined) throw 'not_found';
        return value;
      } finally {
        calls.running -= 1;
      }
    },
    async band(handle, index) {
      calls.bands.push([handle, index]);
      const width = core.bandWidth ?? 2;
      return { width, rows: 2, pixels: new Uint8Array(width * 2 * 4).fill(7) };
    },
    async release(handle) {
      calls.released.push(handle);
    }
  };
  return core;
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
async function settled(times = 5) {
  for (let i = 0; i < times; i += 1) await settle();
}

test('GIVEN a room picture the user asks for WHEN it loads THEN its bands are drawn into the page and the core copy is released', async () => {
  const core = fakeCore({ '!a:s': { pictureId: PICTURE, image: view() } });
  const pictures = new RoomPictures(core);
  let changes = 0;
  pictures.subscribe(() => (changes += 1));
  assert.equal(pictures.state('!a:s', PICTURE).kind, 'none');

  pictures.request('!a:s', PICTURE);
  assert.equal(pictures.state('!a:s', PICTURE).kind, 'loading');
  await settled();

  const shown = pictures.state('!a:s', PICTURE);
  assert.equal(shown.kind, 'shown');
  assert.equal(shown.width, 2);
  assert.equal(shown.pixels.byteLength, 16);
  assert.deepEqual(core.calls.bands, [[HANDLE, 0]]);
  assert.deepEqual(core.calls.released, [HANDLE]);
  assert.ok(changes >= 2);
  // Another picture ID for the same room is not this picture.
  assert.equal(pictures.state('!a:s', OTHER).kind, 'none');
  assert.equal(pictures.state('!a:s', null).kind, 'none');

  pictures.request('!a:s', PICTURE);
  await settled();
  assert.deepEqual(core.calls.loads, ['!a:s'], 'a shown picture is not loaded again');
});

test('GIVEN automatic loading off WHEN rooms are listed THEN no picture is fetched', async () => {
  const core = fakeCore({ '!a:s': { pictureId: PICTURE, image: view() } });
  const pictures = new RoomPictures(core);
  pictures.sync([{ roomId: '!a:s', pictureId: PICTURE }, { roomId: '!b:s', pictureId: null }], false);
  await settled();
  assert.deepEqual(core.calls.loads, []);
  assert.equal(pictures.size, 0);
});

test('GIVEN automatic loading on WHEN rooms are listed THEN each picture loads, one at a time, and rooms without one are skipped', async () => {
  const core = fakeCore({
    '!a:s': { pictureId: PICTURE, image: view() },
    '!b:s': { pictureId: OTHER, image: view() }
  });
  const pictures = new RoomPictures(core);
  pictures.sync(
    [
      { roomId: '!a:s', pictureId: PICTURE },
      { roomId: '!none:s', pictureId: null },
      { roomId: '!b:s', pictureId: OTHER }
    ],
    true
  );
  await settled(10);
  assert.deepEqual(core.calls.loads, ['!a:s', '!b:s']);
  assert.equal(core.calls.most, 1, 'never two loads at once');
  assert.equal(pictures.state('!a:s', PICTURE).kind, 'shown');
  assert.equal(pictures.state('!b:s', OTHER).kind, 'shown');
});

test('GIVEN a picture the core refuses WHEN asked again THEN it is not fetched again', async () => {
  for (const code of ['rejected', 'not_found']) {
    const core = fakeCore({ '!a:s': code });
    const pictures = new RoomPictures(core);
    pictures.request('!a:s', PICTURE);
    await settled();
    const failed = pictures.state('!a:s', PICTURE);
    assert.deepEqual(failed, { kind: 'failed', reason: code, retry: false }, code);
    pictures.request('!a:s', PICTURE);
    pictures.sync([{ roomId: '!a:s', pictureId: PICTURE }], true);
    await settled();
    assert.equal(core.calls.loads.length, 1, code);
  }
});

test('GIVEN a picture that failed for now WHEN listed again THEN automatic loading tries a bounded number of times, and the user can always try again', async () => {
  const core = fakeCore({ '!a:s': 'unavailable' });
  const pictures = new RoomPictures(core);
  const listed = [{ roomId: '!a:s', pictureId: PICTURE }];
  for (let i = 0; i < MAX_AUTOMATIC_TRIES + 2; i += 1) {
    pictures.sync(listed, true);
    await settled();
  }
  assert.equal(core.calls.loads.length, MAX_AUTOMATIC_TRIES);
  assert.deepEqual(pictures.state('!a:s', PICTURE), { kind: 'failed', reason: 'unavailable', retry: true });
  pictures.request('!a:s', PICTURE);
  await settled();
  assert.equal(core.calls.loads.length, MAX_AUTOMATIC_TRIES + 1);
});

test('GIVEN a view outside the picture limits WHEN loaded THEN nothing is drawn and the handle is released', async () => {
  for (const image of [view(97), { ...view(), bands: 2 }, { ...view(), handle: 'x' }]) {
    const core = fakeCore({ '!a:s': { pictureId: PICTURE, image } });
    const pictures = new RoomPictures(core);
    pictures.request('!a:s', PICTURE);
    await settled();
    assert.deepEqual(pictures.state('!a:s', PICTURE), { kind: 'failed', reason: BAD_IMAGE, retry: false });
    assert.deepEqual(core.calls.bands, [], 'no band is fetched');
    assert.deepEqual(core.calls.released, [image.handle]);
  }
  const core = fakeCore({ '!a:s': { pictureId: PICTURE, image: view() } });
  core.bandWidth = 3;
  const pictures = new RoomPictures(core);
  pictures.request('!a:s', PICTURE);
  await settled();
  assert.equal(pictures.state('!a:s', PICTURE).reason, BAD_IMAGE, 'a band of the wrong width');
  assert.deepEqual(core.calls.released, [HANDLE]);
});

test('GIVEN the picture changed in the core first WHEN the reply names another picture THEN it is not shown as the asked one', async () => {
  const core = fakeCore({ '!a:s': { pictureId: OTHER, image: view() } });
  const pictures = new RoomPictures(core);
  pictures.request('!a:s', PICTURE);
  await settled();
  assert.deepEqual(pictures.state('!a:s', PICTURE), { kind: 'failed', reason: 'changed', retry: true });
  assert.equal(pictures.state('!a:s', OTHER).kind, 'none');
  assert.deepEqual(core.calls.bands, []);
  assert.deepEqual(core.calls.released, [HANDLE]);
});

test('GIVEN a room whose picture changed or was removed WHEN rooms are listed THEN its old picture is dropped', async () => {
  const core = fakeCore({ '!a:s': { pictureId: PICTURE, image: view() }, '!b:s': { pictureId: OTHER, image: view() } });
  const pictures = new RoomPictures(core);
  pictures.request('!a:s', PICTURE);
  pictures.request('!b:s', OTHER);
  await settled(10);
  assert.equal(pictures.size, 2);
  pictures.sync([{ roomId: '!a:s', pictureId: OTHER }], false);
  assert.equal(pictures.state('!a:s', PICTURE).kind, 'none');
  assert.equal(pictures.state('!a:s', OTHER).kind, 'none', 'the new picture loads only when asked');
  assert.equal(pictures.size, 0, 'a room no longer listed keeps nothing');
});

test('GIVEN automatic loading turned off WHEN pictures wait their turn THEN those not yet started and not asked for are dropped', async () => {
  const core = fakeCore({ '!a:s': { pictureId: PICTURE, image: view() }, '!b:s': { pictureId: OTHER, image: view() } });
  core.gated = true;
  const pictures = new RoomPictures(core);
  const listed = [
    { roomId: '!a:s', pictureId: PICTURE },
    { roomId: '!b:s', pictureId: OTHER }
  ];
  pictures.sync(listed, true);
  await settled();
  pictures.sync(listed, false);
  core.gated = false;
  core.gates.forEach((open) => open());
  await settled(10);
  assert.deepEqual(core.calls.loads, ['!a:s'], 'the waiting picture was never fetched');
  assert.equal(pictures.state('!a:s', PICTURE).kind, 'shown', 'the one already loading finishes');
});

test('GIVEN a session change WHEN a picture is still loading THEN nothing from it is kept and its handle is released', async () => {
  const core = fakeCore({ '!a:s': { pictureId: PICTURE, image: view() } });
  core.gated = true;
  const pictures = new RoomPictures(core);
  pictures.request('!a:s', PICTURE);
  await settled();
  pictures.clear();
  core.gates.forEach((open) => open());
  await settled();
  assert.equal(pictures.size, 0);
  assert.equal(pictures.state('!a:s', PICTURE).kind, 'none');
  assert.deepEqual(core.calls.bands, [], 'no band of the old session is fetched');
  assert.deepEqual(core.calls.released, [HANDLE]);
});

test('GIVEN more rooms than the limit WHEN all are listed with automatic loading THEN at most the limit is kept', async () => {
  const answers = {};
  const listed = [];
  for (let i = 0; i < 5; i += 1) {
    const id = `${i}`.repeat(16).slice(0, 16);
    answers[`!${i}:s`] = { pictureId: id, image: view() };
    listed.push({ roomId: `!${i}:s`, pictureId: id });
  }
  const core = fakeCore(answers);
  const pictures = new RoomPictures(core, 2);
  pictures.sync(listed, true);
  await settled(20);
  assert.equal(core.calls.loads.length, 2);
  assert.ok(pictures.size <= 2);
});

test('GIVEN a picture ID outside the contract WHEN asked for THEN nothing is fetched', async () => {
  const core = fakeCore({});
  const pictures = new RoomPictures(core);
  for (const id of ['', 'ABCDEF0123456789', '0123456789abcde', '0123456789abcdef0', 'mxc://s/m', null]) {
    assert.equal(validPictureId(id), false, String(id));
    pictures.request('!a:s', id);
  }
  await settled();
  assert.deepEqual(core.calls.loads, []);
  assert.equal(validPictureId(PICTURE), true);
});

test('GIVEN picture error codes WHEN shown THEN each is a short fixed text', () => {
  for (const code of ['rejected', 'not_found', 'limit_exceeded', 'locked', 'changed', BAD_IMAGE, 'unavailable', '<b>x</b>']) {
    const text = pictureFailureText(code);
    assert.ok(text.length > 0 && text.length < 120, code);
    assert.ok(!text.includes('<'), code);
  }
});

const room = (pictureId) => ({
  roomId: '!r:s',
  displayName: 'Room',
  unreadCount: 0,
  encryption: 'encrypted',
  contact: null,
  pictureId
});
const shownPictures = {
  state: () => ({ kind: 'shown', width: 96, height: 96, pixels: new Uint8ClampedArray(96 * 96 * 4) }),
  subscribe: () => () => {}
};
const emptyPictures = { state: () => ({ kind: 'none' }), subscribe: () => () => {} };

test('GIVEN a room with a picture WHEN the list shows THEN the usual icon stays until the picture is loaded, then a decorative canvas replaces it', () => {
  const listed = (pictures) =>
    render(RoomList, {
      props: { rooms: [room(PICTURE)], truncated: false, selected: null, onselect: () => {}, pictures }
    }).body;
  const before = listed(emptyPictures);
  assert.doesNotMatch(before, /<canvas/);
  assert.match(before, /class="[^"]*hash/);
  assert.doesNotMatch(before, /Load the room picture/, 'the list never offers loading');

  const after = listed(shownPictures);
  assert.match(after, /<canvas class="room-picture small" width="96" height="96" aria-hidden="true">/);
  assert.doesNotMatch(after, /mxc:|https?:|<img/);
});

test('GIVEN the timeline header WHEN the room has a picture not loaded THEN a labelled load button shows only where loading on request is offered', () => {
  const header = (props) =>
    render(Timeline, {
      props: {
        title: 'Room',
        roomId: '!r:s',
        items: [],
        hasOlder: false,
        loading: false,
        error: null,
        onolder: () => {},
        ...props
      }
    }).body;
  assert.match(
    header({ pictureId: PICTURE, pictures: emptyPictures, onloadpicture: () => {} }),
    /Load the room picture/
  );
  assert.doesNotMatch(header({ pictureId: PICTURE, pictures: emptyPictures }), /Load the room picture/);
  assert.doesNotMatch(header({ pictureId: null, pictures: emptyPictures, onloadpicture: () => {} }), /Load the room picture/);
  const shown = header({ pictureId: PICTURE, pictures: shownPictures, onloadpicture: () => {} });
  assert.match(shown, /<canvas class="room-picture large"/);
  assert.doesNotMatch(shown, /Load the room picture/);
});

test('GIVEN the picture sources WHEN read THEN no URL, blob, data URL or <img> is used for room pictures', async () => {
  for (const file of ['RoomPicture.svelte', 'roomPictures.ts']) {
    const source = await readFile(new URL(`../src/${file}`, import.meta.url), 'utf8');
    assert.doesNotMatch(source, /<img|blob:|data:|createObjectURL|fetch\(|mxc:/, file);
  }
});
