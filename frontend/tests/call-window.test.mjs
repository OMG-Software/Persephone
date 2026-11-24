import assert from 'node:assert/strict';
import { test } from 'node:test';
import { callGrid } from '../src/callWindow.ts';
import {
  DEVICE_CHANGE_TEXT,
  DEVICE_TEST_LABEL,
  IDLE_CALL,
  IDLE_DEVICE_TEST,
  deviceKey,
  deviceTestActive,
  deviceTestText,
  levelMap,
  parseLevels
} from '../src/calls.ts';

const person = (userId, deviceId, more = {}) => ({
  userId,
  deviceId,
  speaking: false,
  reachable: true,
  keyed: true,
  sharing: false,
  muted: false,
  ...more
});

test('GIVEN a call with cameras on and off WHEN the grid is made THEN every device has a cell, with its tile or a placeholder', () => {
  const status = {
    ...IDLE_CALL,
    phase: 'active',
    muted: true,
    participants: [person('@a:s', 'A', { speaking: true }), person('@b:s', 'B', { muted: true })],
    video: [
      { tile: 0, participant: null, own: true, stream: 1 },
      { tile: 2, participant: { userId: '@a:s', deviceId: 'A' }, own: false, stream: 3 },
      { tile: 5, participant: { userId: '@b:s', deviceId: 'B' }, own: false, stream: 4 }
    ]
  };
  const cells = callGrid(status);
  assert.deepEqual(
    cells.map((cell) => [cell.own, cell.userId, cell.tile, cell.screen, cell.muted, cell.speaking]),
    [
      [true, null, 0, false, true, false],
      [false, '@a:s', 2, false, false, true],
      // B shares a screen but has no camera: a placeholder, then the screen.
      [false, '@b:s', null, false, true, false],
      [false, '@b:s', 5, true, false, false]
    ]
  );
  assert.equal(new Set(cells.map((cell) => cell.key)).size, cells.length, 'keys are unique');
});

test('GIVEN a call with no video WHEN the grid is made THEN you and each device show placeholders', () => {
  const cells = callGrid({ ...IDLE_CALL, phase: 'active', participants: [person('@a:s', 'A')] });
  assert.deepEqual(cells.map((cell) => cell.tile), [null, null]);
});

test('GIVEN level lists WHEN parsed THEN only bounded lists pass, and a device left out is silent', () => {
  const bob = { userId: '@bob:s', deviceId: 'B', level: 30 };
  assert.deepEqual(parseLevels([bob]), [bob]);
  assert.deepEqual(parseLevels([]), []);
  for (const bad of [
    null,
    {},
    [null],
    [{ ...bob, level: -1 }],
    [{ ...bob, level: 2.5 }],
    [{ ...bob, deviceId: 7 }],
    [{ userId: '@bob:s', level: 1 }],
    [{ ...bob, userId: 'é'.repeat(128) }],
    Array(33).fill(bob)
  ]) {
    assert.equal(parseLevels(bad), null, JSON.stringify(bad)?.slice(0, 60));
  }
  const map = levelMap([bob]);
  assert.equal(map.get(deviceKey('@bob:s', 'B')), 30);
  assert.equal(map.get(deviceKey('@carol:s', 'C')), undefined);
  // The key keeps the two IDs apart: no ID can stand for another pair.
  assert.notEqual(deviceKey('@a:s', 'B/C'), deviceKey('@a:s/B', 'C'));
});

test('GIVEN each device test state THEN only the newest test has a text, and every state has one', () => {
  assert.equal(deviceTestText(IDLE_DEVICE_TEST, 'microphone'), '');
  for (const state of ['starting', 'running', 'done', 'not_found', 'denied', 'unsupported', 'unavailable', 'in_call', 'failed']) {
    for (const kind of ['microphone', 'speaker', 'camera']) {
      const status = { test: kind, state, stream: 0 };
      assert.notEqual(deviceTestText(status, kind), '', `${kind} ${state}`);
      assert.equal(deviceTestText(status, kind === 'camera' ? 'speaker' : 'camera'), '');
      assert.equal(deviceTestActive(status, kind), state === 'starting' || state === 'running');
    }
  }
  assert.match(deviceTestText({ test: 'microphone', state: 'running', stream: 0 }, 'microphone'), /meter/);
  assert.match(deviceTestText({ test: 'speaker', state: 'running', stream: 0 }, 'speaker'), /tone/);
  assert.match(deviceTestText({ test: 'camera', state: 'denied', stream: 0 }, 'camera'), /privacy/);
  // The camera reports a privacy block as unavailable, so its text names both.
  const camera = deviceTestText({ test: 'camera', state: 'unavailable', stream: 0 }, 'camera');
  assert.match(camera, /another app/i);
  assert.match(camera, /privacy/);
  assert.doesNotMatch(deviceTestText({ test: 'microphone', state: 'unavailable', stream: 0 }, 'microphone'), /privacy/);
  assert.deepEqual(Object.keys(DEVICE_TEST_LABEL), ['microphone', 'speaker', 'camera']);
  assert.deepEqual(Object.keys(DEVICE_CHANGE_TEXT).sort(), ['failed', 'not_found', 'unsupported']);
});
