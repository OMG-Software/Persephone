import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  AUTO_PICK,
  feedKeys,
  GRID_PICK,
  keepPick,
  largeKey,
  selectFeed,
  showAll,
  splitFeeds
} from '../src/callView.ts';
import {
  DEVICE_SWITCH_TEXT,
  DEVICE_SAVED_TEXT,
  DEVICE_SWITCHING_TEXT,
  deviceSwitchText,
  feedLabel,
  SHOW_ALL_LABEL,
  SHOW_LARGE_LABEL
} from '../src/calls.ts';

const cameras = ['own', 'a', 'b'];
const withScreen = [...cameras, 'screen'];
const feed = (key) => ({ kind: 'feed', key });

test('GIVEN no pick and no screen share WHEN the large feed is read THEN the grid shows', () => {
  assert.equal(largeKey(AUTO_PICK, cameras), null);
});

test('GIVEN no pick WHEN a screen share starts THEN the screen is large', () => {
  assert.equal(largeKey(AUTO_PICK, withScreen), 'screen');
});

test('GIVEN a picked camera WHEN a screen share starts THEN the pick stays large', () => {
  assert.equal(largeKey(feed('a'), withScreen), 'a');
});

test('GIVEN the grid in view WHEN a camera is selected THEN it is large', () => {
  assert.deepEqual(selectFeed(AUTO_PICK, 'a', cameras), feed('a'));
});

test('GIVEN a large camera WHEN another feed is selected THEN that feed is large', () => {
  const next = selectFeed(feed('a'), 'b', cameras);
  assert.equal(largeKey(next, cameras), 'b');
  const { large, rest } = splitFeeds(cameras.map((key) => ({ key })), largeKey(next, cameras));
  assert.equal(large.key, 'b');
  assert.deepEqual(rest.map((cell) => cell.key), ['own', 'a']);
});

test('GIVEN a large camera WHEN it is selected again THEN the grid shows', () => {
  assert.equal(largeKey(selectFeed(feed('a'), 'a', cameras), cameras), null);
});

test('GIVEN the automatic large screen WHEN it is selected THEN the grid shows, also with the share on', () => {
  const next = selectFeed(AUTO_PICK, 'screen', withScreen);
  assert.deepEqual(next, GRID_PICK);
  assert.equal(largeKey(next, withScreen), null);
});

test('GIVEN a large feed WHEN Show all is chosen THEN the grid shows', () => {
  assert.equal(largeKey(showAll(), cameras), null);
  assert.equal(largeKey(showAll(), withScreen), null);
});

test('GIVEN a grid choice WHEN the screen share ends and another starts THEN the new share is large', () => {
  const after = keepPick(GRID_PICK, cameras);
  assert.deepEqual(after, AUTO_PICK);
  assert.equal(largeKey(after, withScreen), 'screen');
});

test('GIVEN a large feed WHEN it goes away THEN the grid shows', () => {
  assert.deepEqual(keepPick(feed('b'), ['own', 'a']), AUTO_PICK);
  assert.equal(largeKey(feed('b'), ['own', 'a']), null);
});

test('GIVEN a large camera WHEN that camera goes off THEN the pick drops and the grid shows', () => {
  const on = feedKeys([
    { key: 'own', tile: 0 },
    { key: 'a', tile: 1 }
  ]);
  assert.equal(largeKey(feed('a'), on), 'a');
  // The person stays in the call; only the picture is gone.
  const off = feedKeys([
    { key: 'own', tile: 0 },
    { key: 'a', tile: null }
  ]);
  assert.deepEqual(off, ['own']);
  assert.deepEqual(keepPick(feed('a'), off), AUTO_PICK);
  assert.equal(largeKey(feed('a'), off), null);
});

test('GIVEN a camera that is off WHEN its cell is selected THEN nothing changes', () => {
  const keys = feedKeys([
    { key: 'own', tile: null },
    { key: 'a', tile: 1 },
    { key: 'screen', tile: 5 }
  ]);
  assert.deepEqual(keys, ['a', 'screen']);
  assert.deepEqual(selectFeed(AUTO_PICK, 'own', keys), AUTO_PICK);
  assert.equal(largeKey(selectFeed(feed('a'), 'own', keys), keys), 'a');
});

test('GIVEN a large camera that went away WHEN a screen share runs THEN the screen is large', () => {
  assert.equal(largeKey(feed('b'), ['own', 'a', 'screen']), 'screen');
});

test('GIVEN the large feed is the screen WHEN the share ends THEN the grid shows', () => {
  assert.equal(largeKey(feed('screen'), cameras), null);
});

test('GIVEN a key that is not in the call WHEN it is selected THEN nothing changes', () => {
  assert.equal(largeKey(selectFeed(feed('a'), 'zzz', cameras), cameras), 'a');
  assert.equal(largeKey(selectFeed(AUTO_PICK, 'zzz', cameras), cameras), null);
});

test('GIVEN a feed button WHEN it is labelled THEN a large feed goes back to all and the others show large', () => {
  assert.equal(feedLabel(true), SHOW_ALL_LABEL);
  assert.equal(feedLabel(false), SHOW_LARGE_LABEL);
  assert.notEqual(SHOW_ALL_LABEL, SHOW_LARGE_LABEL);
});

test('GIVEN no large feed WHEN the cells are split THEN all stay in order', () => {
  const cells = cameras.map((key) => ({ key }));
  const split = splitFeeds(cells, null);
  assert.equal(split.large, null);
  assert.deepEqual(split.rest, cells);
});

test('GIVEN the device panel WHEN a change is read THEN each result has its own text', () => {
  assert.equal(deviceSwitchText(false, false, null), '');
  assert.equal(deviceSwitchText(true, true, null), DEVICE_SWITCHING_TEXT);
  assert.equal(deviceSwitchText(true, false, null), DEVICE_SAVED_TEXT);
  assert.ok(!/switched/i.test(DEVICE_SAVED_TEXT), 'no claim before the worker answers');
  for (const change of ['not_found', 'unsupported', 'kept', 'failed']) {
    assert.equal(deviceSwitchText(true, false, change), DEVICE_SWITCH_TEXT[change]);
    // A failure from before the panel opened shows too.
    assert.equal(deviceSwitchText(false, false, change), DEVICE_SWITCH_TEXT[change]);
    assert.equal(deviceSwitchText(false, true, change), DEVICE_SWITCHING_TEXT);
  }
  assert.match(DEVICE_SWITCH_TEXT.not_found, /not connected/);
  assert.match(DEVICE_SWITCH_TEXT.unsupported, /next call/);
  assert.match(DEVICE_SWITCH_TEXT.failed, /rejoin/);
});
