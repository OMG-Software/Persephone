import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DeviceTests, testLevel, testLive, testTiles } from '../src/deviceTests.ts';
import { IDLE_DEVICE_TEST } from '../src/calls.ts';

const status = (id, kind, state, stream = 0) => ({ id, test: kind, state, stream });

// A bridge whose answers the test releases by hand, and that keeps its calls.
function bridge() {
  const calls = [];
  const waiting = [];
  const answer = (name, args) =>
    new Promise((resolve, reject) => {
      calls.push([name, ...args]);
      waiting.push({ name, resolve, reject });
    });
  return {
    calls,
    next(name) {
      const index = waiting.findIndex((entry) => entry.name === name);
      assert.ok(index >= 0, `no ${name} waits`);
      return waiting.splice(index, 1)[0];
    },
    commands: {
      start: (kind, device) => answer('start', [kind, device]),
      stop: (id = null) => answer('stop', [id]),
      status: () => answer('status', [])
    }
  };
}

const turn = () => new Promise((resolve) => setImmediate(resolve));

test('GIVEN a status read that answers late WHEN a test starts meanwhile THEN the start is shown and the old read is not', async () => {
  const core = bridge();
  const shown = [];
  const tests = new DeviceTests(core.commands, (next) => shown.push(next), IDLE_DEVICE_TEST);
  void tests.read();
  const started = tests.run('microphone', 'mic-1');
  core.next('start').resolve(status(2, 'microphone', 'starting'));
  await started;
  core.next('status').resolve(status(1, 'speaker', 'done'));
  await turn();
  assert.deepEqual(shown, [status(2, 'microphone', 'starting')]);
  assert.deepEqual(core.calls[1], ['start', 'microphone', 'mic-1']);
});

test('GIVEN a microphone test that runs WHEN its button is used again THEN it stops and nothing starts', async () => {
  const core = bridge();
  const tests = new DeviceTests(core.commands, () => {}, status(3, 'microphone', 'running'));
  const stopped = tests.run('microphone', 'mic-1');
  core.next('stop').resolve();
  await stopped;
  assert.deepEqual(core.calls, [['stop', null]]);
});

test('GIVEN a start that answers after the settings close WHEN it is running THEN only that test is stopped, by its ID', async () => {
  const core = bridge();
  const shown = [];
  const tests = new DeviceTests(core.commands, (next) => shown.push(next), IDLE_DEVICE_TEST);
  const started = tests.run('camera', null);
  tests.close();
  core.next('start').resolve(status(7, 'camera', 'starting'));
  await started;
  assert.deepEqual(core.calls, [['start', 'camera', null], ['stop', 7]]);
  assert.deepEqual(shown, [], 'a closed page shows nothing');

  // A start that already failed holds no device: nothing to stop.
  const late = new DeviceTests(core.commands, () => {}, IDLE_DEVICE_TEST);
  const failed = late.run('speaker', null);
  late.close();
  core.next('start').resolve(status(8, 'speaker', 'failed'));
  await failed;
  assert.equal(core.calls.length, 3);
});

test('GIVEN a test that runs WHEN the settings close THEN it is stopped by its ID; an ended test is not', () => {
  const core = bridge();
  new DeviceTests(core.commands, () => {}, status(4, 'speaker', 'running')).close();
  new DeviceTests(core.commands, () => {}, status(5, 'speaker', 'done')).close();
  assert.deepEqual(core.calls, [['stop', 4]]);
});

test('GIVEN a refused start WHEN the test runs THEN the reason reaches the page and the status stays', async () => {
  const core = bridge();
  const shown = [];
  const tests = new DeviceTests(core.commands, (next) => shown.push(next), IDLE_DEVICE_TEST);
  const started = tests.run('microphone', null);
  core.next('start').reject('limit_exceeded');
  await assert.rejects(started, (reason) => reason === 'limit_exceeded');
  assert.deepEqual(shown, []);
});

test('GIVEN each test state WHEN the meter and preview are made THEN they show only for the test that runs', () => {
  assert.equal(testLevel(status(1, 'microphone', 'running'), 70), 70);
  assert.equal(testLevel(status(1, 'microphone', 'starting'), 70), 70);
  // The call's own level is no test level.
  assert.equal(testLevel(status(1, 'speaker', 'running'), 70), 0);
  assert.equal(testLevel(status(1, 'microphone', 'done'), 70), 0);
  assert.equal(testLevel(IDLE_DEVICE_TEST, 70), 0);

  assert.deepEqual(testTiles(status(2, 'camera', 'running', 9)), [{ tile: 0, participant: null, own: true, stream: 9 }]);
  assert.deepEqual(testTiles(status(2, 'camera', 'starting', 9)), []);
  assert.deepEqual(testTiles(status(2, 'microphone', 'running', 9)), []);

  assert.equal(testLive(status(1, 'speaker', 'starting')), true);
  assert.equal(testLive(status(1, 'speaker', 'unavailable')), false);
});
