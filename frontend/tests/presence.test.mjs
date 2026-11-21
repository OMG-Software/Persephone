import assert from 'node:assert/strict';
import { test } from 'node:test';
import { PRESENCE_CHOICES, presenceLabel, presenceOf } from '../src/presence.ts';

test('GIVEN a chosen status WHEN read THEN only the three statuses pass, and anything else is online', () => {
  for (const { value } of PRESENCE_CHOICES) assert.equal(presenceOf(value), value);
  for (const value of ['busy', 'Online', '', null, undefined, 1, { online: null }]) {
    assert.equal(presenceOf(value), 'online', JSON.stringify(value) ?? 'undefined');
  }
});

test('GIVEN a received presence WHEN shown THEN it has words only when it is one of the three statuses', () => {
  assert.equal(presenceLabel('online'), 'Online');
  assert.equal(presenceLabel('unavailable'), 'Away');
  assert.equal(presenceLabel('offline'), 'Offline');
  for (const value of ['busy', '<b>online</b>', null, undefined]) {
    assert.equal(presenceLabel(value), null);
  }
});
