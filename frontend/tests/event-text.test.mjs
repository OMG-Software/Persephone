import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DEFAULT_SETTINGS } from '../src/preferences.ts';
import { OTHER_SWITCHES, itemShown, otherParts, otherSwitch } from '../src/eventText.ts';
import { EVENT_SWITCHES } from '../src/stateText.ts';
import { applyRelations, quoteOf } from '../src/timeline.ts';

const A = '@alice:example.invalid';
const line = (parts) =>
  parts.map((part) => ('person' in part ? part.person : 'name' in part ? `[${part.name}]` : part.text)).join('');

test('GIVEN other events with fixed text WHEN described THEN the line names the sender and never the type', () => {
  const cases = [
    ['verification_request', `${A} sent a verification request`],
    ['verification_done', `${A} finished verification`],
    ['call_started', `${A} started a call`],
    ['call_left', `${A} left the call`],
    ['poll_vote', `${A} voted in a poll`],
    ['sticker', `${A} sent a sticker`],
    ['third_party_invite', `${A} invited someone by email or phone`]
  ];
  for (const [event, expected] of cases) {
    assert.equal(line(otherParts(A, event, 'm.secret.type')), expected, event);
  }
});

test('GIVEN other events without fixed text WHEN described THEN the type is its own name piece', () => {
  assert.deepEqual(otherParts(A, 'event', '<b>x</b>'), [
    { person: A },
    { text: ' sent an event of type ' },
    { name: '<b>x</b>' }
  ]);
  assert.equal(line(otherParts(A, 'message', 'org.x')), `${A} sent a message of type [org.x]`);
  assert.equal(line(otherParts(A, 'state', 'org.y')), `${A} changed room state of type [org.y]`);
  // A kind this build does not know is named by its type.
  assert.equal(line(otherParts(A, 'teleport', 'org.z')), `${A} sent an event of type [org.z]`);
  assert.equal(line(otherParts(A, 'toString', 'org.z')), `${A} sent an event of type [org.z]`);
});

test('GIVEN each other kind WHEN its switch is read THEN it is a listed switch, and an unknown kind is other events', () => {
  const listed = new Set([...EVENT_SWITCHES, ...OTHER_SWITCHES].map((entry) => entry.key));
  for (const event of ['verification_step', 'call_joined', 'call_signalling', 'poll_ended', 'location', 'space_parent', 'widget', 'state']) {
    assert.ok(listed.has(otherSwitch(event)), event);
  }
  assert.equal(otherSwitch('third_party_invite'), 'invitations');
  assert.equal(otherSwitch('call_signalling'), 'callSignalling');
  assert.equal(otherSwitch('teleport'), 'otherEvents');
  assert.equal(otherSwitch('__proto__'), 'otherEvents');
  // Every switch the settings have is offered once.
  const keys = [...EVENT_SWITCHES, ...OTHER_SWITCHES].map((entry) => entry.key);
  assert.deepEqual([...keys].sort(), Object.keys(DEFAULT_SETTINGS.timelineEvents).sort());
});

test('GIVEN switches off WHEN items are filtered THEN only those kinds go, and encryption notices always show', () => {
  const events = { ...DEFAULT_SETTINGS.timelineEvents, verification: false, deletedMessages: false, joins: false };
  const other = (event) => ({ kind: 'other', event_id: '$o', sender: A, origin_server_ts: null, event, event_type: 't', state: false });
  const notice = (kind) => ({ kind: 'notice', event_id: '$n', sender: A, notice: kind, origin_server_ts: null });
  assert.equal(itemShown(other('verification_ready'), events), false);
  assert.equal(itemShown(other('call_started'), events), true);
  assert.equal(itemShown(notice('deleted'), events), false);
  for (const kind of ['undecryptable', 'withheld', 'untrusted_sender']) {
    assert.equal(itemShown(notice(kind), events), true, kind);
  }
  const join = { kind: 'state', event_id: '$s', sender: A, origin_server_ts: null, change: { type: 'member', target: A, membership: 'joined' } };
  assert.equal(itemShown(join, events), false);
  assert.equal(itemShown(join, DEFAULT_SETTINGS.timelineEvents), true);
  assert.equal(itemShown({ kind: 'message', event_id: '$m', sender: A, body: 'hi', origin_server_ts: null, reply_to: null }, events), true);
});

test('GIVEN deletions of other events WHEN applied THEN a message-like event shows as deleted and a state event keeps its line', () => {
  const other = (id, event, state) => ({ kind: 'other', event_id: id, sender: A, origin_server_ts: 5, event, event_type: 't', state });
  const redaction = (redacts) => ({ kind: 'redaction', event_id: `$r${redacts}`, sender: A, origin_server_ts: 6, redacts });
  // The same kind as a message (m.beacon) and as state (m.beacon_info):
  // the core's state flag decides, not the kind.
  const shown = applyRelations([
    other('$poll', 'poll_started', false),
    other('$space', 'space_child', true),
    other('$beacon', 'location', false),
    other('$beaconInfo', 'location', true),
    redaction('$poll'),
    redaction('$space'),
    redaction('$beacon'),
    redaction('$beaconInfo')
  ]);
  assert.deepEqual(
    shown.map((item) => [item.event_id, item.kind]),
    [['$poll', 'notice'], ['$space', 'other'], ['$beacon', 'notice'], ['$beaconInfo', 'other']]
  );
  assert.equal(shown[0].notice, 'deleted');
  assert.deepEqual(quoteOf(shown, '$space'), { sender: A, text: 'An event that is not a message' });
});
