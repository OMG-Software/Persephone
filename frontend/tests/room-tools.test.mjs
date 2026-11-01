import assert from 'node:assert/strict';
import { test } from 'node:test';
import { healthLabels } from '../src/healthLabels.ts';
import { appendAttachments, localTimestamp, searchFilters } from '../src/roomTools.ts';

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
