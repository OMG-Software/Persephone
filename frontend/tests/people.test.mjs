import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as people from '../src/people.ts';

test('GIVEN a Matrix user ID WHEN shown as an avatar THEN it gets one or two letters', () => {
  const { initials, localpart } = people;
  assert.equal(localpart('@alex.moreno:persephone.chat'), 'alex.moreno');
  assert.equal(localpart('not an id'), 'not an id');
  assert.equal(initials('@alex.moreno:persephone.chat'), 'AM');
  assert.equal(initials('@mira:matrix.org'), 'M');
  assert.equal(initials('@jonas_k_weber:example.invalid'), 'JW');
  assert.equal(initials('@élodie:example.invalid'), 'É');
});

test('GIVEN hostile characters in a user ID WHEN shown as an avatar THEN none reach it', () => {
  const { initials } = people;
  // Bidi overrides, zero-width and markup characters are skipped.
  assert.equal(initials('@‮evil:example.invalid'), 'E');
  assert.equal(initials('@​<img>:example.invalid'), 'I');
  assert.equal(initials('@<>:example.invalid'), '?');
});

test('avatar colours are stable and one of six', () => {
  const { avatarColour } = people;
  const ids = ['@a:x', '@b:x', '@alex:persephone.chat', '@mira:matrix.org', ''];
  for (const id of ids) {
    assert.match(avatarColour(id), /^c[0-5]$/);
    assert.equal(avatarColour(id), avatarColour(id));
  }
});

test('isolate wraps text in first-strong isolate marks', () => {
  assert.equal(people.isolate('‮abc'), '⁨‮abc⁩');
});

test('GIVEN a person typed but not added WHEN a room is created THEN they are invited too', () => {
  const { inviteesWith } = people;
  assert.deepEqual(inviteesWith(['@a:s'], ' @b:s '), { invite: ['@a:s', '@b:s'] });
  assert.deepEqual(inviteesWith(['@a:s'], ''), { invite: ['@a:s'] }, 'nothing typed');
  assert.deepEqual(inviteesWith(['@a:s'], '@a:s'), { invite: ['@a:s'] }, 'already listed');
});

test('GIVEN typed text that cannot be invited WHEN a room is created THEN it is refused with the reason', () => {
  const { inviteesWith, MAX_INVITEES } = people;
  assert.deepEqual(inviteesWith([], 'bob'), { problem: 'not_a_matrix_id' });
  assert.deepEqual(inviteesWith([], '@bob'), { problem: 'not_a_matrix_id' });
  assert.deepEqual(inviteesWith([], '@' + 'b'.repeat(255) + ':s'), { problem: 'not_a_matrix_id' });
  const full = Array.from({ length: MAX_INVITEES }, (_, n) => `@u${n}:s`);
  assert.deepEqual(inviteesWith(full, '@extra:s'), { problem: 'too_many' });
});
