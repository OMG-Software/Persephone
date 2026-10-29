import assert from 'node:assert/strict';
import { test } from 'node:test';
import { TYPING_IDLE_MS, TYPING_RESEND_MS, TypingNotifier } from '../src/typing.ts';

function clock() {
  let now = 0;
  let next = 1;
  const timers = new Map();
  return {
    now: () => now,
    schedule: (run, ms) => {
      const id = next++;
      timers.set(id, { run, at: now + ms });
      return id;
    },
    cancel: (id) => timers.delete(id),
    advance(ms) {
      now += ms;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= now) {
          timers.delete(id);
          timer.run();
        }
      }
    }
  };
}

test('GIVEN steady typing WHEN keys arrive THEN typing is sent once, again after the resend time, then stopped when idle', () => {
  const sent = [];
  const time = clock();
  const notifier = new TypingNotifier((typing) => sent.push(typing), time.now, time.schedule, time.cancel);
  notifier.input();
  time.advance(1000);
  notifier.input();
  assert.deepEqual(sent, [true]);
  time.advance(TYPING_RESEND_MS);
  notifier.input();
  assert.deepEqual(sent, [true, true]);
  time.advance(TYPING_IDLE_MS);
  assert.deepEqual(sent, [true, true, false]);
  time.advance(TYPING_IDLE_MS);
  assert.deepEqual(sent, [true, true, false], 'nothing more once stopped');
});

test('GIVEN a sent message WHEN typing stops THEN stopped is sent once, and only after typing', () => {
  const sent = [];
  const time = clock();
  const notifier = new TypingNotifier((typing) => sent.push(typing), time.now, time.schedule, time.cancel);
  notifier.stop();
  assert.deepEqual(sent, [], 'no stop without a start');
  notifier.input();
  notifier.stop();
  notifier.stop();
  time.advance(TYPING_IDLE_MS);
  assert.deepEqual(sent, [true, false]);
});
