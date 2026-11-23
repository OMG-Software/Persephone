import assert from 'node:assert/strict';
import { before, mock, test } from 'node:test';
import { mockIPC } from '@tauri-apps/api/mocks';

// The page's records for the diagnostic log (ADR-0058): bounded, batched,
// dropped while off, and sent only through `diagnostic_records`.

let page;

before(async () => {
  // The bridge's invoke goes through window.__TAURI_INTERNALS__.
  globalThis.window = globalThis;
  page = await import('../src/bridge/diagnostics.ts');
});

test('GIVEN text over the limit WHEN cut THEN it stays within the bytes and never splits a character', () => {
  assert.equal(page.cut('short', 10), 'short');
  assert.equal(page.cut('abcdef', 3), 'abc');
  // "é" is two bytes: four bytes hold "aé" and not half of the next one.
  assert.equal(page.cut('aéé', 4), 'aé');
  assert.equal(page.cut('😀x', 3), '');
});

test('GIVEN rejection reasons WHEN coded THEN only short lowercase codes are kept', () => {
  assert.equal(page.outcomeCode('limit_exceeded'), 'limit_exceeded');
  assert.equal(page.outcomeCode('Bearer abc'), 'error');
  assert.equal(page.outcomeCode({ code: 'x' }), 'error');
  assert.equal(page.outcomeCode('x'.repeat(49)), 'error');
});

test('GIVEN a listener WHEN the switch changes THEN it hears each change until it unsubscribes', () => {
  const heard = [];
  const stop = page.onDiagnosticsChange((on) => heard.push(on));
  page.setPageDiagnostics(true);
  page.setPageDiagnostics(true);
  page.setPageDiagnostics(false);
  stop();
  page.setPageDiagnostics(true);
  page.setPageDiagnostics(false);
  assert.deepEqual(heard, [true, false]);
  assert.equal(page.diagnosticsOn(), false);
});

test('GIVEN more records than may wait WHEN batches are taken THEN the drop count comes first and each batch is bounded', () => {
  page.setPageDiagnostics(true);
  for (let index = 0; index < page.MAX_WAITING + 5; index += 1) {
    page.note('debug', 'page::test', `record ${index}`);
  }
  const first = page.takeBatch();
  assert.equal(first.length, page.MAX_BATCH);
  assert.deepEqual(first[0], { level: 'warn', target: 'page', message: '5 records dropped: over the limit' });
  assert.equal(first[1].message, 'record 0');
  let total = first.length - 1;
  for (let batch = page.takeBatch(); batch.length > 0; batch = page.takeBatch()) {
    assert.ok(batch.length <= page.MAX_BATCH);
    total += batch.length;
  }
  assert.equal(total, page.MAX_WAITING);
  page.setPageDiagnostics(false);
});

test('GIVEN the log on WHEN records wait a second THEN one batch goes to diagnostic_records, and off sends nothing', async () => {
  const sent = [];
  mockIPC((command, args) => {
    sent.push({ command, records: args.records });
    return null;
  });
  mock.timers.enable({ apis: ['setTimeout'] });
  try {
    page.setPageDiagnostics(true);
    page.note('info', 'page::test', 'one');
    page.noteCall('call_video_frame', 'ok', performance.now());
    page.noteCall('call_video_frame', 'core_internal', performance.now());
    mock.timers.tick(1000);
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(sent.length, 1);
    assert.equal(sent[0].command, 'diagnostic_records');
    assert.deepEqual(
      sent[0].records.map((record) => [record.level, record.message.replace(/ \d+ ms$/, '')]),
      [
        ['info', 'one'],
        ['warn', 'call_video_frame core_internal']
      ]
    );

    page.note('info', 'page::test', 'dropped by off');
    page.setPageDiagnostics(false);
    mock.timers.tick(1000);
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(sent.length, 1);
  } finally {
    mock.timers.reset();
  }
});

test('GIVEN the page hooks WHEN an error and a rejection happen THEN each is noted once, with free text only from an Error', () => {
  const target = new EventTarget();
  page.installPageDiagnostics(target);
  page.installPageDiagnostics(target);
  page.setPageDiagnostics(true);
  const error = new Event('error');
  Object.assign(error, { message: 'boom', filename: 'app.js', lineno: 3, colno: 7 });
  target.dispatchEvent(error);
  const rejection = new Event('unhandledrejection');
  Object.assign(rejection, { reason: 'password: hunter2' });
  target.dispatchEvent(rejection);
  const thrown = new Event('unhandledrejection');
  Object.assign(thrown, { reason: new Error('failed to load') });
  target.dispatchEvent(thrown);
  const batch = page.takeBatch();
  page.setPageDiagnostics(false);
  assert.deepEqual(
    batch.map((record) => [record.target, record.message]),
    [
      ['page::error', 'boom at app.js:3:7'],
      ['page::rejection', 'error'],
      ['page::rejection', 'failed to load']
    ]
  );
});
