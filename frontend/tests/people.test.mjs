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

const page = (names) => ({ names });

test('GIVEN pages of the newest messages WHEN remembered THEN a newer name replaces an older one and only the newest are kept', () => {
  const { withNames } = people;
  const first = withNames(new Map(), page([
    { userId: '@a:s', displayName: 'Ann' },
    { userId: '@b:s', displayName: 'Ben' }
  ]), true);
  const second = withNames(first, page([{ userId: '@a:s', displayName: 'Annie' }]), true);
  assert.deepEqual([...second], [
    ['@b:s', 'Ben'],
    ['@a:s', 'Annie']
  ]);
  assert.equal(first.get('@a:s'), 'Ann', 'the known names are not changed in place');
  const bounded = withNames(second, page([{ userId: '@c:s', displayName: 'Cy' }]), true, 2);
  assert.deepEqual([...bounded.keys()], ['@a:s', '@c:s'], 'the longest-known name goes first');
});

test('GIVEN a newest page that lists a person without a display name WHEN remembered THEN their old name is gone and others stay', () => {
  const known = new Map([
    ['@a:s', 'Ann'],
    ['@b:s', 'Ben']
  ]);
  const names = people.withNames(known, page([{ userId: '@a:s', displayName: null }]), true);
  assert.equal(people.shownName('@a:s', names), null);
  assert.equal(people.shownName('@b:s', names), 'Ben', 'Ben is not listed, so the page says nothing about him');
});

test('GIVEN a newest page that does not list a person, as when their member lookup failed WHEN remembered THEN their known name stays', () => {
  const known = new Map([['@a:s', 'Ann']]);
  // The core could not find @a:s's member event: their name is unknown, not removed.
  const names = people.withNames(known, page([{ userId: '@b:s', displayName: 'Ben' }]), true);
  assert.equal(people.shownName('@a:s', names), 'Ann');
  // A person never known and not listed is still not known, so an older page may name them.
  const older = people.withNames(names, page([{ userId: '@c:s', displayName: 'Cy' }]), false);
  assert.equal(people.shownName('@c:s', older), 'Cy');
});

test('GIVEN a name a newest page removed WHEN an older page still has it THEN it does not come back', () => {
  const cleared = people.withNames(new Map([['@a:s', 'Ann']]), page([{ userId: '@a:s', displayName: null }]), true);
  const older = people.withNames(cleared, page([{ userId: '@a:s', displayName: 'Ann' }]), false);
  assert.equal(people.shownName('@a:s', older), null);
  // A later newest page that names them again is believed.
  const renamed = people.withNames(older, page([{ userId: '@a:s', displayName: 'Annie' }]), true);
  assert.equal(people.shownName('@a:s', renamed), 'Annie');
});

test('GIVEN an older page WHEN remembered THEN it only adds names of people not yet known, and never removes one', () => {
  const known = new Map([['@a:s', 'Annie']]);
  const names = people.withNames(known, page([
    { userId: '@a:s', displayName: 'Ann' },
    { userId: '@b:s', displayName: 'Ben' },
    { userId: '@c:s', displayName: null }
  ]), false);
  assert.deepEqual([...names], [
    ['@a:s', 'Annie'],
    ['@b:s', 'Ben']
  ]);
  const removed = people.withNames(known, page([{ userId: '@a:s', displayName: null }]), false);
  assert.equal(people.shownName('@a:s', removed), 'Annie', 'an old page without the name says nothing about now');
});

test('GIVEN known names WHEN a person is shown THEN the name comes with the ID, and a missing or ID-only name gives the ID once', () => {
  const { personText, shownName } = people;
  const names = new Map([
    ['@a:s', 'Ann'],
    ['@id:s', '@id:s']
  ]);
  assert.equal(shownName('@a:s', names), 'Ann');
  assert.equal(shownName('@id:s', names), null);
  assert.equal(shownName('@none:s', names), null);
  assert.equal(personText('@a:s', names), 'Ann (@a:s)');
  assert.equal(personText('@none:s', names), '@none:s');
});
