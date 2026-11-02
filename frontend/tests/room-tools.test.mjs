import assert from 'node:assert/strict';
import { test } from 'node:test';
import { healthLabels } from '../src/healthLabels.ts';
import { appendAttachments, createAttachmentQueryGuard, localTimestamp, pinSummary, searchFilters } from '../src/roomTools.ts';

test('GIVEN local date input WHEN invalid or normalized THEN reject it', () => {
  for (const value of ['', '1960-01-01T10:00', '2026-02-30T10:00', '2026-11-01', 'invalid', '2026-13-01T10:00']) assert.equal(localTimestamp(value), null);
  assert.equal(localTimestamp('2026-11-01T10:00'), new Date(2026, 10, 1, 10, 0).getTime());
});

test('GIVEN search filters WHEN sender and dates are valid THEN retain exact boundaries', () => {
  const filters = searchFilters(' @alice:example.org ', '2026-01-01T10:00', '2026-01-02T10:00');
  assert.deepEqual(filters, { sender: '@alice:example.org', fromMs: new Date(2026, 0, 1, 10).getTime(), beforeMs: new Date(2026, 0, 2, 10).getTime() });
  assert.deepEqual(searchFilters('', '', ''), { sender: null, fromMs: null, beforeMs: null });
  assert.equal(searchFilters('alice', '', ''), null);
  assert.equal(searchFilters('@alice:example.org', '2026-01-02T10:00', '2026-01-01T10:00'), null);
  assert.equal(searchFilters('', '2026-01-01T10:00', '2026-01-01T10:00'), null);
});

const file = (id) => ({ kind: 'file', event_id: id });
test('GIVEN overlapping metadata pages WHEN appended THEN deduplicate, filter and bound state', () => {
  assert.deepEqual(appendAttachments([file('$a')], [file('$a'), { kind: 'gap' }, { kind: 'message', event_id: '$x' }, file('$b')]), [file('$a'), file('$b')]);
  assert.equal(appendAttachments([], Array.from({ length: 600 }, (_, at) => file(`$${at}`))).length, 500);
});


test('GIVEN health codes WHEN shown THEN use friendly fixed labels and a generic error', () => {
  const labels = healthLabels({ accountState: 'signed_in', storeState: 'restart_required', syncMode: 'sliding', lastError: 'sync_failed' });
  assert.deepEqual(labels, { account: 'Signed in', store: 'Restart required', mode: 'Sliding Sync', error: 'The last connection attempt failed.' });
  assert.equal(healthLabels({ accountState: 'signed_out', storeState: 'locked', syncMode: 'unknown', lastError: null }).error, 'None recorded');
});


test('GIVEN control characters in sender filters WHEN validated THEN reject C0 and C1 controls', () => {
  for (let code = 0; code <= 0x9f; code += 1) {
    if (code >= 0x20 && code < 0x7f) continue;
    assert.equal(searchFilters(`@alice${String.fromCodePoint(code)}:example.org`, '', ''), null, `U+${code.toString(16)}`);
  }
  assert.equal(searchFilters('@alice:example.org', '', '').sender, '@alice:example.org');
});

// Unit coverage for the request guard. This does not mount RoomTools or exercise its effects.
test('GIVEN an attachment query guard WHEN selection changes away and back THEN its old ticket is invalid', async () => {
  const guard = createAttachmentQueryGuard();
  assert.equal(guard.select('images'), true);
  const current = guard.capture('images');
  let resolve;
  const response = new Promise((done) => { resolve = done; });
  const pending = response.then(() => current());
  assert.equal(guard.select('images'), false);
  assert.equal(current(), true);
  guard.select('files');
  guard.select('images');
  resolve();
  assert.equal(await pending, false);
  assert.equal(guard.capture('images')(), true);
});

test('GIVEN long pinned text WHEN describing its controls THEN bound sender and summary', () => {
  const label = pinSummary({ kind: 'message', sender: 's'.repeat(1000), body: 'x'.repeat(10000) });
  assert.equal(label, `Message from ${'s'.repeat(80)}: ${'x'.repeat(120)}`);
  assert.equal(pinSummary(), 'Unavailable pinned message');
  assert.equal(pinSummary(undefined, '$missing'), 'Unavailable pinned message: $missing');
});


test('GIVEN exact sender server names WHEN validated THEN enforce Ruma host and port syntax', () => {
  for (const server of ['example.org', 'example.org:443', 'example.org:8448', 'example.org:65535', 'example.org:0', '127.0.0.1', '[::1]', '[::1]:443', '[2001:db8::1]:8448', '[::ffff:192.0.2.1]']) {
    const sender = `@alice:${server}`;
    assert.equal(searchFilters(sender, '', '')?.sender, sender, server);
  }
  for (const server of ['example.org:99999', 'example.org:65536', 'example.org:', 'example.org:-1', 'example.org:abc', 'example.org:443:1', ':8448', 'example.org/path', 'example_org', '[::1', '[invalid]', '[::1]:99999', '[::1]extra', '::1']) {
    assert.equal(searchFilters(`@alice:${server}`, '', ''), null, server);
  }
});

test('GIVEN identical pin previews WHEN labels are made THEN full valid event IDs distinguish each target', () => {
  const prefix = '$' + 'a'.repeat(250);
  const first = prefix + '1111';
  const second = prefix + '2222';
  const item = { kind: 'message', sender: '@alice:example.org', body: 'Same text', event_id: first };
  const one = pinSummary(item, first);
  const two = pinSummary({ ...item, event_id: second }, second);
  assert.notEqual(one, two);
  assert.ok(one.endsWith(first));
  assert.ok(two.endsWith(second));
  assert.equal(pinSummary(item), one);
  assert.notEqual(pinSummary(undefined, first), pinSummary(undefined, second));
  assert.ok(pinSummary(undefined, first).endsWith(first));
});
