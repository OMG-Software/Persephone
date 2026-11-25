import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';
import {
  BAD_VIDEO,
  VIDEO_PREFIX_BYTES,
  VideoFetcher,
  SCREEN_TILE,
  VIDEO_TILES,
  maxPixels,
  maxSide,
  parseVideoPicture,
  tileCanvas,
  tileLayout
} from '../src/video.ts';

// GIVEN a canvas registered under a tile WHEN its cell moves to another
// tile THEN the registration moves with it; WHEN another canvas took the old
// tile first THEN that canvas stays; WHEN destroyed THEN only its own tile
// is cleared.
test('a tile canvas follows its tile and clears only its own registration', () => {
  const canvases = {};
  const first = { name: 'first' };
  const action = tileCanvas(canvases, first, 2);
  assert.equal(canvases[2], first);
  action.update(2);
  assert.equal(canvases[2], first);
  action.update(1);
  assert.equal(canvases[1], first);
  assert.equal(canvases[2], undefined);

  const second = { name: 'second' };
  const other = tileCanvas(canvases, second, 3);
  action.update(3);
  assert.equal(canvases[3], first);
  other.update(4);
  assert.equal(canvases[3], first, 'the moved canvas keeps tile 3');
  assert.equal(canvases[4], second);
  other.destroy();
  assert.equal(canvases[4], undefined);
  canvases[1] = second;
  action.destroy();
  assert.equal(canvases[3], undefined);
  assert.equal(canvases[1], second, 'a tile another canvas holds stays');
});

let vite;
let CallVideo;
let CallBar;
let render;

