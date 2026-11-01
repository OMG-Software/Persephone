import assert from 'node:assert/strict';
import { test } from 'node:test';
import { RoomGroupRestoration } from '../src/roomGroupRestoration.ts';

test('GIVEN unsaved grouping edits and a save error WHEN an unrelated preference changes THEN both survive', () => {
  const restoration = new RoomGroupRestoration();
  let controls = { section: '', favourite: false, failure: null };
  const restore = (preferences) => {
    const saved = preferences.find((entry) => entry.roomId === '!current:s');
    const changed = restoration.changed('!current:s', saved?.section ?? '', saved?.favourite ?? false);
    if (changed) controls = { section: changed.section, favourite: changed.favourite, failure: null };
  };
  restore([{ roomId: '!current:s', section: 'Saved', favourite: false }]);
  controls = { section: 'Unsaved', favourite: true, failure: 'Save failed' };

  restore([
    { roomId: '!current:s', section: 'Saved', favourite: false },
    { roomId: '!other:s', section: 'Elsewhere', favourite: true }
  ]);
  assert.deepEqual(controls, { section: 'Unsaved', favourite: true, failure: 'Save failed' });

  restore([{ roomId: '!current:s', section: 'Saved', favourite: false }]);
  assert.deepEqual(controls, { section: 'Unsaved', favourite: true, failure: 'Save failed' });
});

test('GIVEN the current saved group WHEN its section or favourite changes THEN the new values restore once', () => {
  const restoration = new RoomGroupRestoration();
  assert.deepEqual(restoration.changed('!r:s', 'Saved', false), { roomId: '!r:s', section: 'Saved', favourite: false });
  assert.equal(restoration.changed('!r:s', 'Saved', false), null);
  assert.deepEqual(restoration.changed('!r:s', 'Updated', false), { roomId: '!r:s', section: 'Updated', favourite: false });
  assert.deepEqual(restoration.changed('!r:s', 'Updated', true), { roomId: '!r:s', section: 'Updated', favourite: true });
  assert.deepEqual(restoration.changed('!r:s', '', false), { roomId: '!r:s', section: '', favourite: false });
  assert.equal(restoration.changed('!r:s', '', false), null);
});

test('GIVEN two rooms with identical saved values WHEN the selected room changes THEN its controls restore', () => {
  const restoration = new RoomGroupRestoration();
  restoration.changed('!first:s', '', false);
  assert.deepEqual(restoration.changed('!second:s', '', false), { roomId: '!second:s', section: '', favourite: false });
  assert.deepEqual(restoration.changed('!first:s', '', false), { roomId: '!first:s', section: '', favourite: false });
});
