import assert from 'node:assert/strict';
import { test } from 'node:test';
import { GainSender } from '../src/callGain.ts';

const deferred = () => {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
};
const flush = () => new Promise((resolve) => setImmediate(resolve));

const setup = () => {
  const sent = [];
  const waits = [];
  const replies = [];
  const settles = [];
  const sender = new GainSender(
    (gain) => {
      sent.push(gain);
      const wait = deferred();
      waits.push(wait);
      return wait.promise;
    },
    (reply, newest) => replies.push([reply, newest]),
    (failed) => settles.push(failed)
  );
  return { sender, sent, waits, replies, settles };
};

test('GIVEN a send that waits WHEN 150, 250 and 300 are chosen THEN only 150 and 300 are sent, in that order', async () => {
  const { sender, sent, waits, replies, settles } = setup();
  sender.set(150);
  sender.set(250);
  sender.set(300);
  assert.deepEqual(sent, [150]);
  waits[0].resolve('r150');
  await flush();
  assert.deepEqual(sent, [150, 300]);
  waits[1].resolve('r300');
  await flush();
  assert.deepEqual(sent, [150, 300]);
  assert.deepEqual(replies, [
    ['r150', false],
    ['r300', true]
  ]);
  assert.deepEqual(settles, [false]);
});

test('GIVEN a failed newest send WHEN it ends THEN settled reports the failure', async () => {
  const { sender, waits, settles } = setup();
  sender.set(120);
  waits[0].reject(new Error('refused'));
  await flush();
  assert.deepEqual(settles, [true]);
});

test('GIVEN a failed older send WHEN a newer value was chosen THEN the newer send runs and settled reports no failure', async () => {
  const { sender, sent, waits, settles } = setup();
  sender.set(120);
  sender.set(180);
  waits[0].reject(new Error('refused'));
  await flush();
  assert.deepEqual(sent, [120, 180]);
  waits[1].resolve('ok');
  await flush();
  assert.deepEqual(settles, [false]);
});