before(async () => {
  // Each test file keeps its own Vite cache, so parallel files do not
  // invalidate one another's pre-bundles.
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/video',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: CallVideo } = await vite.ssrLoadModule('/src/CallVideo.svelte'));
  ({ default: CallBar } = await vite.ssrLoadModule('/src/CallBar.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

/** A raw `call_video_frame` response, as the host builds it. */
function raw({ seq = 1n, rotation = 0, width = 2, height = 1, length = width * height * 4, fill = 7 } = {}) {
  const body = new Uint8Array(VIDEO_PREFIX_BYTES + length).fill(fill);
  const prefix = new DataView(body.buffer);
  prefix.setBigUint64(0, BigInt(seq), true);
  prefix.setUint32(8, rotation, true);
  prefix.setUint32(12, width, true);
  prefix.setUint32(16, height, true);
  return body;
}

const picture = (seq) => ({ seq, rotation: 0, width: 1, height: 1, pixels: new Uint8ClampedArray(4) });
const later = () => new Promise((done) => setImmediate(done));

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((ok, no) => {
    resolve = ok;
    reject = no;
  });
  return { promise, resolve, reject };
}

test('GIVEN an empty body WHEN it is parsed THEN there is no newer picture', () => {
  assert.equal(parseVideoPicture(new ArrayBuffer(0), 0), null);
  assert.equal(parseVideoPicture(new Uint8Array(0), 0), null);
});

test('GIVEN a whole picture WHEN it is parsed THEN the numbers and exactly its pixels come back', () => {
  const body = raw({ seq: 9007199254740991n, rotation: 3, width: 2, height: 1 });
  for (const input of [body, body.buffer, new DataView(body.buffer)]) {
    const parsed = parseVideoPicture(input, 0);
    assert.equal(parsed.seq, Number.MAX_SAFE_INTEGER);
    assert.deepEqual([parsed.rotation, parsed.width, parsed.height], [3, 2, 1]);
    assert.equal(parsed.pixels.length, 8);
    assert.deepEqual([...parsed.pixels], Array(8).fill(7));
  }
  // The largest pictures: 640 by 360, and the same turned.
  assert.equal(parseVideoPicture(raw({ width: 640, height: 360 }), 0).pixels.length, 640 * 360 * 4);
  assert.equal(parseVideoPicture(raw({ width: 360, height: 640, rotation: 1 }), 0).height, 640);
});

test('GIVEN a picture read from the middle of a larger buffer WHEN it is parsed THEN offsets are kept', () => {
  const body = raw({ width: 1, height: 1 });
  const outer = new Uint8Array(body.length + 10);
  outer.set(body, 6);
  const parsed = parseVideoPicture(outer.subarray(6, 6 + body.length), 0);
  assert.equal(parsed.pixels.byteOffset, 6 + VIDEO_PREFIX_BYTES);
  assert.deepEqual([...parsed.pixels], [7, 7, 7, 7]);
});

test('GIVEN a body outside the contract WHEN it is parsed THEN it is refused', () => {
  const good = { width: 2, height: 1 };
  const bad = [
    ['not bytes', 'text'],
    ['null', null],
    ['an object', { byteLength: 0 }],
    ['a short prefix', new Uint8Array(19)],
    ['the prefix alone', raw({ ...good, length: 0 })],
    ['pixels missing a byte', raw({ ...good, length: 7 })],
    ['a byte too many', raw({ ...good, length: 9 })],
    ['rotation 4', raw({ ...good, rotation: 4 })],
    ['rotation far out', raw({ ...good, rotation: 0xffffffff })],
    ['width 0', raw({ width: 0, height: 1, length: 0 })],
    ['height 0', raw({ width: 1, height: 0, length: 0 })],
    ['width 641', raw({ width: 641, height: 1 })],
    ['height 641', raw({ width: 1, height: 641 })],
    ['over 230400 pixels', raw({ width: 640, height: 361 })],
    ['a screen size on a camera tile', raw({ width: 1280, height: 720, length: 8 })],
    ['seq above the safe integers', raw({ ...good, seq: 9007199254740992n })],
    ['seq at the top', raw({ ...good, seq: 0xffffffffffffffffn })]
  ];
  for (const [name, input] of bad) {
    assert.throws(() => parseVideoPicture(input, 0), (reason) => reason === BAD_VIDEO, name);
  }
});

test('GIVEN tiles set WHEN no request runs THEN each tile is asked once, after its own last number', async () => {
  const asked = [];
  const fetcher = new VideoFetcher(
    async (tile, after) => {
      asked.push([tile, after]);
      return null;
    },
    () => {}
  );
  fetcher.setTiles([{ tile: 0, key: 'own', stream: 0 }, { tile: 2, key: 'a', stream: 0 }]);
  await later();
  assert.deepEqual(asked.sort(), [[0, 0], [2, 0]]);
});

test('GIVEN a request in flight WHEN pokes arrive THEN at most one runs per tile and one more follows', async () => {
  const gates = [];
  let running = 0;
  let most = 0;
  const shown = [];
  const fetcher = new VideoFetcher(
    (tile, after) => {
      running += 1;
      most = Math.max(most, running);
      const gate = deferred();
      gates.push({ tile, after, gate });
      return gate.promise.finally(() => (running -= 1));
    },
    (tile, next) => shown.push([tile, next.seq])
  );
  fetcher.setTiles([{ tile: 1, key: 'a', stream: 0 }]);
  fetcher.poke();
  fetcher.poke();
  fetcher.poke();
  assert.equal(gates.length, 1);
  gates[0].gate.resolve(picture(4));
  await later();
  // The pokes during the request make exactly one more request, after seq 4.
  assert.equal(gates.length, 2);
  assert.equal(gates[1].after, 4);
  gates[1].gate.resolve(null);
  await later();
  assert.equal(gates.length, 2);
  assert.deepEqual(shown, [[1, 4]]);
  assert.equal(most, 1);
});

test('GIVEN two tiles WHEN one is slow THEN the other is not held back', async () => {
  const gates = new Map();
  const shown = [];
  const fetcher = new VideoFetcher(
    (tile) => {
      const gate = deferred();
      gates.set(tile, gate);
      return gate.promise;
    },
    (tile, next) => shown.push([tile, next.seq])
  );
  fetcher.setTiles([{ tile: 0, key: 'own', stream: 0 }, { tile: 1, key: 'a', stream: 0 }]);
  gates.get(1).resolve(picture(2));
  await later();
  assert.deepEqual(shown, [[1, 2]]);
  gates.get(0).resolve(picture(3));
  await later();
  assert.deepEqual(shown.sort(), [[0, 3], [1, 2]]);
});

test('GIVEN a picture that is not newer WHEN it arrives THEN it is ignored and the number does not go back', async () => {
  const replies = [picture(5), picture(5), picture(3), picture(6)];
  const shown = [];
  const asked = [];
  const fetcher = new VideoFetcher(
    async (tile, after) => {
      asked.push(after);
      return replies.shift() ?? null;
    },
    (tile, next) => shown.push(next.seq)
  );
  fetcher.setTiles([{ tile: 0, key: 'own', stream: 0 }]);
  await later();
  for (let i = 0; i < 3; i += 1) {
    fetcher.poke();
    await later();
  }
  assert.deepEqual(shown, [5, 6]);
  assert.deepEqual(asked, [0, 5, 5, 5]);
});

test('GIVEN a failed request WHEN the next poke comes THEN the picture stays and the tile asks again', async () => {
  let fail = true;
  const shown = [];
  const fetcher = new VideoFetcher(
    async () => {
      if (fail) throw BAD_VIDEO;
      return picture(1);
    },
    (tile, next) => shown.push(next.seq)
  );
  fetcher.setTiles([{ tile: 0, key: 'own', stream: 0 }]);
  await later();
  assert.deepEqual(shown, []);
  fail = false;
  fetcher.poke();
  await later();
  assert.deepEqual(shown, [1]);
});

test('GIVEN a tile that disappears WHEN its reply arrives THEN it is dropped and its state is cleared', async () => {
  const gates = new Map();
  const shown = [];
  const cleared = [];
  const fetcher = new VideoFetcher(
    (tile) => {
      const gate = deferred();
      gates.set(tile, gate);
      return gate.promise;
    },
    (tile, next) => shown.push([tile, next.seq]),
    (tile) => cleared.push(tile)
  );
  fetcher.setTiles([{ tile: 0, key: 'own', stream: 0 }, { tile: 3, key: 'a', stream: 0 }]);
  fetcher.setTiles([{ tile: 0, key: 'own', stream: 0 }]);
  assert.deepEqual(cleared, [3]);
  assert.deepEqual(fetcher.active, [0]);
  gates.get(3).resolve(picture(8));
  gates.get(0).resolve(picture(9));
  await later();
  assert.deepEqual(shown, [[0, 9]], 'only the tile still wanted shows');
});

test('GIVEN another device taking a tile WHEN the old reply arrives last THEN it is dropped and the new picture stays', async () => {
  const gates = [];
  const shown = [];
  const asked = [];
  const fetcher = new VideoFetcher(
    (tile, after) => {
      asked.push(after);
      const gate = deferred();
      gates.push(gate);
      return gate.promise;
    },
    (tile, next) => shown.push(next.seq)
  );
  fetcher.setTiles([{ tile: 1, key: 'a', stream: 0 }]);
  fetcher.setTiles([{ tile: 1, key: 'b', stream: 0 }]);
  assert.equal(gates.length, 1, 'the tile keeps its one request');
  gates[0].resolve(picture(50));
  await later();
  assert.equal(gates.length, 2, 'the new device is asked when the old reply is in');
  gates[1].resolve(picture(2));
  await later();
  assert.deepEqual(asked, [0, 0], 'the new device starts again from 0');
  assert.deepEqual(shown, [2], 'the old picture is not shown');
});

test('GIVEN a tile whose stream number changes WHEN the old reply arrives last THEN it is dropped and the tile asks for the new stream', async () => {
  const gates = [];
  const shown = [];
  const asked = [];
  const cleared = [];
  const fetcher = new VideoFetcher(
    (tile, after, stream) => {
      asked.push([tile, after, stream]);
      const gate = deferred();
      gates.push(gate);
      return gate.promise;
    },
    (tile, next) => shown.push(next.seq),
    (tile) => cleared.push(tile)
  );
  fetcher.setTiles([{ tile: 1, key: 'a', stream: 1 }]);
  fetcher.setTiles([{ tile: 1, key: 'a', stream: 1 }]);
  assert.equal(gates.length, 1, 'an unchanged tile keeps its request');
  fetcher.setTiles([{ tile: 1, key: 'a', stream: 2 }]);
  assert.deepEqual(cleared, [1]);
  assert.deepEqual(asked, [[1, 0, 1]], 'no second request while the first runs');
  gates[0].resolve(picture(9));
  await later();
  assert.deepEqual(asked, [[1, 0, 1], [1, 0, 2]], 'the new stream from 0, at once');
  gates[1].resolve(picture(4));
  await later();
  assert.deepEqual(shown, [4], 'the old stream picture is not shown');
});

test('GIVEN five tiles each with a request at the host WHEN streams change and tiles go and come back THEN there are never more than five requests', async () => {
  const pending = [];
  let most = 0;
  const shown = [];
  const fetcher = new VideoFetcher(
    (tile, after, stream) => {
      const gate = deferred();
      pending.push({ tile, stream, gate });
      most = Math.max(most, pending.length);
      return gate.promise.finally(() => pending.splice(pending.findIndex((entry) => entry.gate === gate), 1));
    },
    (tile, next) => shown.push([tile, next.seq])
  );
  const tiles = (stream) => [0, 1, 2, 3, 4].map((tile) => ({ tile, key: 'k', stream }));
  fetcher.setTiles(tiles(1));
  fetcher.setTiles(tiles(2));
  fetcher.setTiles([]);
  fetcher.setTiles(tiles(3));
  assert.equal(pending.length, 5);
  for (const { gate } of [...pending]) gate.resolve(picture(7));
  await later();
  assert.deepEqual(shown, [], 'no picture of an old stream shows');
  assert.equal(pending.length, 5, 'each tile asks once for its new stream');
  assert.ok(pending.every((entry) => entry.stream === 3));
  for (const { gate } of [...pending]) gate.resolve(picture(1));
  await later();
  assert.equal(shown.length, 5);
  assert.equal(most, 5);
  assert.deepEqual(fetcher.active, [0, 1, 2, 3, 4]);
});

test('GIVEN requests at the host WHEN the page unmounts and mounts again THEN the new tiles wait for them and the new page shows', async () => {
  const pending = [];
  const first = [];
  const second = [];
  const fetcher = new VideoFetcher(
    (tile, after, stream) => {
      const gate = deferred();
      pending.push({ tile, stream, gate });
      return gate.promise.finally(() => pending.splice(pending.findIndex((entry) => entry.gate === gate), 1));
    },
    () => {}
  );
  const tiles = (stream) => [0, 1, 2, 3, 4].map((tile) => ({ tile, key: 'k', stream }));
  fetcher.attach((tile, next) => first.push([tile, next.seq]));
  fetcher.setTiles(tiles(1));
  fetcher.detach();
  assert.deepEqual(fetcher.active, []);
  fetcher.attach((tile, next) => second.push([tile, next.seq]));
  fetcher.setTiles(tiles(2));
  assert.equal(pending.length, 5, 'no request beyond the five at the host');
  for (const { gate } of [...pending]) gate.resolve(picture(3));
  await later();
  assert.deepEqual(first, [], 'the old page shows nothing');
  assert.deepEqual(second, [], 'no picture of the old stream shows');
  assert.equal(pending.length, 5, 'each tile asks once for its new stream');
  assert.ok(pending.every((entry) => entry.stream === 2));
  for (const { gate } of [...pending]) gate.resolve(picture(4));
  await later();
  assert.equal(second.length, 5);
});

test('GIVEN a stream number that is not a safe non-negative integer WHEN set THEN the tile is ignored', async () => {
  const asked = [];
  const fetcher = new VideoFetcher(
    async (tile, after, stream) => {
      asked.push([tile, stream]);
      return null;
    },
    () => {}
  );
  fetcher.setTiles([
    { tile: 0, key: 'own', stream: -1 },
    { tile: 1, key: 'a', stream: 1.5 },
    { tile: 2, key: 'b', stream: Number.MAX_SAFE_INTEGER + 1 },
    { tile: 3, key: 'c', stream: Number.MAX_SAFE_INTEGER }
  ]);
  await later();
  assert.deepEqual(asked, [[3, Number.MAX_SAFE_INTEGER]]);
});

test('GIVEN a stopped fetcher WHEN a reply or a poke arrives THEN nothing is shown or asked', async () => {
  const gate = deferred();
  const shown = [];
  let asked = 0;
  const fetcher = new VideoFetcher(
    () => {
      asked += 1;
      return gate.promise;
    },
    (tile, next) => shown.push(next.seq)
  );
  fetcher.setTiles([{ tile: 0, key: 'own', stream: 0 }]);
  fetcher.stop();
  fetcher.poke();
  gate.resolve(picture(1));
  await later();
  assert.deepEqual(shown, []);
  assert.equal(asked, 1);
  assert.deepEqual(fetcher.active, []);
});

test('GIVEN tiles outside 0 to 5 or repeated WHEN set THEN they are ignored', async () => {
  const asked = [];
  const fetcher = new VideoFetcher(
    async (tile) => {
      asked.push(tile);
      return null;
    },
    () => {}
  );
  fetcher.setTiles([
    { tile: 6, key: 'x', stream: 0 },
    { tile: -1, key: 'x', stream: 0 },
    { tile: 1.5, key: 'x', stream: 0 },
    { tile: 4, key: 'a', stream: 0 },
    { tile: 4, key: 'b', stream: 0 }
  ]);
  await later();
  assert.deepEqual(asked, [4]);
});

test('GIVEN rotations and the own tile WHEN the layout is made THEN a quarter turn swaps the sides and only the own tile mirrors', () => {
  assert.deepEqual(tileLayout(640, 360, 0, false), {
    ratio: 640 / 360,
    canvasPercent: 100,
    transform: 'translate(-50%, -50%) rotate(0deg)'
  });
  const quarter = tileLayout(640, 360, 1, false);
  assert.equal(quarter.ratio, 360 / 640);
  assert.equal(quarter.canvasPercent, (100 * 640) / 360);
  assert.equal(quarter.transform, 'translate(-50%, -50%) rotate(90deg)');
  assert.equal(tileLayout(640, 360, 2, false).ratio, 640 / 360);
  assert.equal(tileLayout(640, 360, 3, false).transform, 'translate(-50%, -50%) rotate(270deg)');
  assert.equal(tileLayout(640, 360, 0, true).transform, 'translate(-50%, -50%) scaleX(-1) rotate(0deg)');
});

test('GIVEN video tiles with hostile names WHEN rendered THEN names are inert text and the own tile says You', () => {
  const hostile = '<img src=x onerror=alert(1)>';
  const { body: html } = render(CallVideo, {
    props: {
      tiles: [
        { tile: 2, participant: { userId: '@bob:example.invalid', deviceId: 'D' }, own: false },
        { tile: 0, participant: null, own: true }
      ],
      names: new Map([['@bob:example.invalid', hostile]]),
      ticks: 0
    }
  });
  assert.match(html, /&lt;img src=x onerror=alert\(1\)>/);
  assert.doesNotMatch(html, /<img\b/i);
  assert.match(html, /You/);
  assert.ok(html.indexOf('You') < html.indexOf('@bob'), 'tiles show in tile order');
  assert.equal((html.match(/<canvas/g) ?? []).length, 2);
});

test('GIVEN no tiles WHEN rendered THEN nothing shows', () => {
  const { body: html } = render(CallVideo, { props: { tiles: [], names: new Map(), ticks: 0 } });
  assert.doesNotMatch(html, /<canvas|call-video/);
});

test('GIVEN each camera state WHEN the call bar is rendered THEN failures say so in plain text and working states do not', () => {
  const base = {
    phase: 'active',
    roomId: '!r:s',
    muted: false,
    deafened: false,
    volume: 100,
    participants: [],
    ended: null,
    joinError: null,
    screen: 'off',
    video: []
  };
  const bar = (camera) =>
    render(CallBar, {
      props: {
        status: { ...base, camera },
        dismissed: false,
        roomName: 'Plans',
        names: new Map(),
        busy: false,
        cameraBusy: false,
        screenBusy: false,
        volume: 100,
        onmute() {},
        ondeafen() {},
        onvolume() {},
        oncamera() {},
        onscreen() {},
        onleave() {},
        ondismiss() {}
      }
    }).body;
  assert.match(bar('not_found'), /The chosen camera is not connected/);
  assert.match(bar('unavailable'), /The camera could not start/);
  assert.match(bar('starting'), /Starting the camera/);
  assert.match(bar('off'), /Turn camera on/);
  assert.match(bar('on'), /Turn camera off/);
  assert.doesNotMatch(bar('on'), /not connected|could not start|Starting the camera/);
  assert.doesNotMatch(bar('off'), /not connected|could not start|Starting the camera/);
  assert.match(bar('not_found'), /aria-pressed="false"/);
});

test('GIVEN a call WHEN the call bar is rendered THEN deafen is a toggle that says what it does, and the volume is a 0 to 100 slider read as a percentage', () => {
  const bar = (over, volume = 40) =>
    render(CallBar, {
      props: {
        status: {
          phase: 'active',
          roomId: '!r:s',
          muted: false,
          deafened: false,
          volume: 100,
          participants: [],
          ended: null,
          joinError: null,
          camera: 'off',
          video: [],
          ...over
        },
        dismissed: false,
        roomName: 'Plans',
        names: new Map(),
        busy: false,
        cameraBusy: false,
        volume,
        onmute() {},
        ondeafen() {},
        onvolume() {},
        oncamera() {},
        onleave() {},
        ondismiss() {}
      }
    }).body;
  const hearing = bar({});
  assert.match(hearing, /class="secondary call-deafen"[^>]*aria-pressed="false"/);
  assert.match(hearing, />\s*Deafen\s*</);
  assert.match(hearing, /Turns off all call sound and mutes your microphone/);
  assert.match(hearing, /type="range"[^>]*min="0"[^>]*max="100"/);
  assert.match(hearing, /aria-valuetext="40%"/);
  const deaf = bar({ deafened: true, muted: true });
  assert.match(deaf, /class="secondary call-deafen"[^>]*aria-pressed="true"/);
  assert.match(deaf, />\s*Undeafen\s*</);
  assert.match(deaf, />\s*Unmute\s*</);
  const ended = bar({ phase: 'ended', ended: 'left' });
  assert.doesNotMatch(ended, /type="range"|call-deafen/);
});

test('GIVEN the video source files WHEN read THEN they use no raw HTML insertion', async () => {
  for (const file of ['video.ts', 'CallVideo.svelte', 'CallBar.svelte']) {
    const source = await readFile(new URL(`../src/${file}`, import.meta.url), 'utf8');
    assert.doesNotMatch(source, /\{@html\b|\binnerHTML\b/, file);
  }
});

test('GIVEN the screen tile WHEN pictures are parsed THEN 1280 by 720 passes only there and larger ones are refused first', () => {
  assert.deepEqual([VIDEO_TILES, SCREEN_TILE], [6, 5]);
  assert.deepEqual([maxSide(0), maxSide(4), maxSide(5)], [640, 640, 1280]);
  assert.deepEqual([maxPixels(0), maxPixels(5)], [640 * 360, 1280 * 720]);
  const full = parseVideoPicture(raw({ width: 1280, height: 720 }), SCREEN_TILE);
  assert.equal(full.pixels.length, 3686400);
  assert.equal(parseVideoPicture(raw({ width: 720, height: 1280, rotation: 1 }), SCREEN_TILE).height, 1280);
  // The size is checked from the prefix, before the pixels are viewed: a
  // prefix that announces a bigger picture is refused even with a short body.
  const refused = [
    ['1281 wide', raw({ width: 1281, height: 1, length: 1281 * 4 })],
    ['721 high at 1280', raw({ width: 1280, height: 721 })],
    ['1 by 1281', raw({ width: 1, height: 1281, length: 1281 * 4 })]
  ];
  for (const [name, input] of refused) {
    assert.throws(() => parseVideoPicture(input, SCREEN_TILE), (reason) => reason === BAD_VIDEO, name);
  }
  // The same sizes are refused on every camera tile.
  for (const tile of [0, 1, 2, 3, 4]) {
    assert.throws(() => parseVideoPicture(raw({ width: 1280, height: 720 }), tile), (reason) => reason === BAD_VIDEO, `tile ${tile}`);
    assert.throws(() => parseVideoPicture(raw({ width: 641, height: 360 }), tile), (reason) => reason === BAD_VIDEO, `tile ${tile}`);
  }
  for (const tile of [6, -1, 1.5, NaN]) {
    assert.throws(() => parseVideoPicture(raw({ width: 1, height: 1 }), tile), (reason) => reason === BAD_VIDEO, `tile ${tile}`);
  }
});

test('GIVEN the screen tile WHEN the fetcher asks THEN it is asked like any other tile', async () => {
  const asked = [];
  const fetcher = new VideoFetcher(
    async (tile) => {
      asked.push(tile);
      return null;
    },
    () => {}
  );
  fetcher.setTiles([{ tile: 5, key: 'a', stream: 1 }]);
  await later();
  assert.deepEqual(asked, [5]);
  assert.deepEqual(fetcher.active, [5]);
});

test('GIVEN each screen state WHEN the call bar is rendered THEN the button and the note follow the state and nothing names a source', async () => {
  const base = {
    phase: 'active',
    roomId: '!r:s',
    muted: false,
    participants: [],
    ended: null,
    joinError: null,
    camera: 'off',
    video: []
  };
  const bar = (screen, over = {}) =>
    render(CallBar, {
      props: {
        status: { ...base, screen },
        dismissed: false,
        roomName: 'Plans',
        names: new Map(),
        busy: false,
        cameraBusy: false,
        screenBusy: false,
        onmute() {},
        oncamera() {},
        onscreen() {},
        onleave() {},
        ondismiss() {},
        ...over
      }
    }).body;
  assert.match(bar('off'), /Share screen/);
  assert.match(bar('starting'), /Stop sharing/);
  assert.match(bar('starting'), /Starting the screen share/);
  assert.match(bar('on'), /Stop sharing/);
  assert.match(bar('on'), /You are sharing your screen/);
  assert.match(bar('not_found'), /Share screen/);
  assert.match(bar('not_found'), /not available any more/);
  assert.match(bar('unavailable'), /Screen sharing is not available here/);
  assert.match(bar('refused'), /did not allow the screen share/);
  assert.doesNotMatch(bar('off'), /not available|did not allow|You are sharing/);
  assert.match(bar('off', { screenBusy: true }), /call-screen[^>]*disabled/);
  assert.match(bar('refused'), /aria-pressed="false"/);
  assert.match(bar('on'), /aria-pressed="true"/);
  // Both windows show the platform's refusal text (the macOS one names the
  // setting), from the one helper.
  for (const file of ['CallBar.svelte', 'CallWindow.svelte']) {
    const source = await readFile(new URL(`../src/${file}`, import.meta.url), 'utf8');
    assert.match(source, /screenNote\(status\.screen, platform\)/, file);
    assert.doesNotMatch(source, /SCREEN_TEXT\[status\.screen\]/, file);
  }
});

test('GIVEN a participant that shares WHEN the call bar is rendered THEN the mark is plain text, also for a hostile name', () => {
  const person = (sharing) => ({
    userId: '@a:s',
    deviceId: 'D',
    speaking: false,
    reachable: true,
    keyed: true,
    sharing
  });
  const bar = (sharing, name) =>
    render(CallBar, {
      props: {
        status: {
          phase: 'active',
          roomId: '!r:s',
          muted: false,
          participants: [person(sharing)],
          ended: null,
          joinError: null,
          camera: 'off',
          screen: 'off',
          video: []
        },
        dismissed: false,
        roomName: 'Plans',
        names: new Map([['@a:s', name]]),
        busy: false,
        cameraBusy: false,
        screenBusy: false,
        onmute() {},
        oncamera() {},
        onscreen() {},
        onleave() {},
        ondismiss() {}
      }
    }).body;
  assert.match(bar(true, 'Ann'), /is sharing their screen/);
  assert.doesNotMatch(bar(false, 'Ann'), /is sharing their screen/);
  const hostile = bar(true, '<img src=x onerror=alert(1)>');
  assert.doesNotMatch(hostile, /<img/);
  assert.match(hostile, /&lt;img/);
});

test('GIVEN the screen tile WHEN the video is rendered THEN it is a canvas labelled as a shared screen with the sharer as text', () => {
  const tile = {
    tile: 5,
    own: false,
    participant: { userId: '@a:s', deviceId: 'D' },
    stream: 3
  };
  const { body } = render(CallVideo, {
    props: { tiles: [tile], names: new Map([['@a:s', '<b>Ann</b>']]), ticks: 0 }
  });
  assert.match(body, /<canvas/);
  assert.match(body, /Shared screen/);
  assert.doesNotMatch(body, /<b>Ann/);
});
