import assert from 'node:assert/strict';
import { test } from 'node:test';
import { appMenuFailure, appMenuView } from '../src/appMenu.ts';

// The "App menu" panel (ADR-0072): it shows only for a checked AppImage, and
// each state offers only the changes the core accepts.

test('GIVEN no AppImage WHEN the panel is built THEN it is hidden', () => {
  assert.equal(appMenuView('unavailable'), null);
});

test('GIVEN each AppImage state WHEN the panel is built THEN it offers only the matching changes', () => {
  const offers = (state) => appMenuView(state).actions.map((action) => [action.on, action.label]);
  assert.deepEqual(offers('absent'), [[true, 'Add to app menu…']]);
  assert.deepEqual(offers('added'), [[false, 'Remove from app menu']]);
  assert.deepEqual(offers('outdated'), [
    [true, 'Update menu entry…'],
    [false, 'Remove from app menu']
  ]);
  assert.deepEqual(offers('provided'), [], "another program's entry is never removed from here");
  // A change that asks first says so with an ellipsis; a removal does not ask.
  for (const state of ['absent', 'added', 'outdated']) {
    for (const action of appMenuView(state).actions) {
      assert.equal(action.label.endsWith('…'), action.on, action.label);
    }
  }
});

test('GIVEN a refused change WHEN shown THEN only fixed words appear', () => {
  assert.equal(appMenuFailure('limit_exceeded'), 'A change to the app menu entry is already under way.');
  for (const reason of ['core_unavailable', '/home/example/x', new Error('internal'), undefined]) {
    assert.equal(appMenuFailure(reason), 'The app menu entry could not be changed.');
  }
});
