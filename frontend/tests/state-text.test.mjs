import assert from 'node:assert/strict';
import { test } from 'node:test';
import { stateLine, stateParts } from '../src/stateText.ts';

const A = '@alice:example.invalid';
const B = '@bob:example.invalid';
const member = (membership) => ({ type: 'member', target: B, membership });

test('GIVEN each membership change WHEN described THEN the line says who did what to whom', () => {
  const cases = [
    ['joined', `${B} joined`],
    ['left', `${B} left`],
    ['invited', `${A} invited ${B}`],
    ['invite_rejected', `${B} declined the invitation`],
    ['invite_withdrawn', `${A} withdrew the invitation for ${B}`],
    ['kicked', `${A} removed ${B}`],
    ['banned', `${A} banned ${B}`],
    ['unbanned', `${A} unbanned ${B}`],
    ['knocked', `${B} asked to join`],
    ['knock_withdrawn', `${B} withdrew their request to join`],
    ['knock_denied', `${A} refused the request to join from ${B}`]
  ];
  for (const [membership, line] of cases) {
    assert.equal(stateLine(A, member(membership)), line, membership);
  }
});

test('GIVEN room changes WHEN described THEN set and removed values read differently', () => {
  assert.equal(stateLine(A, { type: 'name', name: 'Plans' }), `${A} renamed the room to Plans`);
  assert.equal(stateLine(A, { type: 'name', name: null }), `${A} removed the room name`);
  assert.equal(stateLine(A, { type: 'topic', topic: 'Q4' }), `${A} changed the topic to Q4`);
  assert.equal(stateLine(A, { type: 'topic', topic: null }), `${A} removed the topic`);
  assert.equal(
    stateLine(A, { type: 'display_name', target: B, name: 'Bobby' }),
    `${B} is now shown as Bobby`
  );
  assert.equal(
    stateLine(A, { type: 'display_name', target: B, name: null }),
    `${B} removed their display name`
  );
  assert.equal(stateLine(A, { type: 'encryption' }), `${A} turned on end-to-end encryption`);
  assert.equal(
    stateLine(A, { type: 'join_rule', rule: 'public' }),
    `${A} changed who can join: Anyone who finds the room`
  );
  assert.equal(
    stateLine(A, { type: 'history_visibility', visibility: 'world_readable' }),
    `${A} changed who can read the history: Anyone, members or not`
  );
  for (const type of ['avatar', 'power_levels', 'addresses', 'created', 'server_acl', 'upgraded']) {
    assert.match(stateLine(A, { type }), new RegExp(`^${A} \\S`), type);
  }
});

test('GIVEN names from the event WHEN split into parts THEN only they are name parts', () => {
  const hostile = '‮gnp.exe <b>';
  const parts = stateParts(hostile, { type: 'member', target: hostile, membership: 'kicked' });
  assert.deepEqual(parts, [{ name: hostile }, { text: ' removed ' }, { name: hostile }]);
  const topic = stateParts(A, { type: 'topic', topic: hostile });
  assert.deepEqual(topic[2], { name: hostile });
  assert.ok(topic.filter((part) => 'text' in part).every((part) => !part.text.includes(hostile)));
});

test('GIVEN a change this build does not know WHEN described THEN a neutral line shows', () => {
  assert.equal(stateLine(A, { type: 'future' }), `${A} changed the room`);
});
