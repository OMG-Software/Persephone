import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';
import * as image from '../src/image.ts';

let vite;
let Timeline;
let Composer;
let render;

before(async () => {
  // Test files run in parallel: each server keeps its own dependency cache,
  // and none discovers dependencies mid-run, so no server finds another's
  // pre-bundle outdated (ERR_OUTDATED_OPTIMIZED_DEP).
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/image',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: Timeline } = await vite.ssrLoadModule('/src/Timeline.svelte'));
  ({ default: Composer } = await vite.ssrLoadModule('/src/Composer.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

const HANDLE = '0123456789abcdef0123456789abcdef';
const hostileName = '<img src=x onerror=alert(1)>‮gnp.exe';

/** A raw band response: width and rows as little-endian u32, then the pixels. */
function rawBand(width, rows, pixels = new Uint8Array(width * rows * 4)) {
  const body = new Uint8Array(8 + pixels.byteLength);
  const prefix = new DataView(body.buffer);
  prefix.setUint32(0, width, true);
  prefix.setUint32(4, rows, true);
  body.set(pixels, 8);
  return body.buffer;
}

/** Band `index` of a `width`-wide image: every byte is its band number + 1. */
const filled = (width, rows, index) => new Uint8Array(width * rows * 4).fill(index + 1);

test('GIVEN a raw band response WHEN parsed THEN width, rows and exactly its pixels are read', () => {
  const band = image.parseBand(rawBand(3, 2, filled(3, 2, 6)));
  assert.equal(band.width, 3);
  assert.equal(band.rows, 2);
  assert.equal(band.pixels.byteLength, 24);
  assert.ok(band.pixels.every((byte) => byte === 7));
  // A typed-array view of the same bytes reads the same.
  assert.equal(image.parseBand(new Uint8Array(rawBand(1, 1))).width, 1);
});

test('GIVEN a band response outside the layout WHEN parsed THEN it is rejected as bad_image', () => {
  const tooLong = new Uint8Array(rawBand(2, 2).byteLength + 1);
  tooLong.set(new Uint8Array(rawBand(2, 2)));
  const cut = rawBand(2, 2).slice(0, 8 + 15);
  for (const raw of [
    null,
    'AAAA',
    [0, 0, 0, 0],
    new ArrayBuffer(7),
    tooLong.buffer,
    cut,
    rawBand(0, 1, new Uint8Array(0)),
    rawBand(1, 0, new Uint8Array(0)),
    rawBand(2049, 1),
    // 1024 × 257 × 4 is over the 1 MiB band limit.
    rawBand(1024, 257)
  ]) {
    assert.throws(() => image.parseBand(raw), (reason) => reason === 'bad_image');
  }
});

test('GIVEN bands with a shorter last band WHEN assembled THEN each is copied at its row offset', async () => {
  const view = { handle: HANDLE, width: 3, height: 5, bands: 3, bandRows: 2 };
  const asked = [];
  const pixels = await image.assembleImage(view, async (index) => {
    asked.push(index);
    const rows = index === 2 ? 1 : 2;
    return image.parseBand(rawBand(3, rows, filled(3, rows, index)));
  });
  assert.deepEqual(asked, [0, 1, 2], 'in order, one at a time');
  assert.ok(pixels instanceof Uint8ClampedArray);
  assert.equal(pixels.byteLength, 3 * 5 * 4);
  const rowBytes = 3 * 4;
  const rowValue = (row) => pixels[row * rowBytes];
  assert.deepEqual([0, 1, 2, 3, 4].map(rowValue), [1, 1, 2, 2, 3]);
  assert.ok(pixels.subarray(4 * rowBytes).every((byte) => byte === 3), 'the last row is whole');
});

test('GIVEN a band of the wrong size WHEN assembled THEN the image is rejected and nothing is returned', async () => {
  const view = { handle: HANDLE, width: 3, height: 5, bands: 3, bandRows: 2 };
  const wrong = [
    // Too many rows for the last band.
    (index) => (index === 2 ? { width: 3, rows: 2, pixels: filled(3, 2, 0) } : null),
    // Another width.
    (index) => (index === 1 ? { width: 2, rows: 2, pixels: filled(2, 2, 0) } : null),
    // The right header with too few bytes.
    (index) => (index === 0 ? { width: 3, rows: 2, pixels: filled(3, 1, 0) } : null)
  ];
  for (const override of wrong) {
    await assert.rejects(
      image.assembleImage(view, async (index) => {
        const rows = index === 2 ? 1 : 2;
        return override(index) ?? { width: 3, rows, pixels: filled(3, rows, index) };
      }),
      (reason) => reason === 'bad_image'
    );
  }
});

test('GIVEN a view outside the limits WHEN assembled THEN no band is fetched', async () => {
  const good = { handle: HANDLE, width: 3, height: 5, bands: 3, bandRows: 2 };
  for (const view of [
    { ...good, bands: 2 },
    { ...good, bands: 4 },
    { ...good, bandRows: 0 },
    { ...good, width: 0 },
    { ...good, height: 2049 },
    { ...good, width: 1.5 },
    { ...good, handle: HANDLE.toUpperCase() },
    { ...good, handle: '../x' },
    // A band of 2048 rows of 2048 pixels is far over 1 MiB.
    { handle: HANDLE, width: 2048, height: 2048, bands: 1, bandRows: 2048 }
  ]) {
    let fetched = 0;
    await assert.rejects(
      image.assembleImage(view, async () => {
        fetched += 1;
        return { width: 1, rows: 1, pixels: new Uint8Array(4) };
      }),
      (reason) => reason === 'bad_image'
    );
    assert.equal(fetched, 0, JSON.stringify(view));
  }
  // The largest documented image fits: 2048 × 2048 in bands of 128 rows.
  assert.ok(image.validView({ handle: HANDLE, width: 2048, height: 2048, bands: 16, bandRows: 128 }));
});

test('GIVEN an image no longer wanted WHEN its bands arrive THEN assembly stops', async () => {
  const view = { handle: HANDLE, width: 1, height: 2, bands: 2, bandRows: 1 };
  let stop = false;
  let fetched = 0;
  const result = await image.assembleImage(
    view,
    async () => {
      fetched += 1;
      stop = true;
      return { width: 1, rows: 1, pixels: new Uint8Array(4) };
    },
    () => stop
  );
  assert.equal(result, null);
  assert.equal(fetched, 1);
});

test('GIVEN more drawn images than the limit WHEN another is admitted THEN the oldest is evicted', () => {
  const shown = new image.ShownImages(2);
  const evicted = [];
  const first = shown.admit(() => evicted.push('first'));
  shown.admit(() => evicted.push('second'));
  shown.forget(first);
  shown.admit(() => evicted.push('third'));
  assert.deepEqual(evicted, [], 'a forgotten image frees its place');
  shown.admit(() => evicted.push('fourth'));
  assert.deepEqual(evicted, ['second']);
  assert.equal(shown.count, 2);
  assert.ok(image.MAX_SHOWN_IMAGES <= 4, 'only a few decoded images stay in the page');
});

test('GIVEN image error codes WHEN shown THEN each is a short fixed message', () => {
  assert.match(image.loadErrorText('rejected'), /^This image cannot be shown\. The server refused the file/);
  assert.doesNotMatch(image.loadErrorText('rejected'), /too large/);
  assert.equal(image.canRetryLoad('rejected'), false, 'the core gives the same answer again');
  assert.equal(image.canRetryLoad('limit_exceeded'), true);
  assert.match(image.loadErrorText('limit_exceeded'), /Other images are loading/);
  assert.match(image.loadErrorText('not_found'), /no longer available/);
  assert.match(image.loadErrorText('bad_image'), /not valid/);
  // Unknown or internal text never reaches the page.
  for (const reason of ['core_unavailable', 'C:\\Users\\a\\secret.png', new Error('boom'), undefined]) {
    assert.match(image.loadErrorText(reason), /^The image could not be loaded/);
    assert.match(image.attachErrorText(reason), /^The image could not be attached/);
    assert.equal(image.sendImageErrorText(reason), 'The image was not queued. Try again.');
  }
  assert.match(image.sendImageErrorText('limit_exceeded'), /An image is already waiting to send/);
  assert.match(image.attachErrorText('rejected'), /larger than 13 MB, damaged, or not a PNG or JPEG/);
  assert.match(image.attachErrorText('limit_exceeded'), /already preparing an image/);
  assert.equal(image.formatSize(48_213), '48 KB');
  assert.equal(image.formatSize(999), '999 bytes');
  assert.equal(image.formatSize(20_971_520), '21.0 MB');
  assert.equal(image.declaredText(48_213, 640, 480), '640 × 480 · 48 KB');
  assert.equal(image.declaredText(null, null, null), '');
});

const WINDOWS_UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0';
const LINUX_UA = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko)';
const MAC_UA = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)';

test('GIVEN an unavailable core or server WHEN the page is on Windows or unknown THEN the text asks to try again without naming a platform', () => {
  for (const userAgent of [WINDOWS_UA, '', 'Node.js/22']) {
    assert.equal(image.notWindows(userAgent), false, userAgent);
    for (const reason of ['unavailable', 'core_unavailable']) {
      assert.equal(image.loadErrorText(reason, userAgent), 'The image could not be loaded right now. Try again.');
      assert.equal(image.attachErrorText(reason, userAgent), 'The image could not be attached right now. Try again.');
      assert.equal(image.canRetryLoad(reason), true);
    }
  }
});

test('GIVEN an unavailable core WHEN the page is on another platform THEN the text says images work on Windows only', () => {
  for (const userAgent of [LINUX_UA, MAC_UA]) {
    assert.equal(image.notWindows(userAgent), true, userAgent);
    assert.equal(
      image.loadErrorText('core_unavailable', userAgent),
      'The image could not be loaded. Images work on Windows only for now.'
    );
    assert.equal(
      image.attachErrorText('unavailable', userAgent),
      'The image could not be attached. Images work on Windows only for now.'
    );
  }
});

test('GIVEN a server refusal WHEN a received image fails as rejected THEN the text says it cannot be shown on every platform', () => {
  for (const userAgent of [WINDOWS_UA, LINUX_UA]) {
    const text = image.loadErrorText('rejected', userAgent);
    assert.match(text, /^This image cannot be shown\./);
    assert.doesNotMatch(text, /too large|Windows/);
  }
  assert.equal(image.canRetryLoad('rejected'), false);
});

const timeline = (items) =>
  render(Timeline, {
    props: {
      title: 'Room',
      roomId: '!r:example.invalid',
      items,
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {}
    }
  }).body;

test('GIVEN a received image WHEN the timeline shows THEN it is an inert placeholder until loaded', () => {
  const html = timeline([
    {
      kind: 'image',
      event_id: '$image',
      sender: hostileName,
      origin_server_ts: 1_767_258_060_000,
      name: hostileName,
      size: 48_213,
      width: 640,
      height: 480,
      encrypted: true
    },
    {
      kind: 'image',
      event_id: '$plain',
      sender: '@b:example.invalid',
      origin_server_ts: null,
      name: '',
      size: null,
      width: null,
      height: null,
      encrypted: false
    }
  ]);
  assert.doesNotMatch(html, /<img/i, 'the name is text');
  assert.doesNotMatch(html, /<canvas/, 'nothing is drawn or fetched before the user asks');
  assert.match(html, /<p class="attachment-name"><bdi>&lt;img src=x onerror=alert\(1\)>‮gnp\.exe<\/bdi><\/p>/);
  assert.match(html, /<strong class="sender"><bdi>&lt;img/);
  assert.match(html, /640 × 480 · 48 KB/);
  assert.equal((html.match(/>Load image</g) ?? []).length, 2);
  assert.match(html, /<button type="button" class="secondary">Load image<span class="visually-hidden">: <bdi>&lt;img/);
  assert.match(html, /class="file-enc"><svg[\s\S]*?<\/svg>(?:<!---->)?Encrypted</);
  assert.match(html, /class="file-plain">Not encrypted</);
  // A file without a name is shown as "Image".
  assert.match(html, /<p class="attachment-name"><bdi>Image<\/bdi><\/p>/);
  assert.match(html, /<time datetime="2026-01-01T09:01:00\.000Z"/);
});

const composer = (props = {}) =>
  render(Composer, {
    props: {
      pending: [],
      blocked: null,
      encryption: 'encrypted',
      roomName: 'R',
      draftVersion: 0,
      onsend: async () => true,
      onretry: () => {},
      ondiscard: () => {},
      onallow: () => {},
      ...props
    }
  }).body;

test('GIVEN images being sent WHEN pending THEN each shows as an image with the text delivery states and actions', () => {
  const sent = (localId, state, failure = null) => ({
    localId,
    roomId: '!r:example.invalid',
    kind: 'image',
    body: hostileName,
    state,
    eventId: state === 'sent' ? '$e' : null,
    failure,
    queuedAt: 1_767_258_000_000
  });
  const html = composer({
    pending: [
      sent(1, 'queued'),
      sent(2, 'sending'),
      sent(3, 'unknown'),
      sent(4, 'failed', 'identity_changed'),
      sent(5, 'failed', 'confirmation_required'),
      sent(6, 'sent')
    ]
  });
  assert.equal((html.match(/<p class="pending-image"><svg[\s\S]*?<\/svg>(?:<!---->)?Image<\/p>/g) ?? []).length, 6);
  assert.doesNotMatch(html, /&lt;img|<img/, 'an image entry shows no body');
  assert.match(html, /Waiting to send…/);
  assert.match(html, /Sending…/);
  assert.match(html, /Delivery unknown/);
  assert.match(html, /Not sent: someone in this room changed their encryption identity\./);
  assert.match(html, /Not sent: this room is not encrypted\./);
  assert.match(html, /Sent\./);
  assert.equal((html.match(/>Retry</g) ?? []).length, 3);
  assert.equal((html.match(/>Send unencrypted</g) ?? []).length, 1);
  assert.equal((html.match(/>Discard</g) ?? []).length, 4);
});

test('GIVEN the composer WHEN shown THEN Attach image is a labelled button, off while blocked or busy', () => {
  assert.match(composer(), /<button type="button" class="attach" aria-label="Attach image" title="Attach image"/);
  const blocked = composer({ blocked: 'Verify this device in Settings, Security & encryption, to send encrypted messages.' });
  assert.match(blocked, /<button type="button" class="attach" aria-label="Attach image" title="Attach image" disabled/);
  assert.match(composer({ attaching: true }), /aria-label="Choosing an image…"[^>]*disabled/);
});

test('GIVEN an attached image WHEN the composer shows it THEN the preview, its size and Send and Cancel show', () => {
  const draft = {
    roomId: '!r:example.invalid',
    preview: { handle: HANDLE, width: 640, height: 480, bands: 2, bandRows: 409 },
    size: 48_213,
    mime: 'image/jpeg'
  };
  const html = composer({ imageDraft: draft });
  assert.match(html, /role="group" aria-labelledby="image-draft-title"/);
  assert.match(html, /<div class="image-view" role="img" aria-label="Preview of the image to send"><canvas class="image-canvas" width="640" height="480" aria-hidden="true">/);
  assert.match(html, /JPEG · 640 × 480 · 48 KB\./);
  assert.match(html, /Location, camera and other details in the file are removed before it is sent\./);
  assert.match(html, /<button type="button" class="primary">Send image<\/button>/);
  assert.match(html, />Cancel<\/button>/);
  assert.match(html, /class="attach"[^>]*disabled/, 'one image at a time');
  const sending = composer({ imageDraft: draft, sendingImage: true });
  assert.match(sending, /<button type="button" class="primary" disabled(?:="")?>Sending…<\/button>/);
  const failed = composer({ imageError: 'An image is already waiting to send. Wait until it is sent, or discard it, then send this one.' });
  assert.match(failed, /<p class="error image-error" role="alert">An image is already waiting to send\./);
});

test('GIVEN the page sources WHEN read THEN images never use blob: or data: URLs or <img>', async () => {
  const src = new URL('../src/', import.meta.url);
  const entries = await readdir(src, { recursive: true });
  for (const entry of entries.filter((name) => /\.(ts|svelte)$/.test(name))) {
    const text = await readFile(new URL(entry.replaceAll('\\', '/'), src), 'utf8');
    assert.doesNotMatch(text, /createObjectURL|toDataURL|toBlob|\bblob:|\bdata:image/, entry);
  }
  for (const file of ['ImageMessage.svelte', 'ImageCanvas.svelte', 'ImageDraft.svelte']) {
    const text = await readFile(new URL(file, src), 'utf8');
    assert.doesNotMatch(text, /<img\b/, file);
  }
});
