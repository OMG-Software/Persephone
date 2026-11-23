import assert from 'node:assert/strict';
import { test } from 'node:test';
import { PRESENCE_CHOICES, knownPresence, ownPresence, presenceLabel, presenceMenu, presenceOf } from '../src/presence.ts';

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

test('GIVEN a reader presence WHEN read THEN only the three statuses pass, and anything else is not known', () => {
  for (const { value } of PRESENCE_CHOICES) assert.equal(knownPresence(value), value);
  for (const value of ['busy', 'Online', '', null, undefined, 1, { online: null }]) {
    assert.equal(knownPresence(value), null, JSON.stringify(value) ?? 'undefined');
  }
});

test('GIVEN the account settings WHEN the footer reads them THEN sharing off shows Not shared, and sharing on the chosen status', () => {
  assert.deepEqual(ownPresence({ sharePresence: true, presence: 'online' }), { status: 'online', label: 'Online' });
  assert.deepEqual(ownPresence({ sharePresence: true, presence: 'unavailable' }), { status: 'unavailable', label: 'Away' });
  assert.deepEqual(ownPresence({ sharePresence: true, presence: 'offline' }), { status: 'offline', label: 'Offline' });
  assert.deepEqual(ownPresence({ sharePresence: true, presence: 'busy' }), { status: 'online', label: 'Online' });
  assert.deepEqual(ownPresence({ sharePresence: false, presence: 'unavailable' }), { status: 'hidden', label: 'Not shared' });
});

test('GIVEN the presence menu WHEN a status is chosen THEN it is saved with sharing on, and stopping keeps the status', () => {
  const patches = [];
  const items = presenceMenu({ sharePresence: false, presence: 'unavailable' }, (patch) => patches.push(patch));
  assert.deepEqual(
    items.map((item) => [item.label, item.checked]),
    [['Online', false], ['Away', false], ['Appear offline', false], ['Do not share presence', true]]
  );
  assert.equal(items[3].separated, true);
  assert.deepEqual(items.map((item) => item.mark), ['presence-mark online', 'presence-mark unavailable', 'presence-mark offline', undefined]);
  items[1].onselect();
  items[2].onselect();
  items[3].onselect();
  assert.deepEqual(patches, [
    { sharePresence: true, presence: 'unavailable' },
    { sharePresence: true, presence: 'offline' },
    { sharePresence: false }
  ]);
  const shared = presenceMenu({ sharePresence: true, presence: 'offline' }, () => {});
  assert.deepEqual(shared.map((item) => item.checked), [false, false, true, false]);
});

test('GIVEN a failed save or a refused presence WHEN the footer shows a notice THEN the failure comes first, the refusal is fixed text, and neither shows nothing', async () => {
  const { PRESENCE_REFUSED, presenceNotice } = await import('../src/presence.ts');
  assert.equal(presenceNotice(null, false), null);
  assert.equal(presenceNotice(null, true), PRESENCE_REFUSED);
  assert.equal(presenceNotice('The setting could not be saved.', true), 'The setting could not be saved.');
  assert.equal(presenceNotice('The setting could not be saved.', false), 'The setting could not be saved.');
  assert.doesNotMatch(PRESENCE_REFUSED, /[<>]/, 'plain text');
});
