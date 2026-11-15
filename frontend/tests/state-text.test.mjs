import assert from 'node:assert/strict';
import { test } from 'node:test';
import { EVENT_SWITCHES, changeShown, eventSwitch, stateLine, stateParts } from '../src/stateText.ts';

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

test('GIVEN names from the event WHEN split into parts THEN only they are person or name parts', () => {
  const hostile = '\u202Egnp.exe <b>';
  const parts = stateParts(hostile, { type: 'member', target: hostile, membership: 'kicked' });
  assert.deepEqual(parts, [{ person: hostile }, { text: ' removed ' }, { person: hostile }]);
  const topic = stateParts(A, { type: 'topic', topic: hostile });
  assert.deepEqual(topic[2], { name: hostile });
  assert.ok(topic.filter((part) => 'text' in part).every((part) => !part.text.includes(hostile)));
});

test('GIVEN a change this build does not know WHEN described THEN a neutral line shows', () => {
  assert.equal(stateLine(A, { type: 'future' }), `${A} changed the room`);
});

test('GIVEN a profile picture, pins and guest access WHEN described THEN each has its own line', () => {
  assert.equal(stateLine(A, { type: 'profile_picture', target: B }), `${B} changed their profile picture`);
  assert.equal(stateLine(A, { type: 'pinned_messages' }), `${A} changed the pinned messages`);
  assert.equal(stateLine(A, { type: 'guest_access', allowed: true }), `${A} let guests join`);
  assert.equal(stateLine(A, { type: 'guest_access', allowed: false }), `${A} stopped guests from joining`);
});

test('GIVEN display names WHEN a line is made THEN people read as name and ID, and a renamed person as their ID', () => {
  const names = new Map([
    [A, 'Alice'],
    [B, 'Bobby']
  ]);
  assert.equal(stateLine(A, member('invited'), names), `Alice (${A}) invited Bobby (${B})`);
  assert.equal(
    stateLine(B, { type: 'display_name', target: B, name: 'Bobby' }, names),
    `${B} is now shown as Bobby`,
    'the current name would read as the new one'
  );
  const parts = stateParts(A, member('kicked'));
  assert.deepEqual(parts, [{ person: A }, { text: ' removed ' }, { person: B }]);
  assert.deepEqual(stateParts(A, { type: 'topic', topic: B }), [{ person: A }, { text: ' changed the topic to ' }, { name: B }]);
});

test('GIVEN every kind of change WHEN its switch is found THEN each kind has one listed switch, and every switch is listed once', () => {
  const memberships = [
    ['joined', 'joins'],
    ['left', 'leaves'],
    ['invited', 'invitations'],
    ['invite_rejected', 'invitations'],
    ['invite_withdrawn', 'invitations'],
    ['kicked', 'removals'],
    ['banned', 'removals'],
    ['unbanned', 'removals'],
    ['knocked', 'joinRequests'],
    ['knock_withdrawn', 'joinRequests'],
    ['knock_denied', 'joinRequests']
  ];
  for (const [membership, key] of memberships) assert.equal(eventSwitch(member(membership)), key, membership);
  const others = [
    [{ type: 'display_name', target: B, name: null }, 'displayNames'],
    [{ type: 'profile_picture', target: B }, 'profilePictures'],
    [{ type: 'name', name: null }, 'roomName'],
    [{ type: 'topic', topic: null }, 'topic'],
    [{ type: 'avatar' }, 'roomPicture'],
    [{ type: 'encryption' }, 'encryption'],
    [{ type: 'join_rule', rule: 'invite' }, 'joinRule'],
    [{ type: 'history_visibility', visibility: 'shared' }, 'historyVisibility'],
    [{ type: 'power_levels' }, 'permissions'],
    [{ type: 'addresses' }, 'addresses'],
    [{ type: 'pinned_messages' }, 'pinnedMessages'],
    [{ type: 'guest_access', allowed: true }, 'guestAccess'],
    [{ type: 'server_acl' }, 'serverAccess'],
    [{ type: 'created' }, 'creation'],
    [{ type: 'upgraded' }, 'upgrades']
  ];
  for (const [change, key] of others) assert.equal(eventSwitch(change), key, change.type);
  const used = new Set([...memberships.map(([, key]) => key), ...others.map(([, key]) => key)]);
  const listed = EVENT_SWITCHES.map((entry) => entry.key);
  assert.deepEqual([...used].sort(), [...listed].sort());
  assert.equal(new Set(listed).size, listed.length);
  assert.equal(eventSwitch({ type: 'future_kind' }), null);
});

test('GIVEN switches WHEN a change is checked THEN only a switch set to false hides it', () => {
  const all = Object.fromEntries(EVENT_SWITCHES.map(({ key }) => [key, true]));
  assert.equal(changeShown(member('left'), all), true);
  assert.equal(changeShown(member('left'), { ...all, leaves: false }), false);
  assert.equal(changeShown(member('joined'), { ...all, leaves: false }), true);
  // A change no switch names, or a switch an older page does not have, shows.
  assert.equal(changeShown({ type: 'future_kind' }, { ...all, leaves: false }), true);
  assert.equal(changeShown(member('left'), {}), true);
});
